//! Low-rank noise E_A = A_L·A_R and E_B = B_L·B_R.
//!
//! The four factors come straight from the official generators in
//! `zk_pow::circuit::pearl_noise`; only the (trivial) sparse products are written here, in the
//! flat layout the GPU uses, and they are tested against `compute_noise_for_indices`.

use anyhow::{ensure, Context, Result};
use zk_pow::circuit::pearl_noise::{generate_permutation_matrix, generate_uniform_random_matrix};

use crate::commit::Commitment;
use crate::problem::Problem;

const fn padded_label(label: [u8; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < label.len() {
        out[i] = label[i];
        i += 1;
    }
    out
}

/// BLAKE3 message seed of every A-side noise hash (`"A_tensor"` zero-padded to 32 bytes).
pub const SEED_LABEL_A: [u8; 32] = padded_label(*b"A_tensor");
/// BLAKE3 message seed of every B-side noise hash (`"B_tensor"` zero-padded to 32 bytes).
pub const SEED_LABEL_B: [u8; 32] = padded_label(*b"B_tensor");

/// Smallest entry of the uniform factors A_L and B_Rᵀ: `(byte & 63) - 32`.
pub const UNIFORM_MIN: i8 = -32;
/// Largest entry of the uniform factors.
pub const UNIFORM_MAX: i8 = 31;
/// Bound of every noise entry: a difference of two distinct uniform entries, so |e| ≤ 63.
pub const NOISE_ABS_MAX: i8 = 63;

/// The four noise factors of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoiseFactors {
    pub rank: usize,
    /// A_L, m×r row major, keyed with `a_noise_seed`, entries in `[-32, 31]`.
    pub a_l: Vec<i8>,
    /// A_R as k column descriptors: column l of A_R is +1 at `a_r[l][0]`, −1 at `a_r[l][1]`.
    pub a_r: Vec<[u32; 2]>,
    /// B_L as k row descriptors: row l of B_L is +1 at `b_l[l][0]`, −1 at `b_l[l][1]`.
    pub b_l: Vec<[u32; 2]>,
    /// B_Rᵀ, n×r row major, keyed with `b_noise_seed`, entries in `[-32, 31]`.
    pub b_rt: Vec<i8>,
}

/// The dense noise, in the same layouts as the problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Noise {
    /// E_A, m×k row major.
    pub e_a: Vec<i8>,
    /// E_Bᵀ, n×k row major (row j is column j of E_B).
    pub e_bt: Vec<i8>,
}

/// Generates the factors with the official zk-pow functions.
pub fn noise_factors(p: &Problem, c: &Commitment) -> Result<NoiseFactors> {
    let r = p.rank();
    let rows: Vec<usize> = (0..p.m).collect();
    let cols: Vec<usize> = (0..p.n).collect();
    let factors = NoiseFactors {
        rank: r,
        a_l: generate_uniform_random_matrix(&SEED_LABEL_A, &c.a_noise_seed, &rows, r).concat(),
        a_r: generate_permutation_matrix(&SEED_LABEL_A, &c.a_noise_seed, p.k, r),
        b_l: generate_permutation_matrix(&SEED_LABEL_B, &c.b_noise_seed, p.k, r),
        b_rt: generate_uniform_random_matrix(&SEED_LABEL_B, &c.b_noise_seed, &cols, r).concat(),
    };
    ensure!(
        factors.a_l.len() == p.m * r && factors.b_rt.len() == p.n * r,
        "uniform factor has the wrong size"
    );
    ensure!(
        factors.a_r.len() == p.k && factors.b_l.len() == p.k,
        "permutation factor has the wrong size"
    );
    Ok(factors)
}

impl NoiseFactors {
    /// E_A[i][l] = A_L[i][a_r[l][0]] − A_L[i][a_r[l][1]] and
    /// E_Bᵀ[j][l] = B_Rᵀ[j][b_l[l][0]] − B_Rᵀ[j][b_l[l][1]], in i8 two's complement exactly like the
    /// reference (`(x as i32 - y as i32) as i8`); with entries in [-32, 31] it never wraps.
    pub fn expand(&self) -> Result<Noise> {
        Ok(Noise {
            e_a: sparse_product(&self.a_l, self.rank, &self.a_r).context("E_A")?,
            e_bt: sparse_product(&self.b_rt, self.rank, &self.b_l).context("E_Bᵀ")?,
        })
    }
}

