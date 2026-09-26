//! The host side of a job: the structured fills, the Merkle layer caches, the seed chain, the
//! per-attempt nonce patch, the canary tile and the PlainProof of a hit.
//!
//! Nothing here holds a matrix: A and Bᵀ are SplitMix64 streams of the work unit's fill seed
//! (`spm_cpuref::fill_int7`, the same generator the GPU runs), regenerated row by row or chunk by
//! chunk whenever a tile, a proof or a Merkle segment needs them. What is kept per job is two
//! [`MatrixTree`]s (~0.5 MiB of chaining values each at 131072 × 4096) and the B permutation.

use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use pearl_blake3::MerkleTree;
use spm_cpuref::{
    fill_int7_at, fill_int7_bytes_at, seed_chain, tile_from_rows, ChunkPatch, ChunkSource, Hash256,
    MatrixTree, NoiseSide, TileResult, CHUNK_BYTES, DOMAIN_A, DOMAIN_BT, SEED_LABEL_A,
    SEED_LABEL_B,
};
use spm_pow::{
    bind_root_a, bind_root_b, IncompleteBlockHeader, MatrixMerkleProof, MiningConfiguration,
    PlainProof, NOISE_RANK,
};
use spm_work::{Shape, WorkUnit, SUPPORTED_CERT_VERSION};

use crate::engine::JobSpec;

/// A-matrix entries the nonce occupies at the start of chunk 0 (16 nibbles of a u64).
pub const NONCE_ENTRIES: usize = 16;

/// What identifies a job's host and device state: the commitment key and the shape. Two work
/// units with the same key (same header, e.g. a new share target) reuse everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub job_key: Hash256,
    pub shape: Shape,
}

/// The 64-bit generator seed of a work unit's fill seed (its first 8 bytes, little endian).
pub fn gen_seed(fill_seed: &[u8; 32]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&fill_seed[..8]);
    u64::from_le_bytes(b)
}

/// Chunk 0 of A with the nonce written into its first 16 entries, one nibble each as
/// `nibble − 8` ∈ [-8, 7] (inside the committed range). The rest of the chunk is A's own.
pub fn nonce_chunk(base: &[u8; CHUNK_BYTES], nonce: u64) -> [u8; CHUNK_BYTES] {
    let mut out = *base;
    for (i, b) in out[..NONCE_ENTRIES].iter_mut().enumerate() {
        *b = (((nonce >> (4 * i)) & 0xf) as i8 - 8) as u8;
    }
    out
}

struct Fill {
    seed: u64,
    domain: u64,
}

impl ChunkSource for Fill {
    fn fill(&self, first_chunk: usize, out: &mut [u8]) {
        fill_int7_bytes_at(self.seed, self.domain, first_chunk * CHUNK_BYTES, out);
    }
}

/// One attempt's A side: the patched chunk 0, the new root and the derived seeds.
#[derive(Debug, Clone)]
pub struct AttemptHost {
    pub nonce: u64,
    patch: ChunkPatch,
    pub root_a: Hash256,
    pub bound_a: Hash256,
    pub a_noise_seed: Hash256,
}

impl AttemptHost {
    /// The patched chunk 0 as A entries (the GPU prefix override).
    pub fn prefix(&self) -> Vec<i8> {
        self.patch.data().iter().map(|&b| b as i8).collect()
    }
}

/// The host state of one job.
pub struct JobHost {
    key: JobKey,
    m: usize,
    n: usize,
    k: usize,
    rank: usize,
    gen_seed: u64,
    header: IncompleteBlockHeader,
    config: MiningConfiguration,
    fill_a: Fill,
    fill_b: Fill,
    tree_a: MatrixTree,
    tree_b: MatrixTree,
    chunk0: [u8; CHUNK_BYTES],
    root_b: Hash256,
    bound_b: Hash256,
    b_noise_seed: Hash256,
    b_side: NoiseSide,
    row_pattern: Vec<u32>,
    col_pattern: Vec<u32>,
}

impl std::fmt::Debug for JobHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHost")
            .field("shape", &self.key.shape)
            .field("job_key", &hex_short(&self.key.job_key))
            .field("root_b", &hex_short(&self.root_b))
            .finish()
    }
}

fn hex_short(h: &Hash256) -> String {
    h[..6].iter().map(|b| format!("{b:02x}")).collect()
}

