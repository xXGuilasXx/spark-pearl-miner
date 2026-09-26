//! spm-pow — thin, audited layer over the official Pearl `zk-pow` crate (ISC, pinned).
//! M0 smoke: re-export the official types so the workspace links the verifier natively.
pub use zk_pow::api::proof::{IncompleteBlockHeader, MiningConfiguration, PeriodicPattern};
pub use zk_pow::api::seed::{bind_root_a, bind_root_b, SeedDerivation};
pub use zk_pow::ffi::plain_proof::{check_cert_version_eligible, CertificateVersion, MatrixMerkleProof, PlainProof};

/// Noise rank mandated since the RankPenalty fork (mainnet height 96,251).
pub const NOISE_RANK: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_crate_links_and_pattern_roundtrips() {
        // Our fragment-aligned 8x16 hash tile: rows [0,8,...,56], cols [0,1,8,9,...,56,57].
        let rows: Vec<usize> = (0..8).map(|i| i * 8).collect();
        let cols: Vec<usize> = (0..8).flat_map(|i| [i * 8, i * 8 + 1]).collect();
        let rp = PeriodicPattern::from_list(&rows);
        let cp = PeriodicPattern::from_list(&cols);
        assert_eq!(rp.to_list(), rows);
        assert_eq!(cp.to_list(), cols);
    }
}
