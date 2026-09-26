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

// ---- Difficulty encoding (Bitcoin "compact" nbits) -------------------------------------------
pub use primitive_types::U256;
pub use zk_pow::api::proof::MMAType;
pub use zk_pow::api::proof_utils::nbits_to_difficulty;
pub use zk_pow::api::sanity_checks::{check_rank_penalty, extract_difficulty_bound, penalized_target_bound};

/// Encode a 256-bit target as compact nbits (Bitcoin `BigToCompact`).
pub fn compact_from_target(target: U256) -> u32 {
    if target.is_zero() { return 0; }
    let mut size = (target.bits() + 7) / 8; // bytes needed
    let mut mantissa: u32 = if size <= 3 {
        (target.low_u64() << (8 * (3 - size))) as u32
    } else {
        (target >> (8 * (size - 3))).low_u64() as u32
    };
    if mantissa & 0x0080_0000 != 0 { mantissa >>= 8; size += 1; }
    ((size as u32) << 24) | (mantissa & 0x00ff_ffff)
}

/// Decode compact nbits into a target (thin alias of the official decoder).
pub fn target_from_compact(nbits: u32) -> U256 { nbits_to_difficulty(nbits) }

/// Pool convention: share target = floor(0xFFFF * 2^208 / diff).
pub fn share_target_for_diff(diff: u64) -> U256 { (U256::from(0xFFFFu64) << 208) / U256::from(diff) }

/// Our fixed `MiningConfiguration` for a job with common dimension `k` (V3, dense, no MoE).
pub fn mining_config(k: u32) -> anyhow::Result<MiningConfiguration> {
    let (rows_pattern, cols_pattern) = hash_tile_patterns()?;
    Ok(MiningConfiguration { common_dim: k, rank: NOISE_RANK, mma_type: MMAType::Int7xInt7ToInt32, rows_pattern, cols_pattern, moe: None })
}

/// The GPU compares the jackpot hash against this bound: the pool's target (rounded through
/// compact nbits, as pools verify with `nbits_override`) scaled by the rank-penalized factor.
pub fn share_bound(pool_target: U256, config: &MiningConfiguration) -> Option<U256> {
    let nbits = compact_from_target(pool_target);
    let rounded = target_from_compact(nbits);
    let t = if rounded < pool_target { rounded } else { pool_target };
    penalized_target_bound(t, config)
}

#[cfg(test)]
mod m1_tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};
    use zk_pow::ffi::mine::try_mine_one;
    use zk_pow::api::verify::verify_plain_proof;

    fn header(nbits: u32) -> IncompleteBlockHeader {
        IncompleteBlockHeader { version: 0x2000_0000, prev_block: [1; 32], merkle_root: [2; 32], timestamp: 0x6666_6666, nbits }
    }

    #[test]
    fn config52_layout() {
        let c = mining_config(4096).unwrap();
        let b = c.to_bytes();
        assert_eq!(b.len(), 52);
        assert_eq!(&b[0..4], &4096u32.to_le_bytes());
        assert_eq!(&b[4..6], &128u16.to_le_bytes());
        assert_eq!(&b[6..8], &0u16.to_le_bytes()); // Int7xInt7ToInt32
        assert_eq!(&b[8..14], &[0x07, 0x07, 0, 0, 0, 0]);
        assert_eq!(&b[14..20], &[0x00, 0x01, 0x03, 0x07, 0, 0]);
        assert!(b[20..52].iter().all(|&x| x == 0)); // no MoE trailer
        assert_eq!(MiningConfiguration::from_bytes(&b).unwrap().to_bytes(), b);
    }

    #[test]
    fn nbits_for_pool_diff_2_pow_21() {
        let t = share_target_for_diff(2_097_152);
        assert_eq!(t, U256::from(0x7fff8u64) << 184);
        assert_eq!(compact_from_target(t), 0x1a07_fff8);
        assert_eq!(target_from_compact(0x1a07_fff8), t);
        // Bound for our config at k = 4096 and r = 128: target * h*w*k = target * 2^19.
        let c = mining_config(4096).unwrap();
        assert_eq!(share_bound(t, &c), Some(t << 19));
        assert_eq!(extract_difficulty_bound(0x1a07_fff8, &c), t << 19);
    }

    #[test]
    fn expand_compact_never_exceeds_target() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..2000 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17; // xorshift
            let shift = (x % 200) as usize;
            let t = (U256::from(x | 1) << shift) + U256::from(x >> 3);
            let e = target_from_compact(compact_from_target(t));
            assert!(e <= t, "expand(compact(t)) must not exceed t");
            // compact keeps 23-24 significant bits, or only 16 when the mantissa's sign bit forces a shift.
            assert!(t - e <= (t >> 15), "rounding loss must stay below t / 2^15");
        }
    }

    #[test]
    fn overflowing_bound_is_refused_not_saturated() {
        let c = mining_config(4096).unwrap();
        assert_eq!(penalized_target_bound(U256::MAX >> 4, &c), None);
    }

    #[test]
    fn reference_miner_proof_with_our_pattern_verifies_and_mutations_fail() {
        // Small shape, easy target: m = n = 256, k = 2048, r = 128, nbits 0x1d7fffff (bound ~2^249).
        let (m, n, k) = (256usize, 256usize, 2048usize);
        let cfg = mining_config(k as u32).unwrap();
        let hdr = header(0x1d7f_ffff);
        let mut found = None;
        for seed in 0..40u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            if let Some(p) = try_mine_one(&mut rng, m, n, k, hdr, cfg, None, false, SeedDerivation::Salted).unwrap() {
                found = Some(p); break;
            }
        }
        let proof = found.expect("the reference miner should find a share at this easy target");
        assert_eq!(proof.noise_rank, 128);
        assert!(matches!(check_cert_version_eligible(3, &proof).unwrap(), CertificateVersion::ZkV3));
        verify_plain_proof(&hdr, &proof, None, SeedDerivation::Salted).expect("official verifier accepts our pattern");
        // The share also verifies at an explicit (equal) pool nbits override.
        verify_plain_proof(&hdr, &proof, Some(0x1d7f_ffff), SeedDerivation::Salted).unwrap();
        // Mutations must be rejected.
        let mut bad = proof.clone(); bad.noise_rank = 256;
        assert!(verify_plain_proof(&hdr, &bad, None, SeedDerivation::Salted).is_err());
        let mut bad = proof.clone(); bad.m = 255;
        assert!(verify_plain_proof(&hdr, &bad, None, SeedDerivation::Salted).is_err());
        let wrong_hdr = header(0x1d7f_fffe);
        assert!(verify_plain_proof(&wrong_hdr, &proof, None, SeedDerivation::Salted).is_err());
        // Legacy (unsalted) derivation must not accept a V3 proof.
        assert!(verify_plain_proof(&hdr, &proof, None, SeedDerivation::Legacy).is_err());
    }
}
