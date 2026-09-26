//! spm-pow — thin, audited layer over the official Pearl `zk-pow` crate (ISC, pinned at 3fe2267).
//! M0 smoke: re-export the official types so the workspace links the verifier natively, and
//! pin the byte encoding of our fragment-aligned hash-tile pattern.
pub use zk_pow::api::proof::{IncompleteBlockHeader, MiningConfiguration, PeriodicPattern};
pub use zk_pow::api::seed::{bind_root_a, bind_root_b, SeedDerivation};
pub use zk_pow::ffi::plain_proof::{check_cert_version_eligible, CertificateVersion, MatrixMerkleProof, PlainProof};

/// Noise rank mandated since the RankPenalty fork (mainnet height 96,251); r = 128 is also the
/// only rank with no penalty, so the miner never uses anything else.
pub const NOISE_RANK: u16 = 128;

/// Row offsets of our hash tile: one row per 8 (rows 0,8,...,56 of a 64-row warp tile).
pub const HASH_ROWS: [u32; 8] = [0, 8, 16, 24, 32, 40, 48, 56];
/// Column offsets of our hash tile: the (2c, 2c+1) pairs of each mma.sync m16n8 fragment,
/// one pair per 8 columns (0,1,8,9,...,56,57) — 8 x 16 = 128 elements per attempt.
pub const HASH_COLS: [u32; 16] = [0, 1, 8, 9, 16, 17, 24, 25, 32, 33, 40, 41, 48, 49, 56, 57];

/// The two periodic patterns committed in every job's `MiningConfiguration`.
pub fn hash_tile_patterns() -> anyhow::Result<(PeriodicPattern, PeriodicPattern)> {
    Ok((PeriodicPattern::from_list(&HASH_ROWS)?, PeriodicPattern::from_list(&HASH_COLS)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_crate_links_and_pattern_roundtrips() {
        let (rp, cp) = hash_tile_patterns().expect("valid patterns");
        assert_eq!(rp.to_list(), HASH_ROWS.to_vec());
        assert_eq!(cp.to_list(), HASH_COLS.to_vec());
    }

    #[test]
    fn pattern_bytes_match_design_review() {
        // Asserted by the design judges against zk-pow proof_utils.rs::to_bytes.
        let (rp, cp) = hash_tile_patterns().unwrap();
        assert_eq!(rp.to_bytes(), [0x07, 0x07, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(cp.to_bytes(), [0x00, 0x01, 0x03, 0x07, 0x00, 0x00]);
    }

    #[test]
    fn patterns_partition_a_512x512_output_exactly_once() {
        // Valid base offsets (t_rows, t_cols) must tile the output with no overlap and no gap.
        let (rp, cp) = hash_tile_patterns().unwrap();
        let (m, n) = (512u32, 512u32);
        let mut cover = vec![0u8; (m * n) as usize];
        for tr in 0..m {
            if !rp.offset_is_valid(tr) { continue; }
            for tc in 0..n {
                if !cp.offset_is_valid(tc) { continue; }
                for r in HASH_ROWS { for c in HASH_COLS {
                    let (rr, cc) = (tr + r, tc + c);
                    if rr < m && cc < n { cover[(rr * n + cc) as usize] += 1; }
                }}
            }
        }
        assert!(cover.iter().all(|&x| x == 1), "every output element must belong to exactly one hash tile");
    }
}
