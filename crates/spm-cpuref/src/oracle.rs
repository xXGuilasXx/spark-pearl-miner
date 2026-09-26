//! Noised GEMM and per-tile transcripts.

use anyhow::{anyhow, ensure, Context, Result};
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use zk_pow::api::proof_utils::compute_jackpot_hash;
use zk_pow::circuit::pearl_program::{JACKPOT_SIZE, LROT_PER_TILE};
use zk_pow::ffi::plain_proof::PlainProof;

use crate::commit::{commit_validated, Commitment};
use crate::noise::{add_noise, noise_factors, Noise, NoiseFactors};
use crate::problem::Problem;
use crate::proof::build_plain_proof_with_key;

/// Words in a tile transcript (the reference `JACKPOT_SIZE`).
pub const TRANSCRIPT_WORDS: usize = JACKPOT_SIZE;
/// Left rotation applied to a transcript word each time it is revisited (`LROT_PER_TILE`).
pub const TRANSCRIPT_ROTL: u32 = LROT_PER_TILE;

/// Transcript and jackpot digest of one hash tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileResult {
    /// Base row: the tile's rows are `t_rows + row_pattern`.
    pub t_rows: u32,
    /// Base column: the tile's columns are `t_cols + col_pattern`.
    pub t_cols: u32,
    pub transcript: [u32; TRANSCRIPT_WORDS],
    /// `blake3_keyed(a_noise_seed, transcript as 64 LE bytes)`; compared as a LE U256.
    pub digest: [u8; 32],
}

impl TileResult {
    /// Size of one record in a dump (see [`TileResult::dump_bytes`]).
    pub const DUMP_LEN: usize = 8 + 4 * TRANSCRIPT_WORDS + 32;

    /// The digest as the little-endian 256-bit integer the difficulty check uses.
    pub fn digest_value(&self) -> U256 {
        U256::from_little_endian(&self.digest)
    }

    /// `digest ≤ bound` (the consensus comparison).
    pub fn meets(&self, bound: U256) -> bool {
        self.digest_value() <= bound
    }

    /// Fixed 104-byte record: `t_rows` LE, `t_cols` LE, the 16 transcript words LE, digest.
    pub fn dump_bytes(&self) -> [u8; Self::DUMP_LEN] {
        let mut out = [0u8; Self::DUMP_LEN];
        out[0..4].copy_from_slice(&self.t_rows.to_le_bytes());
        out[4..8].copy_from_slice(&self.t_cols.to_le_bytes());
        for (i, w) in self.transcript.iter().enumerate() {
            out[8 + 4 * i..12 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        out[8 + 4 * TRANSCRIPT_WORDS..].copy_from_slice(&self.digest);
        out
    }
}

/// BLAKE3 (unkeyed) of the concatenated 104-byte records, in order. One value pins every tile.
pub fn tiles_digest(tiles: &[TileResult]) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(tiles.len() * TileResult::DUMP_LEN);
    for t in tiles {
        bytes.extend_from_slice(&t.dump_bytes());
    }
    pearl_blake3::blake3_digest(&bytes, None)
}

/// Index of the first differing tile, or of the first missing one when the lengths differ.
pub fn first_mismatch(expected: &[TileResult], actual: &[TileResult]) -> Option<usize> {
    expected
        .iter()
        .zip(actual)
        .position(|(e, a)| e != a)
        .or_else(|| (expected.len() != actual.len()).then_some(expected.len().min(actual.len())))
}

/// Everything the oracle knows about one tile, for slice-level debugging of a GPU mismatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileTrace {
    pub tile: TileResult,
    /// XOR fold of the cumulative accumulators after each k-slice (`slices()` entries).
    pub folds: Vec<u32>,
    /// Final cumulative accumulators, h×w row major over (row_pattern, col_pattern). Equal to
    /// the matching entries of C' when r divides k.
    pub acc: Vec<i32>,
}

/// A validated problem with its commitment and noised operands, ready to evaluate tiles.
pub struct Oracle<'p> {
    problem: &'p Problem,
    commitment: Commitment,
    a_noised: Vec<i8>,
    bt_noised: Vec<i8>,
    row_pattern: Vec<u32>,
    col_pattern: Vec<u32>,
}

impl<'p> Oracle<'p> {
    pub fn new(problem: &'p Problem) -> Result<Self> {
        problem.validate()?;
        let commitment = commit_validated(problem)?;
        let noise = noise_factors(problem, &commitment)?.expand()?;
        let a_noised = add_noise(&problem.a, &noise.e_a).context("A' = A + E_A")?;
        let bt_noised = add_noise(&problem.bt, &noise.e_bt).context("B'ᵀ = Bᵀ + E_Bᵀ")?;
        Ok(Self {
            problem,
            commitment,
            a_noised,
            bt_noised,
            row_pattern: problem.row_pattern(),
            col_pattern: problem.col_pattern(),
        })
    }

    pub fn problem(&self) -> &Problem {
        self.problem
    }

    pub fn commitment(&self) -> &Commitment {
        &self.commitment
    }

    /// Recomputes the noise factors (not cached: only debugging tools need them).
    pub fn noise_factors(&self) -> Result<NoiseFactors> {
        noise_factors(self.problem, &self.commitment)
    }

    /// Recomputes the dense noise.
    pub fn noise(&self) -> Result<Noise> {
        self.noise_factors()?.expand()
    }

    /// A' = A + E_A, m×k row major, s8.
    pub fn noised_a(&self) -> &[i8] {
        &self.a_noised
    }

