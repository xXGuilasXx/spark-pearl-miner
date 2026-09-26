//! Golden-fixture format (`tests/fixtures/golden-cpuref-<seed>.json`).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::oracle::{tiles_digest, Oracle, TileResult, TRANSCRIPT_WORDS};

/// Format tag written into every fixture.
pub const GOLDEN_FORMAT: &str = "spm-cpuref-golden/1";

/// One tile of a fixture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenTile {
    pub t_rows: u32,
    pub t_cols: u32,
    pub transcript: [u32; TRANSCRIPT_WORDS],
    /// Hex of the 32 digest bytes (little-endian integer order).
    pub digest: String,
}

impl From<&TileResult> for GoldenTile {
    fn from(t: &TileResult) -> Self {
        Self {
            t_rows: t.t_rows,
            t_cols: t.t_cols,
            transcript: t.transcript,
            digest: hex::encode(t.digest),
        }
    }
}

/// A problem, its commitment, the first tiles in full and a digest over all tiles.
/// Hashes are lowercase hex of the raw bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Golden {
    pub format: String,
    /// `Problem::generate` seed (A = fill_int7(seed, DOMAIN_A), Bᵀ = fill_int7(seed, DOMAIN_BT)).
    pub seed: Option<u64>,
    pub m: usize,
    pub n: usize,
    pub k: usize,
    pub noise_rank: usize,
    /// `IncompleteBlockHeader::to_bytes()` (76 bytes).
    pub header76: String,
    /// `MiningConfiguration::to_bytes()` (52 bytes).
    pub config52: String,
    pub job_key: String,
    pub root_a: String,
    pub root_b: String,
    pub bound_a: String,
    pub bound_b: String,
    pub b_noise_seed: String,
    pub a_noise_seed: String,
    pub tile_count: usize,
    /// [`tiles_digest`] of every tile in order.
    pub tiles_blake3: String,
    pub first_tiles: Vec<GoldenTile>,
}

impl Golden {
    /// Evaluates every tile of the oracle's problem and keeps the first `first` in full.
    pub fn from_oracle(oracle: &Oracle<'_>, first: usize) -> Result<Self> {
        let p = oracle.problem();
        let c = oracle.commitment();
        let tiles = oracle.transcripts()?;
        Ok(Self {
            format: GOLDEN_FORMAT.to_string(),
            seed: p.seed,
            m: p.m,
            n: p.n,
            k: p.k,
            noise_rank: p.rank(),
            header76: hex::encode(p.header.to_bytes()),
            config52: hex::encode(p.config.to_bytes()),
            job_key: hex::encode(c.job_key),
            root_a: hex::encode(c.root_a),
            root_b: hex::encode(c.root_b),
            bound_a: hex::encode(c.bound_a),
            bound_b: hex::encode(c.bound_b),
            b_noise_seed: hex::encode(c.b_noise_seed),
            a_noise_seed: hex::encode(c.a_noise_seed),
            tile_count: tiles.len(),
            tiles_blake3: hex::encode(tiles_digest(&tiles)),
            first_tiles: tiles.iter().take(first).map(GoldenTile::from).collect(),
        })
    }
}
