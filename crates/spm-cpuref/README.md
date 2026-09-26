# spm-cpuref — CPU reference oracle

`spm-cpuref` computes, on the CPU and without any floating point, everything the GPU kernel has to
produce for a PearlHash certificate-V3 job: the commitment, the rank-128 noise, the noised int8
operands, the exact i32 GEMM, the 16-word transcript and jackpot digest of **every** hash tile, and the
`PlainProof` of any tile. It is the bit-exact target of gate G0.

Consensus code is not re-implemented. The noise generators (`generate_uniform_random_matrix`,
`generate_permutation_matrix`), the jackpot hash (`compute_jackpot_hash`), the V3 salting
(`bind_root_a/b`), the Merkle trees (`pearl_blake3::MerkleTree`, `get_multileaf_proof`) and the verifier are
the official `zk-pow` / `pearl-blake3` functions at the pinned revision `3fe2267`. Only the glue that the GPU
must replicate (job key, root → seed chain, sparse noise products, noised GEMM, per-slice transcript
accumulation) is written here, and the tests prove it equal to the reference.

## API

```rust
let p = Problem::generate(m, n, k, header, seed)?;   // or Problem::from_matrices(header, m, n, k, a, bt)
let c = commit(&p)?;                                   // Commitment { job_key, root_a, root_b, bound_a, bound_b, b_noise_seed, a_noise_seed }
let tiles = transcripts(&p)?;                          // Vec<TileResult { t_rows, t_cols, transcript: [u32; 16], digest: [u8; 32] }>
let hits = find_hits(&p, bound)?;                      // tiles with LE-U256(digest) <= bound
let proof = build_plain_proof(&p, &hits[0])?;          // PlainProof { m, n, k, noise_rank, a, bt, moe: None }
verify_v3(&p.header, &proof, Some(nbits_share))?;      // check_cert_version_eligible(3) + verify_plain_proof(Salted)
```

