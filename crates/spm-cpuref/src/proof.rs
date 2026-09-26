//! PlainProof construction and V3 verification.

use anyhow::{ensure, Result};
use pearl_blake3::{pad_to_chunk_boundary, MerkleTree};
use spm_pow::{
    check_cert_version_eligible, CertificateVersion, IncompleteBlockHeader, SeedDerivation,
};
use zk_pow::api::verify::verify_plain_proof;
use zk_pow::ffi::plain_proof::{MatrixMerkleProof, PlainProof};

use crate::commit::{job_key, matrix_bytes, Hash256};
use crate::oracle::TileResult;
use crate::problem::Problem;

/// Multileaf Merkle opening of `row_indices` of a `rows`×`cols` row-major matrix, built exactly
/// like the reference miner: pad to 1024 bytes, keyed tree, leaves covering the rows.
pub fn matrix_proof(
    entries: &[i8],
    rows: usize,
    cols: usize,
    key: &Hash256,
    row_indices: &[usize],
) -> Result<MatrixMerkleProof> {
    ensure!(
        Some(entries.len()) == rows.checked_mul(cols),
        "matrix size mismatch"
    );
    ensure!(!row_indices.is_empty(), "no rows to open");
    ensure!(
        row_indices.windows(2).all(|w| w[0] < w[1]),
        "row indices must be strictly ascending"
    );
    ensure!(
        row_indices.iter().all(|&r| r < rows),
        "row index out of range"
    );
    let padded = pad_to_chunk_boundary(&matrix_bytes(entries));
    let tree = MerkleTree::new(&padded, *key);
    let leaves = MerkleTree::compute_leaf_indices_from_rows(row_indices, (rows, cols));
    ensure!(
        leaves.last().is_some_and(|&l| l < tree.num_leaves()),
        "leaf index out of range"
    );
    Ok(MatrixMerkleProof {
        proof: tree.get_multileaf_proof(&leaves),
        row_indices: row_indices.to_vec(),
    })
}

pub(crate) fn build_plain_proof_with_key(
    p: &Problem,
    key: &Hash256,
    t_rows: u32,
    t_cols: u32,
) -> Result<PlainProof> {
    ensure!(
        p.is_tile(t_rows, t_cols),
        "({t_rows}, {t_cols}) is not a hash-tile base of this problem"
    );
    let a_rows: Vec<usize> = p
        .row_pattern()
        .iter()
        .map(|&d| (t_rows + d) as usize)
        .collect();
    let bt_rows: Vec<usize> = p
        .col_pattern()
        .iter()
        .map(|&d| (t_cols + d) as usize)
        .collect();
    Ok(PlainProof {
        m: p.m,
        n: p.n,
        k: p.k,
        noise_rank: p.rank(),
        a: matrix_proof(&p.a, p.m, p.k, key, &a_rows)?,
        bt: matrix_proof(&p.bt, p.n, p.k, key, &bt_rows)?,
        moe: None,
    })
}

/// PlainProof for the tile at `(tile.t_rows, tile.t_cols)`: the h rows of A and the w rows of
/// Bᵀ of the tile, each with a pearl_blake3 multileaf proof keyed by the job key. Whether the
/// tile is a hit is not checked here (a non-hit makes a structurally valid proof that fails the
/// difficulty check).
pub fn build_plain_proof(p: &Problem, tile: &TileResult) -> Result<PlainProof> {
    p.validate()?;
    build_plain_proof_with_key(p, &job_key(&p.header, &p.config), tile.t_rows, tile.t_cols)
}

/// What a V3 pool or node runs on a share: `check_cert_version_eligible(3)` then
/// `verify_plain_proof(header, proof, nbits, Salted)`.
pub fn verify_v3(
    header: &IncompleteBlockHeader,
    proof: &PlainProof,
    nbits: Option<u32>,
) -> Result<()> {
    let version = check_cert_version_eligible(3, proof)?;
    ensure!(
        version == CertificateVersion::ZkV3,
        "unexpected certificate version {version:?}"
    );
    ensure!(
        version.seed_derivation() == SeedDerivation::Salted,
        "V3 must use the salted derivation"
    );
    verify_plain_proof(header, proof, nbits, SeedDerivation::Salted)
}
