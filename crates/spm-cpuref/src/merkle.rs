//! Merkle layer cache of a matrix commitment, with an overlay for chunk 0 (the nonce patch).
//!
//! The tree is exactly `pearl_blake3::MerkleTree` (keyed BLAKE3, 1024-byte leaves = BLAKE3
//! chunks, layers combined pairwise with the odd node promoted, a root-flagged merge of the last
//! two nodes), so its root is `blake3::keyed_hash(key, data)` and its multileaf proofs are
//! byte-identical to `MerkleTree::get_multileaf_proof`. What differs is what is kept:
//!
//! * only the nodes at and above the **segment level** `s` (subtrees of `2^s` chunks, 64 KiB by
//!   default) are stored; each segment node is one `hazmat` subtree hash over the regenerated
//!   bytes, so building never holds the matrix in memory;
//! * segment 0 is kept in full (its leaves and parents), so replacing chunk 0 costs one chunk
//!   hash plus one parent per level ([`MatrixTree::patch_chunk0`]): ~19 merges at 2^19 leaves;
//! * nodes below the segment level of any other segment are recomputed from the regenerated
//!   segment when a proof needs them (a hash tile opens 8 or 16 rows, a handful of segments).
//!
//! The data comes from a [`ChunkSource`] (a generator that can write any chunk range), so a
//! 512 MiB operand costs 64 KiB of scratch per thread and ~0.5 MiB of stored nodes.

use std::collections::{BTreeSet, HashMap};

use anyhow::{ensure, Context, Result};
use blake3::hazmat::{merge_subtrees_non_root, merge_subtrees_root, HasherExt, Mode};
use pearl_blake3::MerkleProof;
use rayon::prelude::*;

use crate::commit::Hash256;

/// Bytes per leaf (a BLAKE3 chunk).
pub const CHUNK_BYTES: usize = blake3::CHUNK_LEN;
/// Default segment level: segments of 64 chunks (64 KiB).
pub const DEFAULT_SEGMENT_LEVEL: u32 = 6;

/// The committed bytes of a matrix, generated on demand: `fill(first_chunk, out)` writes the
/// `out.len()` bytes that start at byte `first_chunk * 1024`.
pub trait ChunkSource: Sync {
    fn fill(&self, first_chunk: usize, out: &mut [u8]);
}

impl<F: Fn(usize, &mut [u8]) + Sync> ChunkSource for F {
    fn fill(&self, first_chunk: usize, out: &mut [u8]) {
        self(first_chunk, out)
    }
}

/// A byte slice as a source (tests, small matrices).
pub struct SliceSource<'a>(pub &'a [u8]);

impl ChunkSource for SliceSource<'_> {
    fn fill(&self, first_chunk: usize, out: &mut [u8]) {
        let start = first_chunk * CHUNK_BYTES;
        out.copy_from_slice(&self.0[start..start + out.len()]);
    }
}

/// Chunk 0 replaced by other bytes: the chaining values on its path to the root and the new root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkPatch {
    data: Box<[u8; CHUNK_BYTES]>,
    /// `path[l]` is node (l, 0), for l = 0 (the leaf) up to the top level.
    path: Vec<Hash256>,
    root: Hash256,
}

impl ChunkPatch {
    /// The replacement bytes of chunk 0.
    pub fn data(&self) -> &[u8; CHUNK_BYTES] {
        &self.data
    }

    /// Root of the patched tree.
    pub fn root(&self) -> Hash256 {
        self.root
    }
}

/// Stored layers of one matrix tree (see the module docs).
#[derive(Clone, Debug)]
pub struct MatrixTree {
    key: Hash256,
    leaves: usize,
    seg_level: u32,
    /// Levels `0..seg_level` of segment 0 (level l holds `2^(seg_level - l)` nodes).
    seg0: Vec<Vec<Hash256>>,
    /// Levels `seg_level..=top` (the top level holds exactly two nodes).
    upper: Vec<Vec<Hash256>>,
    root: Hash256,
}

fn mode(key: &Hash256) -> Mode<'_> {
    Mode::KeyedHash(key)
}

