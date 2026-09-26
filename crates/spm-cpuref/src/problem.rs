//! Problem definition (header, configuration, A and Bᵀ) and the deterministic int7 generator.

use anyhow::{ensure, Context, Result};
use spm_pow::{IncompleteBlockHeader, MiningConfiguration, SeedDerivation};
use zk_pow::api::proof::PublicProofParams;

/// Smallest entry drawn by [`fill_int7`].
pub const GEN_MIN: i8 = -64;
/// Largest entry drawn by [`fill_int7`] (int7: 128 values).
pub const GEN_MAX: i8 = 63;
/// Smallest committed entry the official verifier accepts (`verify_plain_proof`, IRANGE7P1).
pub const SIGNAL_MIN: i8 = -64;
/// Largest committed entry the official verifier accepts. Note the asymmetry: 64 is legal,
/// and the reference miner (`try_mine_one`) draws from `[-64, 64]`.
pub const SIGNAL_MAX: i8 = 64;

/// Stream separator for A (`"spm-a-01"` in ASCII).
pub const DOMAIN_A: u64 = 0x7370_6d2d_612d_3031;
/// Stream separator for Bᵀ (`"spm-b-01"` in ASCII).
pub const DOMAIN_BT: u64 = 0x7370_6d2d_622d_3031;

/// SplitMix64 (Steele, Lea, Flood 2014). Counter based, so the GPU can reproduce any word
/// directly: word `w` (0-based) of a stream seeded with `s` is `mix(s + (w + 1) * GAMMA)`.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Golden-ratio increment.
    pub const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The output finalizer applied to each counter value.
    pub fn mix(mut z: u64) -> u64 {
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(Self::GAMMA);
        Self::mix(self.state)
    }
}

/// `len` entries uniform in `[GEN_MIN, GEN_MAX]` = `[-64, 63]`.
///
/// Stream: SplitMix64 seeded with `seed ^ domain`; every output word is split into its 8
/// little-endian bytes and each byte becomes one entry `(byte & 0x7f) - 64`. Entry `i` therefore
/// comes from byte `i % 8` of word `i / 8`.
pub fn fill_int7(seed: u64, domain: u64, len: usize) -> Vec<i8> {
    let mut rng = SplitMix64::new(seed ^ domain);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        for byte in rng.next_u64().to_le_bytes() {
            if out.len() == len {
                break;
            }
            // byte & 0x7f is in [0, 127], so the cast is lossless and the subtraction cannot wrap.
            out.push((byte & 0x7f) as i8 - 64);
        }
    }
    out
}

/// One PearlHash V3 work unit: C = A·B with A (m×k) and Bᵀ (n×k) both stored row major, exactly
/// the byte layouts the Merkle commitments hash (A row major, B column major = Bᵀ row major).
#[derive(Clone, Debug)]
pub struct Problem {
    pub m: usize,
    pub n: usize,
    pub k: usize,
    pub header: IncompleteBlockHeader,
    pub config: MiningConfiguration,
    /// m×k, row major, entries in `[SIGNAL_MIN, SIGNAL_MAX]`.
    pub a: Vec<i8>,
    /// n×k, row major (row j is column j of B), entries in `[SIGNAL_MIN, SIGNAL_MAX]`.
    pub bt: Vec<i8>,
    /// Generator seed when built by [`Problem::generate`]; `None` for caller-supplied matrices.
    pub seed: Option<u64>,
}

impl Problem {
    /// Deterministic problem with our fixed configuration (`spm_pow::mining_config(k)`: r = 128,
    /// 8×16 hash tile). A = `fill_int7(seed, DOMAIN_A, m*k)`, Bᵀ = `fill_int7(seed, DOMAIN_BT, n*k)`.
    pub fn generate(
        m: usize,
        n: usize,
        k: usize,
        header: IncompleteBlockHeader,
        seed: u64,
    ) -> Result<Self> {
        let config = spm_pow::mining_config(u32::try_from(k).context("k does not fit in u32")?)?;
        check_shape(&header, &config, m, n)?;
        let a_len = m.checked_mul(k).context("m*k overflows usize")?;
        let bt_len = n.checked_mul(k).context("n*k overflows usize")?;
        let a = fill_int7(seed, DOMAIN_A, a_len);
        let bt = fill_int7(seed, DOMAIN_BT, bt_len);
        Self::from_parts(header, config, m, n, a, bt, Some(seed))
    }

