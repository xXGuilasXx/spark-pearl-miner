//! PlainProofs built by the oracle against the official V3 verifier.

mod common;

use common::*;
use spm_cpuref::*;
use spm_pow::{
    check_cert_version_eligible, check_rank_penalty, extract_difficulty_bound,
    penalized_target_bound, target_from_compact, CertificateVersion, SeedDerivation,
};
use zk_pow::api::verify::verify_plain_proof;

#[test]
fn every_hit_verifies_and_a_non_hit_fails() {
    let hdr = header(EASY_NBITS);
    let mut total_hits = 0;
    for seed in [21u64, 22, 23] {
        let p = Problem::generate(256, 256, 2048, hdr, seed).unwrap();
        let bound = extract_difficulty_bound(EASY_NBITS, &p.config);
        // r = 128 carries no rank penalty: the pool-side penalized bound is the same number.
        assert_eq!(
            penalized_target_bound(target_from_compact(EASY_NBITS), &p.config),
            Some(bound)
        );
        let oracle = Oracle::new(&p).unwrap();
        let tiles = oracle.transcripts().unwrap();
        let hits = oracle.find_hits(bound).unwrap();
        assert_eq!(
            hits,
            tiles
                .iter()
                .filter(|t| t.meets(bound))
                .copied()
                .collect::<Vec<_>>()
        );
        assert!(
            !hits.is_empty() && hits.len() < tiles.len(),
            "seed {seed}: {} hits of {}",
            hits.len(),
            tiles.len()
        );
        assert_eq!(hits, find_hits(&p, bound).unwrap());
        total_hits += hits.len();
        for hit in &hits {
            let proof = oracle.build_plain_proof(hit).unwrap();
            assert_eq!(
                proof.a.row_indices,
                p.row_pattern()
                    .iter()
                    .map(|&d| (hit.t_rows + d) as usize)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                proof.bt.row_indices,
                p.col_pattern()
                    .iter()
                    .map(|&d| (hit.t_cols + d) as usize)
                    .collect::<Vec<_>>()
            );
            assert!(matches!(
                check_cert_version_eligible(3, &proof).unwrap(),
                CertificateVersion::ZkV3
            ));
            verify_plain_proof(&hdr, &proof, Some(EASY_NBITS), SeedDerivation::Salted).unwrap();
            verify_plain_proof(&hdr, &proof, None, SeedDerivation::Salted).unwrap();
            verify_v3(&hdr, &proof, Some(EASY_NBITS)).unwrap();
            check_rank_penalty(&p.config, &hit.digest, EASY_NBITS).unwrap();
            assert_eq!(
                verifier_digest(&hdr, &proof),
                hit.digest,
                "seed {seed} tile ({}, {})",
                hit.t_rows,
                hit.t_cols
            );
            // The free function opens the same rows the same way.
            let again = build_plain_proof(&p, hit).unwrap();
            assert_eq!(
                bincode::serialize(&again).unwrap(),
                bincode::serialize(&proof).unwrap()
            );
        }
        let miss = tiles.iter().find(|t| !t.meets(bound)).unwrap();
        let proof = build_plain_proof(&p, miss).unwrap();
        let err = verify_v3(&hdr, &proof, Some(EASY_NBITS)).unwrap_err();
        assert!(
            format!("{err:#}").contains("Jackpot condition"),
            "wrong failure: {err:#}"
        );
        assert!(check_rank_penalty(&p.config, &miss.digest, EASY_NBITS).is_err());
        // The proof itself is sound: only the difficulty rejects it.
        assert_eq!(verifier_digest(&hdr, &proof), miss.digest);
        verify_v3(&hdr, &proof, Some(SATURATED_NBITS)).unwrap();
    }
    assert!(total_hits >= 30, "only {total_hits} hits over three seeds");
}