/// Keyed chaining value of the subtree made of `bytes` (whole chunks, a complete subtree or the
/// right edge of the tree) starting at chunk `first_chunk`. For one chunk this is its leaf CV.
pub fn subtree_cv(key: &Hash256, first_chunk: usize, bytes: &[u8]) -> Hash256 {
    let mut h = blake3::Hasher::new_keyed(key);
    h.set_input_offset((first_chunk * CHUNK_BYTES) as u64);
    h.update(bytes);
    h.finalize_non_root()
}

/// One layer up: pairs merged, an odd last node promoted (`Blake3Hasher::combine_layer`).
fn combine(key: &Hash256, prev: &[Hash256]) -> Vec<Hash256> {
    prev.chunks(2)
        .map(|p| match p {
            [l, r] => merge_subtrees_non_root(l, r, mode(key)),
            [x] => *x,
            _ => unreachable!("chunks(2) yields one or two nodes"),
        })
        .collect()
}

/// Leaves and parents of a segment starting at chunk `first_chunk` (levels until one node).
fn segment_layers(key: &Hash256, first_chunk: usize, bytes: &[u8]) -> Vec<Vec<Hash256>> {
    let leaves: Vec<Hash256> = bytes
        .chunks(CHUNK_BYTES)
        .enumerate()
        .map(|(i, c)| subtree_cv(key, first_chunk + i, c))
        .collect();
    let mut layers = vec![leaves];
    while layers.last().is_some_and(|l| l.len() > 1) {
        let next = combine(key, layers.last().expect("non-empty"));
        layers.push(next);
    }
    layers
}

impl MatrixTree {
    /// Builds the layers of a `leaves`-chunk matrix (≥ 2 chunks) with the default segment level.
    pub fn build(key: Hash256, leaves: usize, src: &dyn ChunkSource) -> Result<Self> {
        Self::build_with_segment_level(key, leaves, src, DEFAULT_SEGMENT_LEVEL)
    }

    /// Same with an explicit maximum segment level (tests exercise 0 ..= 6). The level used is
    /// lowered when needed so that the tree has at least two segments.
    pub fn build_with_segment_level(
        key: Hash256,
        leaves: usize,
        src: &dyn ChunkSource,
        max_seg_level: u32,
    ) -> Result<Self> {
        ensure!(
            leaves >= 2,
            "a matrix tree needs at least two chunks (got {leaves})"
        );
        let seg_level = max_seg_level.min((leaves - 1).ilog2());
        let seg_chunks = 1usize << seg_level;
        let segments = leaves.div_ceil(seg_chunks);
        let nodes: Vec<Hash256> = (0..segments)
            .into_par_iter()
            .map_init(
                || vec![0u8; seg_chunks * CHUNK_BYTES],
                |buf, j| {
                    let first = j * seg_chunks;
                    let count = seg_chunks.min(leaves - first);
                    let bytes = &mut buf[..count * CHUNK_BYTES];
                    src.fill(first, bytes);
                    subtree_cv(&key, first, bytes)
                },
            )
            .collect();
        let mut seg0 = {
            let mut buf = vec![0u8; seg_chunks * CHUNK_BYTES];
            src.fill(0, &mut buf);
            segment_layers(&key, 0, &buf)
        };
        ensure!(
            seg0.len() == seg_level as usize + 1 && seg0[seg_level as usize] == [nodes[0]],
            "segment 0 disagrees with its subtree hash"
        );
        seg0.truncate(seg_level as usize);
        let mut upper = vec![nodes];
        while upper.last().is_some_and(|l| l.len() > 2) {
            let next = combine(&key, upper.last().expect("non-empty"));
            upper.push(next);
        }
        let top = upper.last().expect("non-empty");
        ensure!(top.len() == 2, "top level must hold two nodes");
        let root = *merge_subtrees_root(&top[0], &top[1], mode(&key)).as_bytes();
        Ok(Self {
            key,
            leaves,
            seg_level,
            seg0,
            upper,
            root,
        })
    }

    pub fn key(&self) -> &Hash256 {
        &self.key
    }

    /// Number of leaves (chunks).
    pub fn num_leaves(&self) -> usize {
        self.leaves
    }

    /// Segment level in use.
    pub fn segment_level(&self) -> u32 {
        self.seg_level
    }

    /// Level holding the last two nodes (the root merges them).
    pub fn top_level(&self) -> usize {
        self.seg_level as usize + self.upper.len() - 1
    }

