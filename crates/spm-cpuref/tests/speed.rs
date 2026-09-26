//! Smallest consensus-valid shape end to end, with a time budget in release builds.
//!
//! Minimums for our configuration: r = 128 forces k ≥ 16r = 2048 (and k ≥ 1024, k % 64 = 0);
//! the 8×16 pattern has period 64 in both dimensions, so m = n = 64 is the smallest problem
//! (32 tiles).

mod common;

use std::time::{Duration, Instant};

use common::*;
use spm_cpuref::*;

#[test]
fn smallest_shape_runs_end_to_end_quickly() {
    let start = Instant::now();
    let hdr = header(SATURATED_NBITS);
    for seed in 0..8u64 {
        let p = Problem::generate(64, 64, 2048, hdr, seed).unwrap();
        let oracle = Oracle::new(&p).unwrap();
        let tiles = oracle.transcripts().unwrap();
        assert_eq!(tiles.len(), 32);
        assert_eq!(
            first_mismatch(&reference_tiles(&p), &tiles),
            None,
            "seed {seed}"
        );
        let hits = oracle.find_hits(U256::MAX).unwrap();
        assert_eq!(hits.len(), 32);
        let proof = oracle.build_plain_proof(&hits[seed as usize]).unwrap();
        verify_v3(&hdr, &proof, None).unwrap();
    }
    let elapsed = start.elapsed();
    if !cfg!(debug_assertions) {
        assert!(
            elapsed < Duration::from_secs(10),
            "small-shape pipeline took {elapsed:?}"
        );
    }
}

#[test]
fn g0_largest_shape_is_practical() {
    // The largest G0 shape (1024×1024×4096, 8192 tiles) must stay cheap enough to be the oracle
    // of the GPU debug-dump comparison.
    if cfg!(debug_assertions) {
        return;
    }
    let start = Instant::now();
    let p = Problem::generate(1024, 1024, 4096, header(MEDIUM_NBITS), 9).unwrap();
    let tiles = transcripts(&p).unwrap();
    assert_eq!(tiles.len(), 8192);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "1024×1024×4096 took {elapsed:?}"
    );
}
