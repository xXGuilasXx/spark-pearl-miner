//! Row-level views: any range of a generated matrix, the noise and noised entries of a few rows,
//! and one tile's transcript and digest from just its 8 A' rows and 16 B'ᵀ rows.
//!
//! This is what the GPU worker needs on the CPU for full-size jobs (131072 × 4096 operands are
//! never materialized): regenerate the rows a tile or a Merkle leaf touches, noise them with the
//! official generators for exactly those row indices, and replay the transcript of
//! [`crate::Oracle::trace`]. The tests prove every piece equal to the whole-matrix oracle.

use anyhow::{ensure, Context, Result};
use zk_pow::api::proof_utils::compute_jackpot_hash;
use zk_pow::circuit::pearl_noise::{generate_permutation_matrix, generate_uniform_random_matrix};

use crate::commit::Hash256;
use crate::noise::add_noise;
use crate::oracle::{TileResult, TRANSCRIPT_ROTL, TRANSCRIPT_WORDS};
use crate::problem::SplitMix64;

/// Entry `start + i` of `fill_int7(seed, domain, ·)` into `out[i]`, for any `start` (SplitMix64
/// is counter based: word `w` is `mix((seed ^ domain) + (w + 1)·γ)`).
pub fn fill_int7_at(seed: u64, domain: u64, start: usize, out: &mut [i8]) {
    let state = seed ^ domain;
    let mut e = start;
    let mut i = 0;
    while i < out.len() {
        let w = (e / 8) as u64;
        let word =
            SplitMix64::mix(state.wrapping_add(w.wrapping_add(1).wrapping_mul(SplitMix64::GAMMA)));
        let bytes = word.to_le_bytes();
        for &b in &bytes[e % 8..] {
            if i == out.len() {
                break;
            }
            // b & 0x7f is in [0, 127]: lossless cast, no wrap.
            out[i] = (b & 0x7f) as i8 - 64;
            i += 1;
            e += 1;
        }
    }
}

/// Same as [`fill_int7_at`] as two's-complement bytes (the bytes the Merkle tree commits).
pub fn fill_int7_bytes_at(seed: u64, domain: u64, start: usize, out: &mut [u8]) {
    let state = seed ^ domain;
    let mut e = start;
    let mut i = 0;
    while i < out.len() {
        let w = (e / 8) as u64;
        let word =
            SplitMix64::mix(state.wrapping_add(w.wrapping_add(1).wrapping_mul(SplitMix64::GAMMA)));
        let bytes = word.to_le_bytes();
        for &b in &bytes[e % 8..] {
            if i == out.len() {
                break;
            }
            out[i] = ((b & 0x7f) as i8 - 64) as u8;
            i += 1;
            e += 1;
        }
    }
}

/// One side of the noise (A with `SEED_LABEL_A` and `a_noise_seed`, or B with `SEED_LABEL_B` and
/// `b_noise_seed`) with its k permutation pairs generated once.
#[derive(Clone, Debug)]
pub struct NoiseSide {
    label: [u8; 32],
    seed: Hash256,
    rank: usize,
    pairs: Vec<[u32; 2]>,
}

impl NoiseSide {
    /// `pairs` = the official `generate_permutation_matrix(label, seed, k, rank)` (A_R or B_L).
    pub fn new(label: &[u8; 32], seed: &Hash256, k: usize, rank: usize) -> Self {
        Self {
            label: *label,
            seed: *seed,
            rank,
            pairs: generate_permutation_matrix(label, seed, k, rank),
        }
    }

    pub fn k(&self) -> usize {
        self.pairs.len()
    }

    /// Noise rows `E[row][l] = F[row][p(l)] − F[row][q(l)]` for `rows`, with the uniform factor
    /// rows F from the official `generate_uniform_random_matrix(label, seed, rows, rank)`.
    pub fn noise_rows(&self, rows: &[usize]) -> Result<Vec<i8>> {
        let factor = generate_uniform_random_matrix(&self.label, &self.seed, rows, self.rank);
        let mut out = Vec::with_capacity(rows.len() * self.pairs.len());
        for f in &factor {
            ensure!(
                f.len() == self.rank,
                "uniform factor row has the wrong width"
            );
            for &[p, q] in &self.pairs {
                let x = f.get(p as usize).context("pair index out of range")?;
                let y = f.get(q as usize).context("pair index out of range")?;
                out.push(x.wrapping_sub(*y));
            }
        }
        Ok(out)
    }

    /// Noised rows `base + E` (`base` holds `rows.len()` rows of k entries, in order).
    pub fn noised_rows(&self, base: &[i8], rows: &[usize]) -> Result<Vec<i8>> {
        ensure!(
            base.len() == rows.len() * self.pairs.len(),
            "base rows have the wrong size"
        );
        add_noise(base, &self.noise_rows(rows)?)
    }
}