    /// Caller-supplied matrices with our fixed configuration for `k`.
    pub fn from_matrices(
        header: IncompleteBlockHeader,
        m: usize,
        n: usize,
        k: usize,
        a: Vec<i8>,
        bt: Vec<i8>,
    ) -> Result<Self> {
        let config = spm_pow::mining_config(u32::try_from(k).context("k does not fit in u32")?)?;
        Self::from_parts(header, config, m, n, a, bt, None)
    }

    /// Fully general constructor (any dense configuration the official sanity check accepts).
    pub fn from_parts(
        header: IncompleteBlockHeader,
        config: MiningConfiguration,
        m: usize,
        n: usize,
        a: Vec<i8>,
        bt: Vec<i8>,
        seed: Option<u64>,
    ) -> Result<Self> {
        let problem = Self {
            m,
            n,
            k: config.common_dim as usize,
            header,
            config,
            a,
            bt,
            seed,
        };
        problem.validate()?;
        Ok(problem)
    }

    /// Checks the shape against the official consensus sanity rules, the buffer sizes and the
    /// entry range. Every public entry point of this crate calls it first.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.k == self.config.common_dim as usize,
            "k={} differs from config.common_dim={}",
            self.k,
            self.config.common_dim
        );
        check_shape(&self.header, &self.config, self.m, self.n)?;
        ensure!(
            Some(self.a.len()) == self.m.checked_mul(self.k),
            "A must hold m*k = {}*{} entries",
            self.m,
            self.k
        );
        ensure!(
            Some(self.bt.len()) == self.n.checked_mul(self.k),
            "Bᵀ must hold n*k = {}*{} entries",
            self.n,
            self.k
        );
        for (name, entries) in [("A", &self.a), ("Bᵀ", &self.bt)] {
            if let Some(pos) = entries
                .iter()
                .position(|v| !(SIGNAL_MIN..=SIGNAL_MAX).contains(v))
            {
                anyhow::bail!(
                    "{name}[{pos}] = {} is outside [{SIGNAL_MIN}, {SIGNAL_MAX}]",
                    entries[pos]
                );
            }
        }
        Ok(())
    }

    /// Noise rank r.
    pub fn rank(&self) -> usize {
        self.config.rank as usize
    }

    /// Number of r-wide k-slices that enter the transcript: `floor(k / r)`. Columns past
    /// `slices() * r` (only when r does not divide k) are committed but never hashed.
    pub fn slices(&self) -> usize {
        self.k / self.rank()
    }

    /// Row pattern of a hash tile (offsets relative to `t_rows`).
    pub fn row_pattern(&self) -> Vec<u32> {
        self.config.rows_pattern.to_list()
    }

    /// Column pattern of a hash tile (offsets relative to `t_cols`).
    pub fn col_pattern(&self) -> Vec<u32> {
        self.config.cols_pattern.to_list()
    }

    /// Valid tile base rows, ascending (the reference `threads_partition` order).
    pub fn row_offsets(&self) -> Vec<u32> {
        (0..self.m as u32)
            .filter(|&t| self.config.rows_pattern.offset_is_valid(t))
            .collect()
    }

    /// Valid tile base columns, ascending.
    pub fn col_offsets(&self) -> Vec<u32> {
        (0..self.n as u32)
            .filter(|&t| self.config.cols_pattern.offset_is_valid(t))
            .collect()
    }

    /// Number of hash tiles; the tiles partition the m×n output exactly once.
    pub fn tile_count(&self) -> usize {
        self.row_offsets().len() * self.col_offsets().len()
    }

    /// Whether `(t_rows, t_cols)` is the base of a hash tile of this problem.
    pub fn is_tile(&self, t_rows: u32, t_cols: u32) -> bool {
        (t_rows as usize) < self.m
            && (t_cols as usize) < self.n
            && self.config.rows_pattern.offset_is_valid(t_rows)
            && self.config.cols_pattern.offset_is_valid(t_cols)
    }

    /// The V3 (salted) public parameters of tile `(t_rows, t_cols)` for committed roots
    /// `hash_a`/`hash_b`, exactly as the verifier reconstructs them from a PlainProof.
    pub fn public_params(
        &self,
        hash_a: [u8; 32],
        hash_b: [u8; 32],
        t_rows: u32,
        t_cols: u32,
    ) -> Result<PublicProofParams> {
        let m = u32::try_from(self.m).context("m does not fit in u32")?;
        let n = u32::try_from(self.n).context("n does not fit in u32")?;
        Ok(PublicProofParams::new(
            self.header,
            SeedDerivation::Salted,
            self.config,
            hash_a,
            hash_b,
            [0u8; 32],
            m,
            n,
            t_rows,
            t_cols,
        ))
    }
}