impl JobHost {
    /// Checks the work unit, then builds both Merkle layer caches (in parallel) and the B-side
    /// seeds. At 131072 × 4096 this hashes 1 GiB of regenerated fill on all cores.
    pub fn new(wu: &WorkUnit) -> Result<Self> {
        ensure!(
            wu.cert_version == SUPPORTED_CERT_VERSION,
            "work unit has cert_version {}, this worker mines only {SUPPORTED_CERT_VERSION}",
            wu.cert_version
        );
        wu.shape.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(
            wu.shape.r == NOISE_RANK,
            "noise rank {} is not {NOISE_RANK}",
            wu.shape.r
        );
        let header = wu.block_header().map_err(|e| anyhow::anyhow!("{e}"))?;
        let config = wu.mining_config().map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(
            config.common_dim == wu.shape.k,
            "config k {} differs from the shape's {}",
            config.common_dim,
            wu.shape.k
        );
        let job_key = spm_cpuref::job_key(&header, &config);
        ensure!(
            job_key == wu.job_key,
            "work unit job_key does not match blake3(header ‖ config)"
        );
        let (m, n, k) = (
            wu.shape.m as usize,
            wu.shape.n as usize,
            wu.shape.k as usize,
        );
        let rank = usize::from(wu.shape.r);
        let seed = gen_seed(&wu.fill_seed);
        let fill_a = Fill {
            seed,
            domain: DOMAIN_A,
        };
        let fill_b = Fill {
            seed,
            domain: DOMAIN_BT,
        };
        // m, n, k are multiples of 64 with k >= 1024, so both matrices are whole chunks.
        let (chunks_a, chunks_b) = (m * k / CHUNK_BYTES, n * k / CHUNK_BYTES);
        let (tree_a, tree_b) = rayon::join(
            || MatrixTree::build(job_key, chunks_a, &fill_a),
            || MatrixTree::build(job_key, chunks_b, &fill_b),
        );
        let (tree_a, tree_b) = (tree_a.context("A tree")?, tree_b.context("Bᵀ tree")?);
        let mut chunk0 = [0u8; CHUNK_BYTES];
        fill_a.fill(0, &mut chunk0);
        let root_b = tree_b.root(None);
        let bound_b = bind_root_b(&root_b, wu.shape.n);
        // The B seed does not depend on A: take it from the chain with any A binding.
        let (b_noise_seed, _) = seed_chain(&job_key, &[0; 32], &bound_b);
        let b_side = NoiseSide::new(&SEED_LABEL_B, &b_noise_seed, k, rank);
        let row_pattern = config.rows_pattern.to_list();
        let col_pattern = config.cols_pattern.to_list();
        Ok(Self {
            key: JobKey {
                job_key,
                shape: wu.shape,
            },
            m,
            n,
            k,
            rank,
            gen_seed: seed,
            header,
            config,
            fill_a,
            fill_b,
            tree_a,
            tree_b,
            chunk0,
            root_b,
            bound_b,
            b_noise_seed,
            b_side,
            row_pattern,
            col_pattern,
        })
    }

    pub fn key(&self) -> JobKey {
        self.key
    }

    pub fn header(&self) -> &IncompleteBlockHeader {
        &self.header
    }

    pub fn config(&self) -> &MiningConfiguration {
        &self.config
    }

    pub fn gen_seed(&self) -> u64 {
        self.gen_seed
    }

    pub fn root_b(&self) -> Hash256 {
        self.root_b
    }

    pub fn b_noise_seed(&self) -> Hash256 {
        self.b_noise_seed
    }

    /// The device job for this host job.
    pub fn spec(&self) -> JobSpec {
        JobSpec {
            m: self.key.shape.m,
            n: self.key.shape.n,
            k: self.key.shape.k,
            gen_seed: self.gen_seed,
            b_noise_seed: self.b_noise_seed,
            dump: false,
            hit_capacity: 0,
        }
    }

    /// Hash tiles per attempt.
    pub fn tiles(&self) -> u64 {
        (self.m * self.n / 128) as u64
    }