/// Transcript and digest of the tile `(t_rows, t_cols)` from its h noised A' rows and w noised
/// B'ᵀ rows (row major, k entries each, in pattern order), exactly as [`crate::Oracle::trace`].
pub fn tile_from_rows(
    t_rows: u32,
    t_cols: u32,
    a_rows: &[i8],
    bt_rows: &[i8],
    k: usize,
    rank: usize,
    a_noise_seed: &Hash256,
) -> Result<TileResult> {
    ensure!(
        k > 0 && rank > 0 && a_rows.len().is_multiple_of(k) && bt_rows.len().is_multiple_of(k),
        "rows must be whole k-long rows"
    );
    let h = a_rows.len() / k;
    let w = bt_rows.len() / k;
    ensure!(h > 0 && w > 0, "empty tile");
    let mut acc = vec![0i32; h * w];
    let mut transcript = [0u32; TRANSCRIPT_WORDS];
    for s in 0..k / rank {
        let span = s * rank..(s + 1) * rank;
        for (u, a_row) in a_rows.chunks_exact(k).enumerate() {
            let a = &a_row[span.clone()];
            for (v, b_row) in bt_rows.chunks_exact(k).enumerate() {
                let dot: i32 = a
                    .iter()
                    .zip(&b_row[span.clone()])
                    .map(|(&x, &y)| i32::from(x) * i32::from(y))
                    .sum();
                acc[u * w + v] = acc[u * w + v].wrapping_add(dot);
            }
        }
        let fold = acc.iter().fold(0u32, |x, &c| x ^ c as u32);
        let slot = s % TRANSCRIPT_WORDS;
        transcript[slot] = transcript[slot].rotate_left(TRANSCRIPT_ROTL) ^ fold;
    }
    Ok(TileResult {
        t_rows,
        t_cols,
        transcript,
        digest: compute_jackpot_hash(&transcript, *a_noise_seed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commit, fill_int7, noise_factors, Oracle, Problem, DOMAIN_A, DOMAIN_BT, SEED_LABEL_A,
        SEED_LABEL_B,
    };
    use spm_pow::IncompleteBlockHeader;

    fn header() -> IncompleteBlockHeader {
        IncompleteBlockHeader {
            version: 0x2000_0000,
            prev_block: [3; 32],
            merkle_root: [4; 32],
            timestamp: 0x6666_6666,
            nbits: 0x1d7f_ffff,
        }
    }

    #[test]
    fn fill_at_any_offset_matches_the_stream() {
        let whole = fill_int7(11, DOMAIN_A, 5000);
        for (start, len) in [
            (0, 5000),
            (1, 7),
            (7, 9),
            (8, 64),
            (1023, 1500),
            (4095, 905),
        ] {
            let mut out = vec![0i8; len];
            fill_int7_at(11, DOMAIN_A, start, &mut out);
            assert_eq!(out, whole[start..start + len], "{start}+{len}");
            let mut bytes = vec![0u8; len];
            fill_int7_bytes_at(11, DOMAIN_A, start, &mut bytes);
            assert!(bytes.iter().zip(&out).all(|(&b, &x)| b == x as u8));
        }
    }

    #[test]
    fn row_noise_and_tiles_match_the_oracle() {
        for (m, n, k, seed) in [(128usize, 192usize, 2048usize, 5u64), (64, 128, 4096, 6)] {
            let p = Problem::generate(m, n, k, header(), seed).unwrap();
            let oracle = Oracle::new(&p).unwrap();
            let c = commit(&p).unwrap();
            let noise = noise_factors(&p, &c).unwrap().expand().unwrap();
            let a_side = NoiseSide::new(&SEED_LABEL_A, &c.a_noise_seed, k, p.rank());
            let b_side = NoiseSide::new(&SEED_LABEL_B, &c.b_noise_seed, k, p.rank());
            let rows = [0usize, 5, 63, m - 1];
            let e = a_side.noise_rows(&rows).unwrap();
            for (i, &r) in rows.iter().enumerate() {
                assert_eq!(e[i * k..(i + 1) * k], noise.e_a[r * k..(r + 1) * k]);
            }
            let cols = [1usize, n - 2];
            let e = b_side.noise_rows(&cols).unwrap();
            for (i, &r) in cols.iter().enumerate() {
                assert_eq!(e[i * k..(i + 1) * k], noise.e_bt[r * k..(r + 1) * k]);
            }
            for &t_rows in &p.row_offsets()[..3] {
                for &t_cols in p.col_offsets().iter().step_by(5) {
                    let ri: Vec<usize> = p
                        .row_pattern()
                        .iter()
                        .map(|&d| (t_rows + d) as usize)
                        .collect();
                    let ci: Vec<usize> = p
                        .col_pattern()
                        .iter()
                        .map(|&d| (t_cols + d) as usize)
                        .collect();
                    let mut a = Vec::new();
                    for &r in &ri {
                        let mut row = vec![0i8; k];
                        fill_int7_at(seed, DOMAIN_A, r * k, &mut row);
                        a.extend_from_slice(&row);
                    }
                    let mut bt = Vec::new();
                    for &r in &ci {
                        let mut row = vec![0i8; k];
                        fill_int7_at(seed, DOMAIN_BT, r * k, &mut row);
                        bt.extend_from_slice(&row);
                    }
                    let a = a_side.noised_rows(&a, &ri).unwrap();
                    let bt = b_side.noised_rows(&bt, &ci).unwrap();
                    let got = tile_from_rows(t_rows, t_cols, &a, &bt, k, p.rank(), &c.a_noise_seed)
                        .unwrap();
                    assert_eq!(
                        got,
                        oracle.tile(t_rows, t_cols).unwrap(),
                        "tile ({t_rows}, {t_cols})"
                    );
                }
            }
        }
    }
}