#[test]
fn other_shapes_verify_with_matching_digests() {
    let hdr = header(SATURATED_NBITS);
    for (seed, m, n, k) in [
        (31u64, 128usize, 192usize, 4096usize),
        (32, 64, 64, 2112),
        (33, 512, 64, 2048),
        (34, 64, 512, 4096),
    ] {
        let p = Problem::generate(m, n, k, hdr, seed).unwrap();
        let oracle = Oracle::new(&p).unwrap();
        let tiles = oracle.transcripts().unwrap();
        for t in [tiles[0], tiles[tiles.len() / 2], tiles[tiles.len() - 1]] {
            let proof = oracle.build_plain_proof(&t).unwrap();
            verify_v3(&hdr, &proof, None).unwrap();
            assert_eq!(
                verifier_digest(&hdr, &proof),
                t.digest,
                "seed {seed} tile ({}, {})",
                t.t_rows,
                t.t_cols
            );
        }
    }
}

#[test]
fn tampered_proofs_fail() {
    // Saturated difficulty: every tile hits, so only the tampering can make verification fail.
    let hdr = header(SATURATED_NBITS);
    let p = Problem::generate(128, 128, 2048, hdr, 5).unwrap();
    let tile = transcripts(&p).unwrap()[3];
    let proof = build_plain_proof(&p, &tile).unwrap();
    verify_v3(&hdr, &proof, None).unwrap();

    let mut bad = proof.clone();
    bad.a.proof.leaf_data[0][5] ^= 1;
    assert!(verify_v3(&hdr, &bad, None).is_err(), "flipped A byte");
    let mut bad = proof.clone();
    bad.bt.proof.leaf_data[1][1000] ^= 0x80;
    assert!(verify_v3(&hdr, &bad, None).is_err(), "flipped Bᵀ byte");
    let mut bad = proof.clone();
    bad.a.proof.siblings[0][0] ^= 1;
    assert!(verify_v3(&hdr, &bad, None).is_err(), "flipped sibling");
    let mut bad = proof.clone();
    bad.bt.proof.root[31] ^= 1;
    assert!(verify_v3(&hdr, &bad, None).is_err(), "wrong B root");
    let mut bad = proof.clone();
    bad.a.row_indices.iter_mut().for_each(|r| *r += 1);
    assert!(verify_v3(&hdr, &bad, None).is_err(), "shifted rows");
    let mut bad = proof.clone();
    bad.m = 192;
    assert!(verify_v3(&hdr, &bad, None).is_err(), "declared m");
    let mut other = hdr;
    other.timestamp ^= 1;
    assert!(verify_v3(&other, &proof, None).is_err(), "other header");
}

#[test]
fn invalid_problems_are_rejected() {
    let hdr = header(EASY_NBITS);
    assert!(
        Problem::generate(256, 256, 1024, hdr, 1).is_err(),
        "k < 16r"
    );
    assert!(
        Problem::generate(256, 256, 2080, hdr, 1).is_err(),
        "k not a multiple of 64"
    );
    assert!(
        Problem::generate(100, 256, 2048, hdr, 1).is_err(),
        "m not a multiple of the period"
    );
    assert!(Problem::generate(256, 0, 2048, hdr, 1).is_err(), "n = 0");
    let mut p = Problem::generate(64, 64, 2048, hdr, 1).unwrap();
    p.a[17] = 64;
    p.bt[3] = -64;
    p.validate().expect("64 is inside the verifier's [-64, 64]");
    p.a[17] = 65;
    assert!(p.validate().is_err() && commit(&p).is_err() && transcripts(&p).is_err());
    p.a[17] = 0;
    p.bt.pop();
    assert!(p.validate().is_err());

    let p = Problem::generate(64, 64, 2048, hdr, 1).unwrap();
    let oracle = Oracle::new(&p).unwrap();
    for (r, c) in [(8u32, 0u32), (0, 1), (64, 0), (0, 64)] {
        assert!(oracle.tile(r, c).is_err(), "({r}, {c})");
        let fake = TileResult {
            t_rows: r,
            t_cols: c,
            transcript: [0; 16],
            digest: [0; 32],
        };
        assert!(build_plain_proof(&p, &fake).is_err(), "({r}, {c})");
    }
}