    /// B'ᵀ = Bᵀ + E_Bᵀ, n×k row major, s8.
    pub fn noised_bt(&self) -> &[i8] {
        &self.bt_noised
    }

    /// C' = A'·B' over the full k, m×n row major, exact i32.
    pub fn gemm(&self) -> Vec<i32> {
        let k = self.problem.k;
        let mut c = Vec::with_capacity(self.problem.m * self.problem.n);
        for a_row in self.a_noised.chunks_exact(k) {
            for b_row in self.bt_noised.chunks_exact(k) {
                c.push(dot(a_row, b_row));
            }
        }
        c
    }

    /// Transcript and digest of the tile based at `(t_rows, t_cols)`.
    pub fn tile(&self, t_rows: u32, t_cols: u32) -> Result<TileResult> {
        Ok(self.trace(t_rows, t_cols)?.tile)
    }

    /// Transcript, digest, per-slice folds and final accumulators of one tile.
    ///
    /// For slice s = 0..floor(k/r): every accumulator of the tile adds its r-long partial dot
    /// product over columns [s·r, (s+1)·r); then the XOR of all (cumulative) accumulators, read
    /// as u32, is folded into word s mod 16: `t[s % 16] = t[s % 16].rotate_left(13) ^ fold`.
    pub fn trace(&self, t_rows: u32, t_cols: u32) -> Result<TileTrace> {
        let p = self.problem;
        ensure!(
            p.is_tile(t_rows, t_cols),
            "({t_rows}, {t_cols}) is not a hash-tile base of this problem"
        );
        let (k, r) = (p.k, p.rank());
        let rows = operand_rows(&self.a_noised, k, t_rows, &self.row_pattern)?;
        let cols = operand_rows(&self.bt_noised, k, t_cols, &self.col_pattern)?;
        let w = cols.len();
        let mut acc = vec![0i32; rows.len() * w];
        let mut folds = Vec::with_capacity(p.slices());
        let mut transcript = [0u32; TRANSCRIPT_WORDS];
        for s in 0..p.slices() {
            let span = s * r..(s + 1) * r;
            for (u, a_row) in rows.iter().enumerate() {
                let a_slice = &a_row[span.clone()];
                for (v, b_row) in cols.iter().enumerate() {
                    let cell = &mut acc[u * w + v];
                    *cell = cell.wrapping_add(dot(a_slice, &b_row[span.clone()]));
                }
            }
            // `as u32` reinterprets the two's-complement bits, like the reference `x as u32`.
            let fold = acc.iter().fold(0u32, |x, &c| x ^ c as u32);
            let slot = s % TRANSCRIPT_WORDS;
            transcript[slot] = transcript[slot].rotate_left(TRANSCRIPT_ROTL) ^ fold;
            folds.push(fold);
        }
        let digest = compute_jackpot_hash(&transcript, self.commitment.a_noise_seed);
        Ok(TileTrace {
            tile: TileResult {
                t_rows,
                t_cols,
                transcript,
                digest,
            },
            folds,
            acc,
        })
    }

    /// Every tile, rows of tiles outer and columns inner, both ascending (the order of the
    /// reference miner). Spread over the available cores; the result is deterministic.
    pub fn transcripts(&self) -> Result<Vec<TileResult>> {
        let row_offsets = self.problem.row_offsets();
        let col_offsets = self.problem.col_offsets();
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, row_offsets.len().max(1));
        let per_thread = row_offsets.len().div_ceil(threads).max(1);
        let cols = &col_offsets;
        std::thread::scope(|scope| {
            let workers: Vec<_> = row_offsets
                .chunks(per_thread)
                .map(|band| {
                    scope.spawn(move || -> Result<Vec<TileResult>> {
                        let mut out = Vec::with_capacity(band.len() * cols.len());
                        for &t_rows in band {
                            for &t_cols in cols {
                                out.push(self.tile(t_rows, t_cols)?);
                            }
                        }
                        Ok(out)
                    })
                })
                .collect();
            let mut all = Vec::with_capacity(row_offsets.len() * cols.len());
            for worker in workers {
                all.extend(
                    worker
                        .join()
                        .map_err(|_| anyhow!("tile worker panicked"))??,
                );
            }
            Ok(all)
        })
    }

    /// Tiles whose digest is ≤ `bound`, in [`Oracle::transcripts`] order.
    pub fn find_hits(&self, bound: U256) -> Result<Vec<TileResult>> {
        Ok(self
            .transcripts()?
            .into_iter()
            .filter(|t| t.meets(bound))
            .collect())
    }

    /// PlainProof opening the tile's rows of A and Bᵀ (see [`crate::build_plain_proof`]).
    pub fn build_plain_proof(&self, tile: &TileResult) -> Result<PlainProof> {
        build_plain_proof_with_key(
            self.problem,
            &self.commitment.job_key,
            tile.t_rows,
            tile.t_cols,
        )
    }
}

/// The k-long rows `base + offset` of a row-major matrix.
fn operand_rows<'a>(
    matrix: &'a [i8],
    k: usize,
    base: u32,
    pattern: &[u32],
) -> Result<Vec<&'a [i8]>> {
    pattern
        .iter()
        .map(|&d| {
            let row = base as usize + d as usize;
            matrix
                .get(row * k..(row + 1) * k)
                .with_context(|| format!("row {row} is out of range"))
        })
        .collect()
}

/// Exact int8 dot product. |a·b| ≤ 127·127·len; with len ≤ k ≤ 2^16 (consensus) this stays
/// below 2^31, so neither the sum nor the tile accumulators can overflow.
fn dot(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(x) * i32::from(y))
        .sum()
}
