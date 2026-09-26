//! spm-cpuref — the exact CPU oracle of the PearlHash certificate-V3 pipeline.
//!
//! Given a header, our mining configuration and the int7 matrices A (m×k) and Bᵀ (n×k), it
//! computes what the GPU kernel must reproduce bit for bit: the commitment (job key, Merkle
//! roots, salted roots, noise seeds), the rank-r noise, the noised int8 operands, the exact i32
//! GEMM, the 16-word transcript and jackpot digest of every hash tile, and the PlainProof of any
//! tile. The consensus pieces (noise generators, jackpot hash, Merkle trees, V3 salting,
//! verifier) are the official `zk-pow` / `pearl-blake3` functions; only the glue the GPU has to
//! replicate is written here, and the tests prove it equal to the reference. See README.md for
//! the algorithm in full.
#![forbid(unsafe_code)]

mod commit;
mod golden;
mod noise;
mod oracle;
mod problem;
mod proof;

pub use commit::{commit, job_key, matrix_bytes, matrix_root, seed_chain, Commitment, Hash256};
pub use golden::{Golden, GoldenTile, GOLDEN_FORMAT};
pub use noise::{
    add_noise, noise_factors, Noise, NoiseFactors, NOISE_ABS_MAX, SEED_LABEL_A, SEED_LABEL_B,
    UNIFORM_MAX, UNIFORM_MIN,
};
pub use oracle::{
    first_mismatch, tiles_digest, Oracle, TileResult, TileTrace, TRANSCRIPT_ROTL, TRANSCRIPT_WORDS,
};
pub use problem::{
    fill_int7, Problem, SplitMix64, DOMAIN_A, DOMAIN_BT, GEN_MAX, GEN_MIN, SIGNAL_MAX, SIGNAL_MIN,
};
pub use proof::{build_plain_proof, matrix_proof, verify_v3};

pub use spm_pow::{IncompleteBlockHeader, MiningConfiguration, PlainProof, U256};

use anyhow::Result;

/// Transcript and digest of every hash tile of `p`, in the reference miner's order.
pub fn transcripts(p: &Problem) -> Result<Vec<TileResult>> {
    Oracle::new(p)?.transcripts()
}

/// The tiles of `p` whose digest, read as a little-endian U256, is ≤ `bound`.
pub fn find_hits(p: &Problem, bound: U256) -> Result<Vec<TileResult>> {
    Oracle::new(p)?.find_hits(bound)
}