    /// Patches `nonce` into chunk 0 and derives the attempt's root and seeds: one leaf hash and
    /// one merge per tree level, then two salted-root and two seed-chain hashes.
    pub fn attempt(&self, nonce: u64) -> AttemptHost {
        let patch = self.tree_a.patch_chunk0(&nonce_chunk(&self.chunk0, nonce));
        let root_a = patch.root();
        let bound_a = bind_root_a(&root_a, self.key.shape.m);
        let (b, a_noise_seed) = seed_chain(&self.key.job_key, &bound_a, &self.bound_b);
        debug_assert_eq!(b, self.b_noise_seed);
        AttemptHost {
            nonce,
            patch,
            root_a,
            bound_a,
            a_noise_seed,
        }
    }

    /// Rows of the attempt's A (k entries each, concatenated), chunk 0 patched.
    pub fn a_rows(&self, att: &AttemptHost, rows: &[usize]) -> Vec<i8> {
        let mut out = vec![0i8; rows.len() * self.k];
        for (dst, &r) in out.chunks_exact_mut(self.k).zip(rows) {
            fill_int7_at(self.gen_seed, DOMAIN_A, r * self.k, dst);
            // Entries [0, 1024) are chunk 0.
            let start = r * self.k;
            if start < CHUNK_BYTES {
                let end = (start + self.k).min(CHUNK_BYTES);
                for (d, &s) in dst.iter_mut().zip(&att.patch.data()[start..end]) {
                    *d = s as i8;
                }
            }
        }
        out
    }

    /// Rows of Bᵀ.
    pub fn bt_rows(&self, rows: &[usize]) -> Vec<i8> {
        let mut out = vec![0i8; rows.len() * self.k];
        for (dst, &r) in out.chunks_exact_mut(self.k).zip(rows) {
            fill_int7_at(self.gen_seed, DOMAIN_BT, r * self.k, dst);
        }
        out
    }

    fn check_tile(&self, t_rows: u32, t_cols: u32) -> Result<(Vec<usize>, Vec<usize>)> {
        ensure!(
            (t_rows as usize) < self.m
                && (t_cols as usize) < self.n
                && self.config.rows_pattern.offset_is_valid(t_rows)
                && self.config.cols_pattern.offset_is_valid(t_cols),
            "({t_rows}, {t_cols}) is not a hash-tile base"
        );
        let rows = self
            .row_pattern
            .iter()
            .map(|&d| (t_rows + d) as usize)
            .collect();
        let cols = self
            .col_pattern
            .iter()
            .map(|&d| (t_cols + d) as usize)
            .collect();
        Ok((rows, cols))
    }

    /// The CPU recomputation of one tile of the attempt (the canary): its 8 A' rows and 16 B'ᵀ
    /// rows noised with the official generators for those indices, then the transcript and the
    /// keyed digest exactly as the kernel does them.
    pub fn tile(&self, att: &AttemptHost, t_rows: u32, t_cols: u32) -> Result<TileResult> {
        let (rows, cols) = self.check_tile(t_rows, t_cols)?;
        let a_side = NoiseSide::new(&SEED_LABEL_A, &att.a_noise_seed, self.k, self.rank);
        let a = a_side.noised_rows(&self.a_rows(att, &rows), &rows)?;
        let bt = self.b_side.noised_rows(&self.bt_rows(&cols), &cols)?;
        tile_from_rows(
            t_rows,
            t_cols,
            &a,
            &bt,
            self.k,
            self.rank,
            &att.a_noise_seed,
        )
    }

    /// PlainProof of tile `(t_rows, t_cols)` of the attempt: the 8 A rows (chunk 0 patched) and
    /// 16 Bᵀ rows with their multileaf proofs from the layer caches.
    pub fn proof(&self, att: &AttemptHost, t_rows: u32, t_cols: u32) -> Result<PlainProof> {
        let (rows, cols) = self.check_tile(t_rows, t_cols)?;
        let leaves_a = MerkleTree::compute_leaf_indices_from_rows(&rows, (self.m, self.k));
        let leaves_b = MerkleTree::compute_leaf_indices_from_rows(&cols, (self.n, self.k));
        let a = self
            .tree_a
            .multileaf_proof(&leaves_a, Some(&att.patch), &self.fill_a)?;
        let bt = self.tree_b.multileaf_proof(&leaves_b, None, &self.fill_b)?;
        Ok(PlainProof {
            m: self.m,
            n: self.n,
            k: self.k,
            noise_rank: self.rank,
            a: MatrixMerkleProof {
                proof: a,
                row_indices: rows,
            },
            bt: MatrixMerkleProof {
                proof: bt,
                row_indices: cols,
            },
            moe: None,
        })
    }