    /// Nodes on level `l`.
    fn level_len(&self, l: usize) -> usize {
        self.leaves.div_ceil(1usize << l)
    }

    /// Chaining values stored by the cache.
    pub fn stored_nodes(&self) -> usize {
        self.seg0.iter().chain(&self.upper).map(Vec::len).sum()
    }

    /// Root of the tree, or of the patched tree.
    pub fn root(&self, patch: Option<&ChunkPatch>) -> Hash256 {
        patch.map_or(self.root, |p| p.root)
    }

    /// Replaces chunk 0: one leaf hash plus one merge per level up to the root.
    pub fn patch_chunk0(&self, data: &[u8; CHUNK_BYTES]) -> ChunkPatch {
        let top = self.top_level();
        let mut path = Vec::with_capacity(top + 1);
        let mut cv = subtree_cv(&self.key, 0, data);
        path.push(cv);
        for l in 0..top {
            let sibling = self
                .stored_node(l, 1)
                .expect("node (l, 1) is stored for l <= top");
            cv = merge_subtrees_non_root(&cv, &sibling, mode(&self.key));
            path.push(cv);
        }
        let right = self.stored_node(top, 1).expect("top level holds two nodes");
        let root = *merge_subtrees_root(&cv, &right, mode(&self.key)).as_bytes();
        ChunkPatch {
            data: Box::new(*data),
            path,
            root,
        }
    }

    /// Node (l, idx) when it is stored (upper levels, or segment 0 below them).
    fn stored_node(&self, l: usize, idx: usize) -> Option<Hash256> {
        let s = self.seg_level as usize;
        if l >= s {
            return self.upper.get(l - s)?.get(idx).copied();
        }
        if idx >> (s - l) == 0 {
            return self.seg0.get(l)?.get(idx).copied();
        }
        None
    }

    /// Node (l, idx) of the (patched) tree; segments below the stored levels are regenerated and
    /// memoized in `cache` (segment index → its local layers).
    fn node(
        &self,
        l: usize,
        idx: usize,
        patch: Option<&ChunkPatch>,
        src: &dyn ChunkSource,
        cache: &mut HashMap<usize, Vec<Vec<Hash256>>>,
    ) -> Result<Hash256> {
        ensure!(idx < self.level_len(l), "node ({l}, {idx}) out of range");
        if let (Some(p), 0) = (patch, idx) {
            return p.path.get(l).copied().context("level above the patch path");
        }
        if let Some(v) = self.stored_node(l, idx) {
            return Ok(v);
        }
        let s = self.seg_level as usize;
        let seg = idx >> (s - l);
        let layers = cache.entry(seg).or_insert_with(|| {
            let first = seg << s;
            let count = (1usize << s).min(self.leaves - first);
            let mut buf = vec![0u8; count * CHUNK_BYTES];
            src.fill(first, &mut buf);
            segment_layers(&self.key, first, &buf)
        });
        let layer = &layers[l.min(layers.len() - 1)];
        layer
            .get(idx - (seg << (s - l)))
            .copied()
            .context("segment node out of range")
    }

    /// Bytes of chunk `i` of the (patched) matrix.
    pub fn chunk(
        &self,
        i: usize,
        patch: Option<&ChunkPatch>,
        src: &dyn ChunkSource,
    ) -> [u8; CHUNK_BYTES] {
        match (patch, i) {
            (Some(p), 0) => *p.data,
            _ => {
                let mut out = [0u8; CHUNK_BYTES];
                src.fill(i, &mut out);
                out
            }
        }
    }

