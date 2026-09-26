//! Shared helpers of the integration tests: fixed headers and the tile-by-tile reference path
//! built only from public zk-pow / pearl-blake3 functions.
#![allow(dead_code)]

use pearl_blake3::{pad_to_chunk_boundary, MerkleTree};
use spm_cpuref::{IncompleteBlockHeader, Problem, TileResult};
use zk_pow::api::proof::{PublicProofParams, SeedDerivation};
use zk_pow::api::proof_utils::{compute_jackpot_hash, CompiledPublicParams};
use zk_pow::circuit::chip::compute_jackpot;
use zk_pow::circuit::pearl_noise::{compute_noise, compute_noise_for_indices};

/// ~1/16 of the tiles hit at k = 2048 (bound ≈ 0x3ffff·2^216·2^18 ≈ 2^252).
pub const EASY_NBITS: u32 = 0x1e03_ffff;
/// ~1/128 of the tiles hit at k = 2048 (bound ≈ 2^249); the spm-pow M1 test uses it too.
pub const MEDIUM_NBITS: u32 = 0x1d7f_ffff;
/// ~2^-15 per tile at k = 2048: a 256×256 problem almost never hits.
pub const HARD_NBITS: u32 = 0x1c7f_ffff;
/// target·h·w·k overflows 256 bits, so the consensus bound saturates to U256::MAX.
pub const SATURATED_NBITS: u32 = 0x207f_ffff;

pub fn header(nbits: u32) -> IncompleteBlockHeader {
    IncompleteBlockHeader {
        version: 0x2000_0000,
        prev_block: [1; 32],
        merkle_root: [2; 32],
        timestamp: 0x6666_6666,
        nbits,
    }
}

fn bytes(entries: &[i8]) -> Vec<u8> {
    entries.iter().map(|&x| x as u8).collect()
}

/// Official V3 public params of tile (t_rows, t_cols), with roots from pearl_blake3 trees
/// keyed by the official job key.
pub fn reference_params(p: &Problem, t_rows: u32, t_cols: u32) -> PublicProofParams {
    let (m, n) = (p.m as u32, p.n as u32);
    let job_key = PublicProofParams::new(
        p.header,
        SeedDerivation::Salted,
        p.config,
        [0; 32],
        [0; 32],
        [0; 32],
        m,
        n,
        0,
        0,
    )
    .job_key();
    let root_a = MerkleTree::new(&pad_to_chunk_boundary(&bytes(&p.a)), job_key).root();
    let root_b = MerkleTree::new(&pad_to_chunk_boundary(&bytes(&p.bt)), job_key).root();
    PublicProofParams::new(
        p.header,
        SeedDerivation::Salted,
        p.config,
        root_a,
        root_b,
        [0; 32],
        m,
        n,
        t_rows,
        t_cols,
    )
}

/// Every tile computed with the official pieces only: `compute_noise_for_indices` for the tile's
/// rows/columns, `compute_jackpot` on the raw (un-noised) strips, `compute_jackpot_hash`.
pub fn reference_tiles(p: &Problem) -> Vec<TileResult> {
    let k = p.k;
    let r = p.config.rank as usize;
    let compiled = CompiledPublicParams::from(&reference_params(p, 0, 0));
    let rows_valid: Vec<u32> = (0..p.m as u32)
        .filter(|&i| p.config.rows_pattern.offset_is_valid(i))
        .collect();
    let cols_valid: Vec<u32> = (0..p.n as u32)
        .filter(|&i| p.config.cols_pattern.offset_is_valid(i))
        .collect();
    let mut out = Vec::new();
    for &t_rows in &rows_valid {
        for &t_cols in &cols_valid {
            let a_idx: Vec<usize> = p
                .config
                .rows_pattern
                .indices_with_offset(t_rows)
                .iter()
                .map(|&i| i as usize)
                .collect();
            let b_idx: Vec<usize> = p
                .config
                .cols_pattern
                .indices_with_offset(t_cols)
                .iter()
                .map(|&i| i as usize)
                .collect();
            let s_a: Vec<Vec<i8>> = a_idx
                .iter()
                .map(|&i| p.a[i * k..(i + 1) * k].to_vec())
                .collect();
            let s_b: Vec<Vec<i8>> = b_idx
                .iter()
                .map(|&j| p.bt[j * k..(j + 1) * k].to_vec())
                .collect();
            let noise = compute_noise_for_indices(k, r, compiled.commitment_hash, &a_idx, &b_idx);
            let transcript = compute_jackpot(&compiled, &s_a, &s_b, &noise);
            let digest = compute_jackpot_hash(&transcript, compiled.a_noise_seed());
            out.push(TileResult {
                t_rows,
                t_cols,
                transcript,
                digest,
            });
        }
    }
    out
}

/// The official per-tile noise path (`compute_noise` on the tile's own compiled params) for
/// one tile, returned as (noise rows of A, noise rows of Bᵀ).
pub fn reference_tile_noise(p: &Problem, t_rows: u32, t_cols: u32) -> (Vec<Vec<i8>>, Vec<Vec<i8>>) {
    let compiled = CompiledPublicParams::from(&reference_params(p, t_rows, t_cols));
    let noise = compute_noise(&compiled);
    (noise.a, noise.b)
}

/// The digest exactly as `verify_plain_proof` recomputes it from a PlainProof (parse, official
/// noise for the opened rows, `compute_jackpot`, `compute_jackpot_hash`).
pub fn verifier_digest(
    hdr: &IncompleteBlockHeader,
    proof: &zk_pow::ffi::plain_proof::PlainProof,
) -> [u8; 32] {
    let (private, public) = proof
        .parse_proof(*hdr, SeedDerivation::Salted)
        .expect("proof parses");
    let compiled = CompiledPublicParams::from(&public);
    let noise = compute_noise(&compiled);
    let jackpot = compute_jackpot(&compiled, &private.s_a, &private.s_b, &noise);
    compute_jackpot_hash(&jackpot, compiled.a_noise_seed())
}

/// Row-major flattening of a `Vec<Vec<i8>>` matrix and its transpose.
pub fn flatten(rows: &[Vec<i8>]) -> Vec<i8> {
    rows.concat()
}

pub fn transpose(rows: &[Vec<i8>]) -> Vec<i8> {
    let (r, c) = (rows.len(), rows.first().map_or(0, Vec::len));
    let mut out = Vec::with_capacity(r * c);
    for j in 0..c {
        for row in rows {
            out.push(row[j]);
        }
    }
    out
}