/// `out[i*k + l] = dense[i*r + pairs[l][0]] − dense[i*r + pairs[l][1]]` (wrapping i8).
fn sparse_product(dense: &[i8], r: usize, pairs: &[[u32; 2]]) -> Result<Vec<i8>> {
    ensure!(
        r > 0 && dense.len().is_multiple_of(r),
        "dense factor is not a whole number of r-wide rows"
    );
    let mut out = Vec::with_capacity(dense.len() / r * pairs.len());
    for row in dense.chunks_exact(r) {
        for &[plus, minus] in pairs {
            let x = row
                .get(plus as usize)
                .context("permutation index out of range")?;
            let y = row
                .get(minus as usize)
                .context("permutation index out of range")?;
            out.push(x.wrapping_sub(*y));
        }
    }
    Ok(out)
}

/// `x + e` entrywise. The sum lies in [-127, 127] (|x| ≤ 64, |e| ≤ 63), so the noised operands
/// are exactly representable as s8, which is what the int8 tensor cores consume. The reference
/// forms the same sum in i32; the checked conversion proves nothing is lost.
pub fn add_noise(x: &[i8], e: &[i8]) -> Result<Vec<i8>> {
    ensure!(x.len() == e.len(), "operand and noise sizes differ");
    x.iter()
        .zip(e)
        .map(|(&x, &e)| {
            i8::try_from(i16::from(x) + i16::from(e))
                .map_err(|_| anyhow::anyhow!("{x} + {e} leaves the s8 range"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The noise hash exactly as README.md describes it for the GPU: a 64-byte message, zero
    /// except `(index + 1)` as i32 LE at bytes `4*slot..4*slot+4` and the label at 32..64,
    /// hashed with BLAKE3 keyed by the noise seed.
    fn spec_hash(index: usize, label: &[u8; 32], key: &[u8; 32], slot: usize) -> [u8; 32] {
        let mut msg = [0u8; 64];
        msg[4 * slot..4 * slot + 4].copy_from_slice(&(index as i32 + 1).to_le_bytes());
        msg[32..].copy_from_slice(label);
        *blake3::keyed_hash(key, &msg).as_bytes()
    }

    /// Row `row` of a uniform factor: bytes `row*r .. (row+1)*r` of the concatenated slot-0
    /// hashes, each mapped to `(byte & 63) - 32`.
    fn spec_uniform_row(label: &[u8; 32], key: &[u8; 32], row: usize, r: usize) -> Vec<i8> {
        (row * r..(row + 1) * r)
            .map(|idx| {
                let byte = spec_hash(idx / 32, label, key, 0)[idx % 32];
                (byte & 63) as i8 - 32
            })
            .collect()
    }

    /// Pair `l` of a permutation factor: word `l % 8` (LE u32) of slot-1 hash `l / 8`;
    /// `p = x & (r - 1)`, `q = p ^ (1 + mulhi32(r - 1, x))`.
    fn spec_pair(label: &[u8; 32], key: &[u8; 32], l: usize, r: usize) -> [u32; 2] {
        let h = spec_hash(l / 8, label, key, 1);
        let w = l % 8;
        let x = u32::from_le_bytes([h[4 * w], h[4 * w + 1], h[4 * w + 2], h[4 * w + 3]]);
        let p = x & (r as u32 - 1);
        let q = p ^ (1 + ((u64::from(r as u32 - 1) * u64::from(x)) >> 32) as u32);
        [p, q]
    }

    #[test]
    fn documented_generators_match_the_official_ones() {
        let r = 128;
        let k = 4096;
        for s in 0..24u8 {
            let key = [s.wrapping_mul(37).wrapping_add(1); 32];
            for label in [&SEED_LABEL_A, &SEED_LABEL_B] {
                let rows = [0usize, 1, 7, 1000, 131_071];
                let official = generate_uniform_random_matrix(label, &key, &rows, r);
                for (row, got) in rows.iter().zip(&official) {
                    assert_eq!(got, &spec_uniform_row(label, &key, *row, r), "row {row}");
                }
                let pairs = generate_permutation_matrix(label, &key, k, r);
                for (l, got) in pairs.iter().enumerate() {
                    assert_eq!(*got, spec_pair(label, &key, l, r), "pair {l}");
                    assert!(got[0] != got[1] && got[0] < r as u32 && got[1] < r as u32);
                }
            }
        }
        assert_eq!(&SEED_LABEL_A[..8], b"A_tensor");
        assert!(SEED_LABEL_B[8..].iter().all(|&b| b == 0));
    }
}
