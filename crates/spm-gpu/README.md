# spm-gpu — libspm_cuda and its Rust wrapper

`spm-gpu` builds `libspm_cuda` (CUDA C++ for the GB10, `sm_121a`) with `build.rs` and wraps its C ABI
(`cuda/include/spm_cuda.h`) in a safe Rust API. It holds the GPU side of a PearlHash certificate-V3
job: operand generation, rank-128 noise, the noised s8 operands and the fused int8 GEMM that hashes
every 8 × 16 tile on the fly. The algorithm is the one `crates/spm-cpuref` defines and is checked
against it bit for bit (gate G0).

This is kernel **v0, strategy A** of `docs/en/KERNEL.md`: a classic `cp.async` pipeline.

## Layout

| Path | What it does |
|---|---|
| `cuda/common/blake3.cuh` | BLAKE3 compression of one 64-byte block (keyed and unkeyed), host and device. The round layout follows the ISC-licensed `csrc/blake3/blake3.cuh` of the official `pearl-gemm`; the ISC notice is in the file header and in `NOTICE`. |
| `cuda/common/splitmix.cuh` | SplitMix64 int7 stream, bit-identical to `spm_cpuref::fill_int7` (counter based: any thread produces any word). |
| `cuda/common/noise_hash.cuh` | The 64-byte keyed noise hash `H(i, label, key, slot)`, the `(byte & 63) − 32` uniform map and the `(p, q)` permutation pair. |
| `cuda/common/u256.cuh` | Branch-free little-endian U256 `digest ≤ bound`. |
| `cuda/prep/prep.cu` | Fill A and Bᵀ on the GPU, uniform factors A_L / B_Rᵀ, permutation pairs, and `X' = X + F[p] − F[q]` (E is never stored; B'ᵀ is built in place). |
| `cuda/gemm/mma_ops.cuh` | MMA policies: `S8S8S32` (`IMMA.16832.S8.S8`, used) and `E4M3E4M3F32` (`QMMA.16832`, for the V4 fork; not instantiated until its transcript is specified). |
| `cuda/gemm/gemm_v0_cpasync.cuh/.cu` | The fused kernel, templated on the MMA policy and on the dump mode. |
| `cuda/gemm/job.cu` | The job object behind the C ABI: buffers, B side at creation, A side per attempt, chunked launches, hit ring, dump, debug read-back. |
| `src/ffi.rs` | The raw bindings — the only `unsafe` code of the crate, one SAFETY comment per call. |
| `src/job.rs` | The safe API (`#![forbid(unsafe_code)]`); the crate root denies `unsafe_code` (a crate-level `forbid` could not be relaxed for `ffi`). |
| `tests/g0.rs` | The G0 harness (feature `gpu`, `SPM_GPU_TESTS=1`). |
| `examples/bench.rs` | The throughput bench. |
| `tools/check-sass.py` | SASS gate: `IMMA.16832.S8.S8` and `LDSM` in every fused kernel, no `HMMA` in the library. |
| `cuda/probes/fill_bw.cu` | Standalone probe (not linked): `cp.async` fill and DSMEM bandwidth on this unit. |

## The kernel (`gemm_v0_cpasync`)

* **CTA tile 128 × 256 × 64**, 3 stages of `cp.async.cg` 16-byte copies (A 8 KiB + B'ᵀ 16 KiB per
  stage, **73,728 B** of shared memory), 256 threads = 8 warps in a 2 × 4 grid of 64 × 64 warp tiles,
  1 CTA per SM.
* Shared-memory rows are 64 bytes; 16-byte chunk `c` of row `r` lives at chunk `c ^ ((r >> 1) & 3)`,
  so the cp.async stores and every 8-row `ldmatrix` phase are bank-conflict free. A and B'ᵀ are both
  K-major, so all fragments come from plain `ldmatrix.x4` (no `.trans`): per k32 step a warp issues
  4 `ldmatrix.x4` for A (one per m16 fragment), 4 for B (two n8 fragments each) and
  **4 × 8 = 32 `mma.sync.m16n8k32.s8.s8.s32`**, i.e. 64 MMAs per 64-wide k-tile and **128 MMAs per
  128-wide transcript slice per warp** (1024 per CTA).
* **One thread = one hash tile.** In the m16n8 accumulator layout, lane `l` of a 64 × 64 warp tile
  holds rows `{l/4, l/4 + 8}` × cols `{2(l%4), 2(l%4)+1}` of each of the 4 × 8 fragments: exactly rows
  `t_rows + {0, 8, …, 56}` × cols `t_cols + {0, 1, 8, 9, …, 56, 57}` with `t_rows = row0 + l/4`,
  `t_cols = col0 + 2(l%4)`. The per-slice XOR fold of the 128 **cumulative** accumulators is
  thread-local (a balanced LOP3 tree): no shuffles, no shared-memory reduction.
* **Transcript in registers.** The 16 words form a rotating register queue whose head is always
  word `s % 16` (update, then shift by one); after the last slice it is rotated back into order
  with four uniform-predicate conditional rotations. No dynamic register indexing, no local memory.
* **Epilogue**: `digest = BLAKE3_keyed(a_noise_seed, t[0..16])`, `LE-U256(digest) ≤ bound` →
  hit ring (atomic counter, `(t_rows, t_cols, digest)`); in dump mode every tile also writes its
  104-byte `TileResult::dump_bytes` record at its reference index.
* Only full 128-wide slices are computed (columns past `floor(k/128)·128` never enter the transcript).
  Shapes: m, n multiples of 64 (edge CTA tiles zero-fill the missing 64-row passes and their warps
  skip the epilogue), 2048 ≤ k ≤ 65536, k % 64 = 0. CTA tiles are rastered in groups of 8 tile rows.
* **Chunked launches and cancellation**: an attempt is a sequence of launches over ranges of CTA
  tiles. The automatic chunk is ~4 ms at the IMMA peak rate and is re-derived from the measured rate
  after every chunk (whole waves); `spm_job_run_attempt` keeps two chunks in flight. It reads the
  host abort flag before queuing each chunk and every few tens of microseconds while waiting; once
  set, it raises a device abort word (side stream) that every CTA reads at its start and tests,
  together with a `__syncthreads_or`, once its first k-tile has landed — so its latency hides
  behind that first wait. CTAs that see it leave before any MMA; the attempt returns
  `SPM_CHUNK_ABORTED` within about one CTA tile time (**0.23–0.53 ms measured** at 16384² × 4096,
  instead of up to two chunks) and is void; hits found so far stay valid and readable.

### Resources (`ptxas -v`, CUDA 13.0, `sm_121a`)

```
gemm_v0_cpasync.cu: fused_kernel<S8S8S32, dump = false>  Used 222 registers, 0 bytes stack frame, 0 bytes spill stores, 0 bytes spill loads, 1 barrier
gemm_v0_cpasync.cu: fused_kernel<S8S8S32, dump = true>   Used 212 registers, 0 bytes stack frame, 0 bytes spill stores, 0 bytes spill loads, 1 barrier
prep.cu: apply_noise_kernel 47 regs (4096 B static smem), uniform_factor_kernel 36, perm_pairs_kernel 31, fill_int7_kernel 20 — all 0 spills
dynamic shared memory of the fused kernel: 73,728 B (3 stages x 24 KiB); occupancy 1 CTA/SM
```

SASS (`tools/check-sass.py`, per fused kernel): `IMMA.16832.S8.S8` × 128, `LDSM.16.M88.4` × 32,
`LDGSTS.E.BYPASS.128` × 24; **no `HMMA`** anywhere in `libspm_cuda.a`. Gate: pass.

### Device memory

| Job | Bytes |
|---|---|
| bench shape m = n = 16384, k = 4096 (A_base + A' + B'ᵀ + A_L + B_Rᵀ + pairs + 4096-entry ring + counters) | 205,701,632 (196.2 MiB) |
| production shape m = n = 131072, k = 4096 | 1,644,347,904 (1.53 GiB) |

`spm_job_create` refuses anything over 2 GiB (`SPM_ERR_BUDGET`); the dump buffer (m·n/128 × 104 B)
only exists in dump mode.

## C ABI and Rust API

```c
spm_job_create(&params, &job);        // m, n, k, config52 (validated), gen_seed | host A/Bᵀ, b_noise_seed, bound, flags
spm_job_patch_a(job, off, bytes, n);  // e.g. the nonce in A chunk 0 (next attempt picks it up)
spm_job_set_attempt(job, a_noise_seed, bound_or_NULL);  // A_L, A pairs, A' = A + E_A; resets ring and cursor
spm_job_run_chunk(job, &status);      // one chunk: SPM_CHUNK_MORE / SPM_CHUNK_DONE
spm_job_run_attempt(job, &abort_flag, &status);  // the rest, 2 chunks in flight; SPM_CHUNK_ABORTED on abort
spm_job_read_hits(job, hits, cap, &total);   spm_job_read_dump(job, buf, cap, &written);
spm_job_read_buffer(job, SPM_BUF_*, off, buf, len);   spm_job_info(job, &info);   spm_job_destroy(job);
```

Every call returns a status (`SPM_OK`, `SPM_ERR_SHAPE`, `SPM_ERR_CONFIG`, `SPM_ERR_BUDGET`,
`SPM_ERR_CUDA` with the CUDA code kept per job, …); nothing throws or aborts across the boundary.
The commitment (job key, Merkle roots, V3 salting, seed chain) stays on the host in v0, so the GPU
takes the two noise seeds, not `header76`; `config52` is checked byte for byte against the only
configuration the kernel implements. In Rust: `Job::new(&JobParams { .. })`, `set_attempt`,
`run_chunk` / `run_attempt(&AtomicU32)` / `run_to_completion`, `hits`, `dump_records`,
`read_buffer`, `info`; errors are `GpuError { op, kind, cuda_error, message }`.

## Gate G0 (2026-09-26, GB10, driver 580.178.04, vLLM resident)

`SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0` — 8 tests, all pass:

* `g0_every_tile_matches_the_oracle`: m, n ∈ {256, 512, 1024} × k ∈ {2048, 4096} × 3 seeds =
  **54 problems, 150,528 tiles**, GPU dump (generated operands) equal to `spm_cpuref::transcripts`
  (`first_mismatch == None`, equal `tiles_digest`) — **0 mismatches**.
* `forced_hits_verify_as_plain_proofs`: 5 problems 256² × 2048 at an easy nbits; the GPU hit set
  equals the oracle's, **166 GPU hits** become PlainProofs (`build_plain_proof`) that pass `verify_v3`
  (`check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`) and `check_rank_penalty`;
  mutated proofs (flipped A/Bᵀ byte, shifted rows, noise rank, other header) are rejected; with the
  bound set to the smallest digest exactly that tile hits.
* `noise_factors_match_the_official_generators`: A_L and the A pairs for **1000 seeds**, B_Rᵀ and the
  B pairs for 24 seeds equal `generate_uniform_random_matrix` / `generate_permutation_matrix`.
* `operands_and_blake3_match_the_cpu`: device BLAKE3 = `blake3::keyed_hash` (200 random cases);
  A_base = `fill_int7`; A' and B'ᵀ = `Oracle::noised_a/noised_bt` (192 × 320 × 4096).
* `host_matrices_edges_chunks_and_abort`: host matrices with ±64 entries, partial CTA tiles
  (192 × 320), one CTA per chunk, an abort (attempt void, `set_attempt` again), then a nonce-style
  patch of A chunk 0 with the new commitment — both dumps equal the oracle.
* `mid_attempt_abort_is_fast_and_leaves_the_job_clean`: another thread raises the abort flag 1, 3
  and 5 ms into 14 ms attempts at 16384² × 4096: the call returns in 0.23–0.53 ms, the partial hits
  are a subset of the clean run's, and the next attempt reproduces the clean hit set exactly.
* `chunking_does_not_change_the_hits`: 16384² × 2048, adaptive vs fixed chunk sizes → identical hit
  sets (~32 k hits).
* `hit_ring_overflow_is_counted`: with every tile a hit and a 100-entry ring, the count stays exact and
  the stored entries are real tiles with their real digests.

`compute-sanitizer` memcheck (with leak check), racecheck and synccheck: 0 errors / 0 hazards on the
edge/chunk/abort test, the forced-hit test and the operand test.

## Throughput

`cargo run --release -p spm-gpu --example bench` (m = n = 16384, k = 4096, 10 s). Phase 1 times every
chunk with CUDA events (kernel only); phase 2 runs back-to-back attempts on the wall clock (what a
miner gets). Peak references from MB1 (`docs/en/BENCHMARKS.md`): 919 MAC/clk/SM, i.e. 106.9 T-MAC/s at
2424 MHz, 96.0 T-MAC/s at 2200 MHz.

Runs of 2026-09-26 with nothing else on the GPU but the resident vLLM (stock clocks, driver
580.178.04, CUDA 13.0). The first row is the final code (commit with the device abort word); the
others are the same kernel one commit earlier (208 registers, host-side abort only):

| Run (UTC) | SM clock | Power mean / max | Chunk GPU time mean / max | Kernel only mean / median / best | Sustained (fused kernel) | Incl. A prep | Abort latency |
|---|---|---|---|---|---|---|---|
| 20:40:01 | 2424 MHz | 90.5 / 96.9 W | 3.77 / 6.83 ms | 81.7 / 83.8 / 96.6 T-MAC/s | **77.0 T-MAC/s** (80.2 % of 96.0) | 72.9 | **0.21 / 0.27 ms** |
| 20:39:26 | 2397 MHz | 86.1 / 95.1 W | 3.88 / 7.26 ms | 83.4 / 85.4 / 95.1 T-MAC/s | 77.1 T-MAC/s (80.3 %) | 72.9 | 7.0 / 7.3 ms |
| 20:33:18 | 2424 MHz | 87.9 / 95.4 W | 3.75 / 7.62 ms | 82.8 / 84.3 / 94.4 T-MAC/s | 77.7 T-MAC/s (80.9 %) | 73.5 | — |
| 20:33:53 | 2356 MHz | 87.4 / 93.3 W | 3.77 / 7.39 ms | 81.5 / 83.2 / 93.1 T-MAC/s | 76.3 T-MAC/s (79.5 %) | 72.2 | — |
| 20:32:33 | 2424 MHz | 84.0 / 95.0 W | 3.79 / 7.63 ms | 81.3 / — / 93.2 T-MAC/s | 75.1 T-MAC/s (78.2 %) | 71.2 | — |

* **Sustained ≈ 77 T-MAC/s ≈ 80 % of the 96.0 T-MAC/s capped peak**, measured at stock 2424 MHz; kernel
  only ≈ 82–84 T-MAC/s ≈ 77–79 % of the register-only IMMA peak at the clock in use (106.9 T-MAC/s at
  2424 MHz); single attempts reach 93–97 T-MAC/s (~90 %) when nothing else touches the GPU or the
  memory. The standalone experiment driver (chunks of 2736 CTA tiles, per-chunk GPU time, medians of
  interleaved runs) sees 86–89 T-MAC/s for the same kernel.
* The A-side prep (A_L, pairs, A' for 16384 × 4096) takes 0.63 ms per attempt; B side and matrix
  generation at job creation 1.4 ms.
* Chunks: the first chunks of a job use the peak-rate size (3216 CTA tiles here, ~7 ms at the
  measured rate), then the adaptive size settles at ~3.8 ms. The abort latency is the time from the
  flag store (another thread, mid-attempt) to the return of `run_attempt`.
* At full load the GB10 draws ~85–97 W at stock clocks and sometimes trims the SM clock a little
  (2356 MHz mean in one run); the 2200 MHz cap would lower both.

Production shape (`--m 131072 --n 131072 --k 4096 --seconds 0`, 20:40:27 UTC, final code, only the
vLLM besides): 1,644,347,904 B of device memory, job creation with matrix fill and B side 13.2 ms,
3 attempts of 7.04 × 10¹³ credited MACs at **79.8 T-MAC/s kernel only (0.88 s per attempt)**,
655 chunks of 4.04 ms mean (max 7.94 ms, the first peak-sized chunks), power 78 W mean / 95 W max,
**abort latency 0.28 ms mean / 0.38 ms max** over 5 trials.

Stock clocks only: locking 2200 MHz needs root, and the vLLM stays resident (its ~6 % background
load time-slices with every kernel). Other CUDA jobs of the owner's machine and the CPU sharing the
LPDDR5x bandwidth move these numbers by ±15 %; the bench prints the other CUDA processes it saw.

### What bounds it

Controlled variants of the same kernel (identical results to the production kernel where the
semantics are unchanged; per-chunk GPU timing, medians of interleaved runs, 16384² × 4096):

| Variant | T-MAC/s |
|---|---|
| production kernel | ~86–89 |
| no global loads at all (operands already in smem) | ~102 (95 % of the 106.9 peak) |
| A loads only (1/3 of the L2 traffic) | ~100 |
| full traffic but always the same k-tile (L2 resident, no DRAM) | ~91 |
| 4 stages / 4 stages + barrier in mid k-tile / BK = 128 × 2 stages / raster groups 6–32 / L2 prefetch hints | within ±2 % of production |
| persistent CTAs with cross-tile prefetch | ~6 % slower |
| CTA pairs sharing B'ᵀ through DSMEM with a cluster barrier per k-tile | 3× slower (`barrier.cluster` per k-tile alone costs ~30 %) |

So the MMA/`ldmatrix`/fold loop itself runs at ~95 % of the register-only peak; the L2 → SM traffic of
the 128 × 256 tile (1.5 MiB per CTA tile at k = 4096, ~1.0 TB/s at 87 T-MAC/s) costs ~11 % and the
DRAM misses ~6 %. `cuda/probes/fill_bw.cu` measures the fill paths on this unit: `cp.async.cg` 16 B
into shared memory reaches ~2.1 TB/s (18 B/clk/SM), but DSMEM moves only ~118 GB/s for the whole GPU
(1.0 B/clk/SM) with either `st.shared::cluster` or `cp.async.bulk` pushes, and ptxas warns that TMA
`.multicast::cluster` is slow on sm_121a. Sharing operands between the SMs of a cluster is therefore
not a lever on GB10; the per-CTA traffic of a 64K-register SM (8 warps × 64 × 64, one hash tile per
thread) is the structural limit of `mma.sync` designs here, and what is left is latency hiding and
DRAM locality (TMA fills, deeper rings, L2-band raster — strategy C's ground).

## Reproduce

```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=$HOME/.cache/spark-pearl-miner/target   # a path without spaces
# build + ptxas report (registers, spills, smem) as build warnings
SPM_PTXAS_VERBOSE=1 cargo build --release -p spm-gpu
# SASS gate
python3 tools/check-sass.py            # or: cuobjdump -sass <libspm_cuda.a> | grep -c IMMA.16832.S8.S8
# unit tests (no GPU) and clippy
cargo test --release -p spm-gpu --lib
cargo clippy --release -p spm-gpu --features gpu --all-targets -- -D warnings
# G0 (touches the GPU for a few seconds; needs --release for the CPU oracle)
SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --nocapture
# sanitizers on the small-shape tests
SPM_GPU_TESTS=1 compute-sanitizer --tool memcheck --leak-check full $CARGO_TARGET_DIR/release/deps/g0-<hash> host_matrices
SPM_GPU_TESTS=1 compute-sanitizer --tool racecheck $CARGO_TARGET_DIR/release/deps/g0-<hash> host_matrices
SPM_GPU_TESTS=1 compute-sanitizer --tool synccheck $CARGO_TARGET_DIR/release/deps/g0-<hash> host_matrices
# bench (10 s, 196 MiB of device memory)
cargo run --release -p spm-gpu --example bench
cargo run --release -p spm-gpu --example bench -- --m 131072 --n 131072 --k 4096 --seconds 0   # production shape
# fill-bandwidth probe
nvcc -O3 -std=c++17 -gencode arch=compute_121a,code=sm_121a cuda/probes/fill_bw.cu -o /tmp/fill_bw && /tmp/fill_bw
```