    /// Multileaf proof of `leaf_indices`, identical to `pearl_blake3::MerkleTree::get_multileaf_proof`
    /// on the (patched) bytes.
    pub fn multileaf_proof(
        &self,
        leaf_indices: &[usize],
        patch: Option<&ChunkPatch>,
        src: &dyn ChunkSource,
    ) -> Result<MerkleProof> {
        ensure!(!leaf_indices.is_empty(), "no leaves to open");
        let unique: BTreeSet<usize> = leaf_indices.iter().copied().collect();
        let sorted: Vec<usize> = unique.iter().copied().collect();
        ensure!(
            sorted.last().is_some_and(|&i| i < self.leaves),
            "leaf index out of range"
        );
        let leaf_data = sorted.iter().map(|&i| self.chunk(i, patch, src)).collect();
        let mut cache = HashMap::new();
        let mut siblings = Vec::new();
        let mut current = unique;
        let mut level_len = self.leaves;
        let mut l = 0;
        while level_len > 1 && !current.is_empty() {
            for &i in &current {
                if i % 2 == 1 {
                    if !current.contains(&(i - 1)) {
                        siblings.push(self.node(l, i - 1, patch, src, &mut cache)?);
                    }
                } else if !current.contains(&(i + 1)) && i + 1 < level_len {
                    siblings.push(self.node(l, i + 1, patch, src, &mut cache)?);
                }
            }
            current = current.iter().map(|&i| i / 2).collect();
            level_len = level_len.div_ceil(2);
            l += 1;
        }
        Ok(MerkleProof {
            leaf_data,
            leaf_indices: sorted,
            total_leaves: self.leaves,
            root: self.root(patch),
            siblings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_blake3::MerkleTree;

    fn data(len: usize, salt: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt) ^ (i >> 10) as u8)
            .collect()
    }

    #[test]
    fn root_and_proofs_match_pearl_blake3_for_small_trees() {
        let key = [7u8; 32];
        for leaves in [2usize, 3, 4, 5, 7, 8, 9, 64, 65, 127, 128, 130, 200] {
            let bytes = data(leaves * CHUNK_BYTES, leaves as u8);
            let reference = MerkleTree::new(&bytes, key);
            for level in [0, 1, 3, 6] {
                let t =
                    MatrixTree::build_with_segment_level(key, leaves, &SliceSource(&bytes), level)
                        .unwrap();
                assert_eq!(
                    t.root(None),
                    reference.root(),
                    "{leaves} leaves, level {level}"
                );
                assert_eq!(t.root(None), *blake3::keyed_hash(&key, &bytes).as_bytes());
                for set in [
                    vec![0],
                    vec![leaves - 1],
                    vec![1, leaves / 2],
                    vec![0, 2.min(leaves - 1), leaves - 2],
                ] {
                    let got = t.multileaf_proof(&set, None, &SliceSource(&bytes)).unwrap();
                    let want = reference.get_multileaf_proof(&set);
                    assert_eq!(got.siblings, want.siblings, "{leaves} leaves {set:?}");
                    assert_eq!(got.leaf_indices, want.leaf_indices);
                    assert_eq!(got.leaf_data, want.leaf_data);
                    assert_eq!(got.root, want.root);
                    assert!(got.verify(key));
                }
            }
        }
    }

    #[test]
    fn patched_chunk0_equals_a_rebuild() {
        let key = [9u8; 32];
        let leaves = 300;
        let mut bytes = data(leaves * CHUNK_BYTES, 3);
        let t = MatrixTree::build(key, leaves, &SliceSource(&bytes)).unwrap();
        let mut chunk = [0u8; CHUNK_BYTES];
        chunk.copy_from_slice(&data(CHUNK_BYTES, 99));
        let patch = t.patch_chunk0(&chunk);
        let base = bytes.clone();
        bytes[..CHUNK_BYTES].copy_from_slice(&chunk);
        let reference = MerkleTree::new(&bytes, key);
        assert_eq!(patch.root(), reference.root());
        assert_ne!(patch.root(), t.root(None));
        let set = [0usize, 1, 5, 64, 299];
        let got = t
            .multileaf_proof(&set, Some(&patch), &SliceSource(&base))
            .unwrap();
        let want = reference.get_multileaf_proof(&set);
        assert_eq!(got.siblings, want.siblings);
        assert_eq!(got.leaf_data, want.leaf_data);
        assert_eq!(got.root, want.root);
        // The unpatched view is untouched.
        assert_eq!(t.root(None), *blake3::keyed_hash(&key, &base).as_bytes());
    }

    #[test]
    fn default_shape_stores_little() {
        // 131072 x 4096 bytes = 2^19 chunks: segments of 64 chunks, 2^13 + ... + 2 upper nodes.
        let leaves = 1usize << 19;
        let segs = leaves >> DEFAULT_SEGMENT_LEVEL;
        let upper: usize = (0..).map(|l| segs >> l).take_while(|&x| x >= 2).sum();
        assert_eq!(upper, 2 * segs - 2);
        assert!((upper + 127) * 32 < 1 << 20);
    }
}
