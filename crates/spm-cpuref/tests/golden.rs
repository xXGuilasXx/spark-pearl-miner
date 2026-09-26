//! Golden fixtures: `tests/fixtures/golden-cpuref-<seed>.json` at the repository root.
//!
//! Regenerate (only after an intentional change, then refresh `tests/fixtures/SHA256SUMS`):
//! `SPM_UPDATE_GOLDEN=1 cargo test --release -p spm-cpuref --test golden`

mod common;

use std::path::PathBuf;

use common::*;
use spm_cpuref::*;

/// (seed, m, n, k) — shapes from the G0 matrix (m, n ∈ {256, 512}, k ∈ {2048, 4096}).
const GOLDEN: &[(u64, usize, usize, usize)] = &[
    (1, 256, 256, 2048),
    (2, 256, 512, 4096),
    (3, 512, 256, 4096),
];
/// Tiles kept in full in each fixture.
const FIRST_TILES: usize = 8;

fn fixture_path(seed: u64) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(format!("golden-cpuref-{seed}.json"))
}

#[test]
fn golden_fixtures_match() {
    let update = std::env::var_os("SPM_UPDATE_GOLDEN").is_some();
    for &(seed, m, n, k) in GOLDEN {
        let p = Problem::generate(m, n, k, header(MEDIUM_NBITS), seed).unwrap();
        let oracle = Oracle::new(&p).unwrap();
        let golden = Golden::from_oracle(&oracle, FIRST_TILES).unwrap();
        let path = fixture_path(seed);
        if update {
            let mut text = serde_json::to_string_pretty(&golden).unwrap();
            text.push('\n');
            std::fs::write(&path, text).unwrap();
        }
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let stored: Golden = serde_json::from_str(&text).unwrap();
        assert_eq!(stored.format, GOLDEN_FORMAT);
        assert_eq!(
            stored,
            golden,
            "{} is stale or the oracle changed",
            path.display()
        );
        // The stored header reproduces the problem's job key through the official path.
        assert_eq!(
            stored.header76,
            hex::encode(header(MEDIUM_NBITS).to_bytes())
        );
        assert_eq!(stored.first_tiles.len(), FIRST_TILES);
    }
}

#[test]
fn golden_tiles_also_match_the_reference_pieces() {
    // Seed 1 in full through the official pieces, so the fixture is anchored to zk-pow and not
    // only to this crate.
    let &(seed, m, n, k) = &GOLDEN[0];
    let p = Problem::generate(m, n, k, header(MEDIUM_NBITS), seed).unwrap();
    let reference = reference_tiles(&p);
    let text = std::fs::read_to_string(fixture_path(seed)).unwrap();
    let stored: Golden = serde_json::from_str(&text).unwrap();
    assert_eq!(stored.tiles_blake3, hex::encode(tiles_digest(&reference)));
    for (g, r) in stored.first_tiles.iter().zip(&reference) {
        assert_eq!(g, &GoldenTile::from(r));
    }
}