`Oracle::new(&p)` keeps the commitment and the noised operands in memory and adds the debugging views:
`noise_factors()` (A_L, A_R, B_L, B_Rᵀ), `noise()` (E_A, E_Bᵀ), `noised_a()` / `noised_bt()` (A', B'ᵀ),
`gemm()` (C'), `tile()` and `trace()` (per-slice folds and final accumulators of one tile).
`transcripts()` uses every core and is deterministic; the largest G0 shape (1024 × 1024 × 4096, 8192 tiles)
takes about 0.1 s on the GB10's 20 CPU cores.

## The algorithm the GPU must replicate

Notation: `blake3(x)` is unkeyed BLAKE3-256, `blake3_k(key, x)` keyed BLAKE3, all integers little endian.
Our configuration (`spm_pow::mining_config(k)`): r = 128, `MMAType::Int7xInt7ToInt32`, hash tile 8 × 16 with
rows `{0, 8, …, 56}` and columns `{0, 1, 8, 9, …, 56, 57}` (period 64 in both dimensions).

### 1. Inputs

* A is m × k, Bᵀ is n × k (row j of Bᵀ is column j of B), both row major, int8. The verifier accepts entries
  in **[-64, 64]** (64 included; the reference miner draws from that range). Our generator draws from
  [-64, 63].
* Shape rules (official `public_params_sanity_check`): k % 64 = 0, 16r ≤ k ≤ min(2^16, 4r²), k ≥ 1024;
  m, n multiples of 64 and ≤ 2^24. With r = 128 the smallest problem is m = n = 64, k = 2048.

### 2. Commitment

1. `job_key = blake3(header76 ‖ config52)` — **unkeyed**, 128 bytes (`IncompleteBlockHeader::to_bytes`, whose
   hash fields are byte-reversed, then `MiningConfiguration::to_bytes`).
2. `root_a = blake3_k(job_key, pad1024(A))`, `root_b = blake3_k(job_key, pad1024(Bᵀ))`: entries as
   two's-complement bytes, zero-padded to a multiple of 1024 bytes. This equals the root of the
   `pearl_blake3` Merkle tree (1024-byte leaves = BLAKE3 chunks). With m, n, k multiples of 64 the matrices
   are always chunk aligned, so the padding never adds bytes in practice.
3. V3 salting: `bound_a = blake3_k(SEED_SALT_A, root_a ‖ u32(m) ‖ 0^28)`,
   `bound_b = blake3_k(SEED_SALT_B, root_b ‖ u32(n) ‖ 0^28)` (`bind_root_a/b`).
4. Seed chain, B first: `b_noise_seed = blake3(job_key ‖ bound_b)`,
   `a_noise_seed = blake3(b_noise_seed ‖ bound_a)`.

### 3. Noise (rank r = 128)

Noise hash: `H(i, label, key, slot) = blake3_k(key, msg)` with a 64-byte `msg` that is zero except bytes
`4·slot .. 4·slot+4 = (i + 1)` as i32 and bytes `32..64 = label`. Labels are `"A_tensor"` / `"B_tensor"`
zero-padded to 32 bytes; the A side is keyed with `a_noise_seed`, the B side with `b_noise_seed`.

* Uniform factors A_L (m × r) and B_Rᵀ (n × r): row `i` is bytes `[i·r, (i+1)·r)` of the stream
  `H(0, ·, ·, 0) ‖ H(1, ·, ·, 0) ‖ …`, each byte mapped to `(byte & 63) − 32` ∈ **[-32, 31]**. With r = 128,
  row i is exactly hashes `4i … 4i+3`.
* Permutation factors A_R and B_L: k pairs. Pair `l` takes the u32 word `l % 8` of `H(l / 8, ·, ·, 1)`,
  `p = x & (r − 1)`, `q = p ^ (1 + ((r − 1)·x >> 32))`. Always `p ≠ q`, both `< r`.
* `E_A[i][l] = A_L[i][p_A(l)] − A_L[i][q_A(l)]` and `E_Bᵀ[j][l] = B_Rᵀ[j][p_B(l)] − B_Rᵀ[j][q_B(l)]`, i.e.
  E_A = A_L·A_R and E_B = B_L·B_R. Entries are in **[-63, 63]**: the reference computes the difference in
  i32 and casts to i8, and it never wraps.

### 4. Noised operands

`A' = A + E_A`, `B'ᵀ = Bᵀ + E_Bᵀ`. The reference forms the sums in i32; they always lie in **[-127, 127]**
(|A| ≤ 64, |E| ≤ 63), so A' and B'ᵀ are exact **s8** — what `IMMA.16832.S8.S8` consumes. No saturation, no
wrap; `add_noise` checks it.

### 5. GEMM

`C'[i][j] = Σ_l A'[i][l]·B'ᵀ[j][l]` in i32. |C'| ≤ 127²·k < 2^31 for every consensus-valid k (≤ 2^16), so the
accumulators never overflow (a wrapping or a plain i32 add give the same bits).

### 6. Tiles

A tile is identified by its base `(t_rows, t_cols)`, where `t_rows` is valid for the row pattern
(`t_rows mod 64 < 8`) and `t_cols` for the column pattern (`t_cols mod 64` even and `< 8`). Its rows are
`t_rows + {0, 8, …, 56}`, its columns `t_cols + {0, 1, 8, 9, …, 56, 57}`: 128 outputs. The tiles partition the
m × n output exactly once (m·n / 128 tiles). The reference enumerates bases row-major: `t_rows` ascending
outer, `t_cols` ascending inner; the first hit in that order is what `try_mine_one` returns.

### 7. Transcript (the part the kernel epilogue must get exactly right)

```
acc[128] = 0 (i32);  t[16] = 0 (u32)
for s in 0 .. floor(k / r):                       # k = 2048 → 16 slices, k = 4096 → 32 slices
    for every (u, v) in the tile:
        acc[u][v] += Σ_{l = s·r}^{s·r + r − 1} A'[row_u][l] · B'ᵀ[col_v][l]
    fold = XOR over all 128 acc[u][v], each read as u32 (two's-complement bits)
    t[s mod 16] = rotl32(t[s mod 16], 13) XOR fold
```

* The fold is over the **cumulative** accumulators (the running partial sums after slice s), not over the
  slice's own contribution.
* The slot is `s mod 16`. At k = 2048 each word is written once (the rotation acts on 0 and is invisible);
  at k = 4096 each word is written twice, and the second write rotates the first fold left by 13 first.
* If r does not divide k, the last `k mod r` columns are committed but never enter the transcript.
* XOR is order independent, so any reduction order inside a slice gives the same fold.

### 8. Digest and difficulty

`digest = blake3_k(a_noise_seed, t[0] ‖ … ‖ t[15])` (64 bytes, each word LE) — `compute_jackpot_hash`. It is a
hit when `U256::from_little_endian(digest) ≤ bound`, with
`bound = nbits_to_difficulty(nbits) · h·w·dot_len` (`extract_difficulty_bound`, saturating; h·w = 128,
dot_len = k − k mod r). At r = 128 the rank-penalized bound (`penalized_target_bound`, what pools check) is the
same number.

### 9. Proof

`PlainProof { m, n, k, noise_rank: 128, a, bt, moe: None }` where `a` opens rows `t_rows + {0, 8, …, 56}` of A
and `bt` opens rows `t_cols + {0, 1, 8, 9, …, 56, 57}` of Bᵀ: `MerkleTree::new(pad1024(bytes), job_key)`,
leaves = `compute_leaf_indices_from_rows(rows, (rows_total, k))`, `get_multileaf_proof(leaves)`.
It is byte-identical (bincode) to what the reference miner emits for the same tile.

## Generator

`Problem::generate` fills A and Bᵀ from SplitMix64 streams seeded with `seed ^ DOMAIN_A` and
`seed ^ DOMAIN_BT` (`"spm-a-01"` / `"spm-b-01"` in ASCII). Each 64-bit output gives 8 entries, one per
little-endian byte, entry = `(byte & 0x7f) − 64`. SplitMix64 is counter based
(word w = `mix(s + (w + 1)·0x9e3779b97f4a7c15)`), so a GPU harness can regenerate any row directly.

## Confirmed reference semantics

| Question | Answer (zk-pow / pearl-blake3 at `3fe2267`) |
|---|---|
| job key | unkeyed `blake3(header76 ‖ config52)` (`compute_job_key`, `PublicProofParams::job_key`) |
| Merkle roots | keyed by `job_key`; data zero-padded to a 1024-byte multiple (`pad_to_chunk_boundary`) |
| V3 seeds | salt roots with `bind_root_a(root_a, m)` / `bind_root_b(root_b, n)`, then B seed first, then A seed |
| uniform noise factors | `(byte & 63) − 32` ∈ [-32, 31] |
| noise entries | difference of two distinct uniform entries ∈ [-63, 63], i8 without wrap |
| committed entries | verifier accepts [-64, 64]; `try_mine_one` draws [-64, 64] |
| noised operands | computed in i32 by the reference, always in [-127, 127], so exact s8 |
| accumulation | i32, never overflows for k ≤ 2^16 |
| fold | XOR of the **cumulative** tile accumulators after each r-wide slice |
| transcript | 16 × u32, slot `s mod 16`, `rotl 13` then XOR |
| digest | `blake3_k(a_noise_seed, 64-byte LE transcript)`, compared as LE U256 ≤ bound |

## Tests

`cargo test --release -p spm-cpuref` (about 3 s of test time on the GB10, once built):

* `reference.rs`: commitment equals `PublicProofParams::job_key` / `commitment_hash`; E_A and E_Bᵀ equal
  `compute_noise_for_indices` (whole matrices and the per-tile `compute_noise` path); **every tile** of five
  problems (3 × 256² × 2048, 128 × 192 × 4096, 64² × 2112) equals the reference pieces
  (`compute_noise_for_indices` + `compute_jackpot` + `compute_jackpot_hash`); extreme entries (±64); final
  accumulators equal C'; and, replaying the RNG of `try_mine_one`, our first hit and its PlainProof are
  byte-identical to the reference miner's (4 seeds), including the miss and "wrong jackpot" modes.
* `proofs.rs`: every hit of three 256² × 2048 problems passes `check_cert_version_eligible(3)` +
  `verify_plain_proof(Salted)` (with and without the nbits override) and `check_rank_penalty`, and the digest
  the verifier recomputes equals ours; a non-hit fails with the jackpot condition (and passes when the bound
  saturates, so only difficulty rejects it); tampered proofs fail; invalid shapes and entries are refused.
* `golden.rs`: `tests/fixtures/golden-cpuref-{1,2,3}.json` (pinned in `SHA256SUMS`).
* `speed.rs`: the smallest valid shape end to end, and the largest G0 shape within a time budget.

## How G0 uses it

1. For each G0 problem (m, n ∈ {256, 512, 1024}, k ∈ {2048, 4096}, 3 seeds) the harness builds the same
   `Problem`, runs the GPU kernel in debug-dump mode and reads back one `TileResult` per tile in the order
   above (the 104-byte record of `TileResult::dump_bytes`: `t_rows`, `t_cols`, 16 words, digest).
2. `tiles_digest(gpu) == tiles_digest(oracle.transcripts())` is the pass condition. On a mismatch,
   `first_mismatch` names the first bad tile and `Oracle::trace` gives its per-slice folds and final
   accumulators; `noise()`, `noised_a()`/`noised_bt()` and `gemm()` localize the stage (noise build,
   operand build, GEMM, fold, transcript, hash).
3. The golden fixtures are the fixed targets that do not depend on this crate staying unchanged.
4. Forced hits: any GPU hit is turned into a PlainProof with `build_plain_proof` and must pass `verify_v3`.