    /// The whole attempt's A (m × k, chunk 0 patched) and Bᵀ: for small shapes only (tests, the
    /// known-answer test and the CPU engine).
    pub fn matrices(&self, att: &AttemptHost) -> (Vec<i8>, Vec<i8>) {
        let all_a: Vec<usize> = (0..self.m).collect();
        let all_b: Vec<usize> = (0..self.n).collect();
        (self.a_rows(att, &all_a), self.bt_rows(&all_b))
    }
}

/// A job's host state is shared with the verifier thread.
pub type SharedHost = Arc<JobHost>;

#[cfg(test)]
mod tests {
    use super::*;
    use spm_cpuref::{build_plain_proof, commit, verify_v3, Oracle, Problem, U256};
    use spm_proto::Job;

    pub(crate) fn wu(shape: Shape, target_bits: usize, seed: u8) -> WorkUnit {
        let mut header = [0u8; 76];
        header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
        header[4..36].fill(seed);
        header[36..68].fill(seed.wrapping_add(1));
        header[68..72].copy_from_slice(&0x6666_6666u32.to_le_bytes());
        header[72..76].copy_from_slice(&0x1c7f_ffffu32.to_le_bytes());
        let job = Job {
            job_id: format!("job-{seed}"),
            header,
            target: U256::one() << target_bits,
            height: Some(1),
            diff: None,
            cert_version: Some(3),
        };
        WorkUnit::build(&job, shape, 7, u64::from(seed)).unwrap()
    }

    #[test]
    fn nonce_chunk_stays_in_range_and_differs() {
        let base = [0x40u8; CHUNK_BYTES];
        let a = nonce_chunk(&base, 0);
        let b = nonce_chunk(&base, 0x0123_4567_89ab_cdef);
        assert!(a[..16].iter().all(|&x| x as i8 == -8));
        assert_eq!(b[0] as i8, 0xf - 8);
        assert_eq!(b[15] as i8, 0x0 - 8);
        assert_eq!(a[16..], base[16..]);
        assert_ne!(a, b);
    }

    #[test]
    fn host_job_matches_the_oracle() {
        let shape = Shape {
            m: 192,
            n: 128,
            k: 2048,
            r: 128,
        };
        let wu = wu(shape, 233, 5);
        let host = JobHost::new(&wu).unwrap();
        for nonce in [0u64, 1, u64::MAX] {
            let att = host.attempt(nonce);
            let (a, bt) = host.matrices(&att);
            let p =
                Problem::from_matrices(wu.block_header().unwrap(), 192, 128, 2048, a, bt).unwrap();
            let c = commit(&p).unwrap();
            assert_eq!(c.job_key, wu.job_key);
            assert_eq!(c.root_a, att.root_a);
            assert_eq!(c.root_b, host.root_b());
            assert_eq!(c.bound_a, att.bound_a);
            assert_eq!(c.b_noise_seed, host.b_noise_seed());
            assert_eq!(c.a_noise_seed, att.a_noise_seed);
            let oracle = Oracle::new(&p).unwrap();
            for &(tr, tc) in &[(0u32, 0u32), (7, 6), (64 + 3, 64 + 2), (128 + 7, 64 + 6)] {
                let want = oracle.tile(tr, tc).unwrap();
                assert_eq!(host.tile(&att, tr, tc).unwrap(), want);
                let mine = host.proof(&att, tr, tc).unwrap();
                let theirs = build_plain_proof(&p, &want).unwrap();
                assert_eq!(
                    bincode::serialize(&mine).unwrap(),
                    bincode::serialize(&theirs).unwrap(),
                    "proof of ({tr}, {tc})"
                );
            }
            // Every hit at the share bound verifies with the official verifier.
            for hit in oracle.find_hits(wu.share_bound()).unwrap().iter().take(3) {
                let proof = host.proof(&att, hit.t_rows, hit.t_cols).unwrap();
                verify_v3(host.header(), &proof, Some(wu.nbits_share)).unwrap();
            }
        }
        assert!(host.tile(&host.attempt(0), 8, 0).is_err());
    }
}
