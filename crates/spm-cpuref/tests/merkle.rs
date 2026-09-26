//! The Merkle layer cache against `pearl_blake3::MerkleTree` on 1000 random trees: roots (plain
//! and with chunk 0 patched) equal `MerkleTree::root` and `blake3::keyed_hash`, and multileaf
//! proofs (random leaf sets and row-shaped sets) are field-for-field identical to
//! `get_multileaf_proof`.

use pearl_blake3::MerkleTree;
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};
use spm_cpuref::{MatrixTree, SliceSource, CHUNK_BYTES};

fn assert_same(got: &pearl_blake3::MerkleProof, want: &pearl_blake3::MerkleProof, what: &str) {
    assert_eq!(got.leaf_indices, want.leaf_indices, "{what}: leaf indices");
    assert_eq!(got.leaf_data, want.leaf_data, "{what}: leaf data");
    assert_eq!(got.total_leaves, want.total_leaves, "{what}: total leaves");
    assert_eq!(got.root, want.root, "{what}: root");
    assert_eq!(got.siblings, want.siblings, "{what}: siblings");
}

#[test]
fn layer_cache_matches_pearl_blake3_on_1000_random_trees() {
    let mut rng = StdRng::seed_from_u64(0x6d65_726b_6c65);
    for case in 0..1000 {
        let leaves = rng.random_range(2..=192usize);
        let level = rng.random_range(0..=6u32);
        let mut key = [0u8; 32];
        rng.fill_bytes(&mut key);
        let mut bytes = vec![0u8; leaves * CHUNK_BYTES];
        rng.fill_bytes(&mut bytes);
        let tree =
            MatrixTree::build_with_segment_level(key, leaves, &SliceSource(&bytes), level).unwrap();
        let what = format!("case {case}: {leaves} leaves, level {level}");

        // Optionally replace chunk 0, like a nonce patch.
        let patched = rng.random_bool(0.5);
        let mut chunk0 = [0u8; CHUNK_BYTES];
        rng.fill_bytes(&mut chunk0);
        let patch = patched.then(|| tree.patch_chunk0(&chunk0));
        let mut view = bytes.clone();
        if patched {
            view[..CHUNK_BYTES].copy_from_slice(&chunk0);
        }
        let reference = MerkleTree::new(&view, key);
        assert_eq!(tree.root(patch.as_ref()), reference.root(), "{what}: root");
        assert_eq!(
            tree.root(patch.as_ref()),
            *blake3::keyed_hash(&key, &view).as_bytes(),
            "{what}"
        );

        // Random leaf sets.
        for _ in 0..3 {
            let count = rng.random_range(1..=leaves.min(12));
            let set: Vec<usize> = (0..count).map(|_| rng.random_range(0..leaves)).collect();
            let got = tree
                .multileaf_proof(&set, patch.as_ref(), &SliceSource(&bytes))
                .unwrap();
            assert_same(
                &got,
                &reference.get_multileaf_proof(&set),
                &format!("{what} {set:?}"),
            );
            assert!(got.verify(key));
        }
        // Row-shaped sets: 8 rows with stride 8 (a hash tile's A rows) of a matrix whose rows
        // are 1024..=4096 bytes.
        let cols = 1024 * rng.random_range(1..=4usize);
        let rows = leaves * CHUNK_BYTES / cols;
        if rows > 0 {
            let base = rng.random_range(0..rows);
            let row_set: Vec<usize> = (0..8).map(|i| base + 8 * i).filter(|&r| r < rows).collect();
            let set = MerkleTree::compute_leaf_indices_from_rows(&row_set, (rows, cols));
            let got = tree
                .multileaf_proof(&set, patch.as_ref(), &SliceSource(&bytes))
                .unwrap();
            assert_same(
                &got,
                &reference.get_multileaf_proof(&set),
                &format!("{what} rows {row_set:?}"),
            );
        }
    }
}

#[test]
fn out_of_range_and_degenerate_inputs_are_refused() {
    let bytes = vec![1u8; 4 * CHUNK_BYTES];
    assert!(MatrixTree::build([0; 32], 1, &SliceSource(&bytes[..CHUNK_BYTES])).is_err());
    let t = MatrixTree::build([0; 32], 4, &SliceSource(&bytes)).unwrap();
    assert!(t.multileaf_proof(&[], None, &SliceSource(&bytes)).is_err());
    assert!(t.multileaf_proof(&[4], None, &SliceSource(&bytes)).is_err());
}