/// Shape rules: dense job, pattern periods divide m and n, and the official
/// `public_params_sanity_check` (rank, k bounds, h·w, m/n limits) passes on tile (0, 0).
fn check_shape(
    header: &IncompleteBlockHeader,
    config: &MiningConfiguration,
    m: usize,
    n: usize,
) -> Result<()> {
    ensure!(
        config.moe.is_none(),
        "MoE (grouped GEMM) jobs are not modelled by this oracle"
    );
    let rp = config.rows_pattern.period() as usize;
    let cp = config.cols_pattern.period() as usize;
    ensure!(
        m > 0 && m.is_multiple_of(rp),
        "m={m} must be a positive multiple of the row pattern period {rp}"
    );
    ensure!(
        n > 0 && n.is_multiple_of(cp),
        "n={n} must be a positive multiple of the column pattern period {cp}"
    );
    let m32 = u32::try_from(m).context("m does not fit in u32")?;
    let n32 = u32::try_from(n).context("n does not fit in u32")?;
    PublicProofParams::new(
        *header,
        SeedDerivation::Salted,
        *config,
        [0; 32],
        [0; 32],
        [0; 32],
        m32,
        n32,
        0,
        0,
    )
    .sanity_check()
    .context("official sanity check rejected the problem shape")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_reference_vector() {
        // Published SplitMix64 outputs for seed 0.
        let mut rng = SplitMix64::new(0);
        let expected = [
            0xe220_a839_7b1d_cdaf_u64,
            0x6e78_9e6a_a1b9_65f4,
            0x06c4_5d18_8009_454f,
        ];
        for e in expected {
            assert_eq!(rng.next_u64(), e);
        }
    }

    #[test]
    fn fill_int7_range_and_layout() {
        let v = fill_int7(7, DOMAIN_A, 1003);
        assert_eq!(v.len(), 1003);
        assert!(v.iter().all(|x| (GEN_MIN..=GEN_MAX).contains(x)));
        // Both extremes show up in a sample this size.
        assert!(v.contains(&GEN_MIN) && v.contains(&GEN_MAX));
        // Entry i is byte i % 8 of word i / 8.
        let mut rng = SplitMix64::new(7 ^ DOMAIN_A);
        let w0 = rng.next_u64().to_le_bytes();
        let w1 = rng.next_u64().to_le_bytes();
        assert_eq!(v[3], (w0[3] & 0x7f) as i8 - 64);
        assert_eq!(v[9], (w1[1] & 0x7f) as i8 - 64);
        // Streams differ per domain and per seed.
        assert_ne!(fill_int7(7, DOMAIN_A, 64), fill_int7(7, DOMAIN_BT, 64));
        assert_ne!(fill_int7(7, DOMAIN_A, 64), fill_int7(8, DOMAIN_A, 64));
    }
}
