//! Job key, Merkle roots and the certificate-V3 noise-seed chain.

use anyhow::{Context, Result};
use pearl_blake3::{blake3_digest, pad_to_chunk_boundary};
use serde::{Deserialize, Serialize};
use spm_pow::{bind_root_a, bind_root_b, IncompleteBlockHeader, MiningConfiguration};

use crate::problem::Problem;

pub type Hash256 = [u8; 32];

/// Everything a job commits to before the first multiplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Commitment {
    /// `blake3(header76 || config52)`, unkeyed.
    pub job_key: Hash256,
    /// `blake3_keyed(job_key, pad1024(A as bytes))` = root of the pearl_blake3 Merkle tree.
    pub root_a: Hash256,
    /// `blake3_keyed(job_key, pad1024(Bᵀ as bytes))`.
    pub root_b: Hash256,
    /// V3 salted root of A: `bind_root_a(root_a, m)`.
    pub bound_a: Hash256,
    /// V3 salted root of B: `bind_root_b(root_b, n)`.
    pub bound_b: Hash256,
    /// `blake3(job_key || bound_b)`.
    pub b_noise_seed: Hash256,
    /// `blake3(b_noise_seed || bound_a)`; also the key of the jackpot hash.
    pub a_noise_seed: Hash256,
}

/// `blake3(header.to_bytes() || config.to_bytes())` (76 + 52 = 128 bytes, unkeyed).
pub fn job_key(header: &IncompleteBlockHeader, config: &MiningConfiguration) -> Hash256 {
    let mut msg = [0u8; 128];
    msg[..76].copy_from_slice(&header.to_bytes());
    msg[76..].copy_from_slice(&config.to_bytes());
    blake3_digest(&msg, None)
}

/// Two's-complement bytes of the entries, in order (what the Merkle tree hashes).
pub fn matrix_bytes(entries: &[i8]) -> Vec<u8> {
    entries.iter().map(|&x| x as u8).collect()
}

/// Keyed BLAKE3 of the entries zero-padded to a 1024-byte chunk boundary. Equal to
/// `pearl_blake3::MerkleTree::new(padded, job_key).root()`.
pub fn matrix_root(job_key: &Hash256, entries: &[i8]) -> Hash256 {
    blake3_digest(
        &pad_to_chunk_boundary(&matrix_bytes(entries)),
        Some(*job_key),
    )
}

/// The seed chain shared by Legacy and Salted derivations, fed with already-bound roots:
/// returns `(b_noise_seed, a_noise_seed)`.
pub fn seed_chain(job_key: &Hash256, bound_a: &Hash256, bound_b: &Hash256) -> (Hash256, Hash256) {
    let mut msg = [0u8; 64];
    msg[..32].copy_from_slice(job_key);
    msg[32..].copy_from_slice(bound_b);
    let b_noise_seed = blake3_digest(&msg, None);
    msg[..32].copy_from_slice(&b_noise_seed);
    msg[32..].copy_from_slice(bound_a);
    let a_noise_seed = blake3_digest(&msg, None);
    (b_noise_seed, a_noise_seed)
}

/// Commitment of a validated problem.
pub(crate) fn commit_validated(p: &Problem) -> Result<Commitment> {
    let m = u32::try_from(p.m).context("m does not fit in u32")?;
    let n = u32::try_from(p.n).context("n does not fit in u32")?;
    let job_key = job_key(&p.header, &p.config);
    let root_a = matrix_root(&job_key, &p.a);
    let root_b = matrix_root(&job_key, &p.bt);
    let bound_a = bind_root_a(&root_a, m);
    let bound_b = bind_root_b(&root_b, n);
    let (b_noise_seed, a_noise_seed) = seed_chain(&job_key, &bound_a, &bound_b);
    Ok(Commitment {
        job_key,
        root_a,
        root_b,
        bound_a,
        bound_b,
        b_noise_seed,
        a_noise_seed,
    })
}

/// Validates `p` and computes its V3 commitment.
pub fn commit(p: &Problem) -> Result<Commitment> {
    p.validate()?;
    commit_validated(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned vectors of zk-pow `api/seed.rs::commitment_hash_pinned_vectors` (Salted case:
    /// job_key 0x11.., hash_a 0xAA.., hash_b 0xBB.., m = 192, n = 320).
    #[test]
    fn seed_chain_matches_zk_pow_pinned_vector() {
        let job_key = [0x11u8; 32];
        let bound_a = bind_root_a(&[0xAA; 32], 192);
        let bound_b = bind_root_b(&[0xBB; 32], 320);
        let (b, a) = seed_chain(&job_key, &bound_a, &bound_b);
        assert_eq!(
            hex::encode(b),
            "60ed9b73c5a9599b200b6cd563e7f0d5d9a67d2402d85fd4ef966c580080d0e5"
        );
        assert_eq!(
            hex::encode(a),
            "301784168005ec833ab0aa60006f7fe7faaa95307d8c1fc6819b2ffdd717eccf"
        );
        // Legacy chain (roots used as is).
        let (b, a) = seed_chain(&job_key, &[0xAA; 32], &[0xBB; 32]);
        assert_eq!(
            hex::encode(b),
            "add6f7ea5feebf89c8a77e2ebfa0d82442e7dbb0046dbd48971861d12fcb0177"
        );
        assert_eq!(
            hex::encode(a),
            "483b07b6f73105030b9482255f37723f3fed69ae916724ee8291848b8c28794b"
        );
    }

    #[test]
    fn matrix_root_equals_merkle_tree_root() {
        let key = [0x5au8; 32];
        for len in [1usize, 1000, 1024, 1025, 4096, 5000] {
            let entries: Vec<i8> = (0..len).map(|i| (i % 128) as i8 - 64).collect();
            let padded = pad_to_chunk_boundary(&matrix_bytes(&entries));
            assert_eq!(
                matrix_root(&key, &entries),
                pearl_blake3::MerkleTree::new(&padded, key).root(),
                "len {len}"
            );
        }
    }
}
