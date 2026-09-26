# spm-gpu — GPU worker library (M5, strategy C)

`spm-gpu` builds `libspm_cuda` (CUDA C++ for sm_121a, `cuda/`) with nvcc and wraps its C ABI
(`cuda/include/spm_cuda.h`) in a safe Rust API. It computes, for a PearlHash V3 job, the noised
int8 operands and every hash tile's transcript, digest and bound comparison, bit-identical to
`spm-cpuref` (gate G0).

```rust
let mut job = Job::new(&JobConfig::new(m, n, k, Operands::Generated { seed }, commitment.b_noise_seed))?;
job.set_attempt(&commitment.a_noise_seed, &bound_le_bytes)?; // A side of the attempt
job.prepare_attempt(&next_a_noise_seed)?;                    // optional: next A' built meanwhile
job.run_attempt()?;                                          // chunks of ~4.5 ms (<= ~8 ms at 1800 MHz), abortable
let (hits, total) = job.hits()?;                             // (t_rows, t_cols, digest)
```

The commitment (job key, Merkle roots, V3 salting, seed chain) stays on the CPU in v0, so a job takes
the derived seeds: `b_noise_seed` at creation, `a_noise_seed` per attempt. `unsafe` lives only in
`src/ffi.rs`; the other modules `#![forbid(unsafe_code)]`.

## Pipeline

| Step | Where | What |
|---|---|---|
| job create | `cuda/prep` | B_Rᵀ (n×128), B_L pairs (k), B'ᵀ = Bᵀ + E_Bᵀ (n×k s8); Bᵀ is the SplitMix64 int7 stream of the seed (never stored) or the uploaded matrix (noised in place) |
| set_attempt | `cuda/prep` | A_L (m×128), A_R pairs (k), A' = A + E_A (m×k s8), optional ≤ 4 KiB prefix override of A (nonce patch); or swap in the set `prepare_attempt` built |
| prepare_attempt (optional) | `cuda/prep` | the same A side for a future attempt, into a spare set, on a lowest-priority stream while the current attempt runs |
| run_chunk | `cuda/gemm` | fused GEMM + per-slice fold + transcript + keyed BLAKE3 + LE-U256 bound compare over a range of CTA tiles; hits to a ring, or one 104-byte `TileResult` record per hash tile in dump mode |

Noise hash, uniform factors, pairs and the int7 fill follow `crates/spm-cpuref/README.md` exactly
(`cuda/common/blake3.cuh`, `splitmix.cuh`, `cuda/prep/prep.cu`). The single-block BLAKE3 layout is
ported from the ISC `miner/pearl-gemm/csrc/blake3/blake3.cuh` (notice in the file header and NOTICE).

## Kernel (strategy C: TMA + mbarrier, persistent)

`cuda/gemm/gemm_tma.cuh`, instantiated in `gemm_tma.cu`:

* Persistent grid of 48 CTAs (one per SM); CTA tiles of 128 × 256 × 64 taken from an atomic
  counter in an L2 band raster (16 CTA rows per band, column-major inside the band).
* 12 warps: 8 MMA warps (2 × 4 warp tiles of 64 × 64) and one producer warpgroup whose lane 0
  issues the TMA loads. `setmaxnreg` gives the MMA warps 232 registers and the producer warpgroup
  40, so each SM sub-partition holds 2 MMA warps + 1 producer warp in its 16K registers.
* 4-stage ring of {A' 128×64, B'ᵀ 256×64} = 24 KiB stages, `cp.async.bulk.tensor.2d` with
  SWIZZLE_64B, `full`/`empty` mbarriers per stage. The tile id travels with the first stage of each
  tile; `-1` stops the CTA. The abort flag (host-mapped) is read before every tile.
* The first CTA row of each band prefetches its B'ᵀ k-tile 4 k-tiles ahead into L2
  (`cp.async.bulk.prefetch.tensor`).
* `mma.sync.m16n8k32.row.col.s32.s8.s8.s32` from `ldmatrix.x4`. Lane l of a 64 × 64 warp tile holds
  exactly the 128 accumulators of hash tile (row0 + l/4, col0 + 2(l%4)), so the per-slice XOR fold
  and the 16-word transcript are thread-local registers (the slot is kept at `t[0]` by rotating
  the array, the leftover rotation is undone before hashing).
* Templated on the MMA policy: `MmaS8` is instantiated; `MmaE4M3` (`kind::f8f6f4` e4m3, QMMA) is
  declared for the v4 fork but not instantiated until the v4 fold is specified.
* The lane geometry (`ldsm_offsets`, `lane_row` / `lane_col`, `dump_index`, `tile_coords`,
  `rotate_right_if`, `OPERAND_SWIZZLE`) is `__host__ __device__`, and `cuda/tests/layout_check.cu`
  proves on the host that it gives every lane exactly one hash tile (see G0 below). Factoring it
  out left the kernel's SASS byte-identical.
* Host side (`cuda/common/job.cu`): chunks sized adaptively to a 4.5 ms target (EMA of the time
  per tile, reacting faster to slower chunks, never below 4 tiles per CTA) and capped at the tiles
  that take 8 ms at an 1800 MHz clock with a per-SM rate below the slow chunks (560 MAC/clk/SM:
  60 CTA tiles per CTA at k = 4096), so a throttled GPU or an estimate that lags a clock drop stays
  under the 10 ms rule. Two chunks in flight so the GPU does not idle between launches; the chunk
  stream has the highest priority.
* Double buffering (opt-in, `Job::prepare_attempt`): the next attempt's A_L, A_R and A' are built
  into a spare set (m·k + m·128 + 2k + 4096 bytes, allocated on first use inside the budget) on a
  lowest-priority stream; `set_attempt` with the same seed and prefix swaps it in. The GEMM holds
  64,512 of the 65,536 registers of an SM, so the prep blocks only run where the GEMM leaves SMs
  idle (chunk tails, attempt boundaries); measured, it does not change the end-to-end rate (below).

## Resource figures (nvcc 13.0.88, `-O3 -gencode arch=compute_121a,code=sm_121a`)

| | `gemm_hash_kernel<MmaS8>` |
|---|---|
| ptxas `-v` | **168 registers** (launch budget of 384 threads), **0 bytes spill stores / loads, 0 stack** |
| MMA warps | 232 registers after `setmaxnreg.inc`; highest register used in SASS: R192 |
| shared memory | 99,408 B dynamic per CTA (4 × 24 KiB stages + 1 KiB alignment + barriers); limit 101,376 |
| threads | 384 (8 MMA warps + 1 producer warpgroup), 1 CTA per SM |
| SASS | `IMMA.16832.S8.S8` ×128, `LDSM.16.M88.4` ×32, `UTMALDG.2D` ×12, `UTMAPF.L2.2D` ×6, `SYNCS.*` ×37, `USETMAXREG`; **no `HMMA`**, no local memory (`LDL`/`STL`), no calls |
| SASS gate | `tools/check-sass.py` (run by `tools/merge-check.sh`): every `gemm_hash_kernel` instantiation must contain `IMMA.16832.S8.S8`, `LDSM`, `UTMALDG`, `SYNCS`, no `HMMA`/`STL`/`LDL`, STACK 0 and LOCAL 0 in `-res-usage`; no `HMMA` anywhere in the library |

Prep kernels: noised operand 40 registers / 2 KiB smem, pairs 30, uniform factor 27, int7 fill 19;
all 0 spills.

## Gate G0 (bit-exactness), 2026-09-26

`SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu -- --test-threads=1`: 17 tests
(5 unit, 11 G0, 1 layout), all pass; the G0 binary holds the GPU for 6.6 s.

| Test | Result |
|---|---|
| layout proof (`tests/layout.rs` → `cuda/tests/layout_check.cu`, host only): the kernel's lane geometry through the TMA SWIZZLE_64B layout (CuTe `Swizzle<2,4,3>`, tied to the descriptor's `OPERAND_SWIZZLE`), `ldmatrix.x4` as the PTX ISA defines it and the m16n8k32 A/B/C layouts of CuTe's `MMA_Traits` for `MmaS8` and `MmaE4M3`: every A'/B'ᵀ register byte is the (row, k) the atom expects, each warp reads its 64-row slabs once without bank conflicts, each lane's `acc[4][8][4]` is exactly the hash tile (`lane_row`, `lane_col`), the CTA tile is covered once; `dump_index` is the reference order on 6 shapes with partial CTA tiles; the band raster visits every CTA tile once (240 grid × band cases); the transcript slot rotation holds for 1..64 slices | **pass**; 10 mutations of the geometry (chunk, swizzle, B matrix order, A rows, tile row/col, dump index, warp grid, last band, rotation) are all caught |
| m, n ∈ {256, 512, 1024} × k ∈ {2048, 4096} × 3 seeds (54 problems, 150,528 tiles), dump vs `spm_cpuref::transcripts`, `first_mismatch == None`, equal `tiles_digest`, GPU hit ring = oracle hits | **pass, 0 mismatches** (GPU phase 0.11 s) |
| odd shapes 192×320×2112, 64×64×2048, 320×192×4160, 128×512×2048 (partial CTA tiles, k mod 128 ≠ 0), host-supplied operands with ±64 entries, 1 KiB / 4 KiB nonce prefix | **pass** |
| ragged and long k: 128×256×2112, 256×128×2176 (17 slices: the epilogue undoes a rotation of 1), 128×128×6144 (48 slices), dump vs oracle | **pass** |
| 1000 random (B, A) seed pairs, one job each: B_Rᵀ, B_L, B'ᵀ and A_L, A_R, A' vs the zk-pow generators and `add_noise` | **pass** (3.9–5.1 s) |
| hit ring overflow: all-ones bound, 100 entries | total = 512 tiles, 100 stored, each a real tile with its digest |
| chunking invariance, 16384² × 2048 (~32768 hits): adaptive over 3 attempts, 240, 1000 and 7 CTA tiles per chunk | identical hit sets, none dropped |
| prepared attempts (double buffering), 320×448×2176: prefixes of 0 / 64 / 4096 bytes, prepared set kept across an unrelated attempt, prepared seed with the wrong prefix; a job at its exact budget refuses to prepare | dump, A' and hits equal the oracle every time; `GpuError::Budget`, job keeps working |
| noise stage: GPU A_L / A_R for 1000 A seeds and B_Rᵀ / B_L for 64 B seeds vs the official `generate_uniform_random_matrix` / `generate_permutation_matrix` (via `spm_cpuref::noise_factors`); E_A, E_Bᵀ, A', B'ᵀ and the generated A, Bᵀ vs the oracle | **pass** |
| forced hits: bound = smallest digest → exactly that tile; every GPU hit at an easy bound → `build_plain_proof` + `verify_v3` (`check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`) + `check_rank_penalty` | **135 PlainProofs verified**; tampered leaf data or shifted row indices fail |
| abort: flag set mid-chunk (16384² × 4096) | chunk stops, `run_chunk` returns 37–72 µs after the flag |
| pipelined 8-tile chunks, abort between calls, resume (2048² × 4096, dump) | dump and hits still equal the oracle |
| `compute-sanitizer` memcheck / synccheck | 0 errors (memcheck `--leak-check full` also on the ragged, hit-ring and prepared-attempt tests: 0 errors, 0 bytes leaked) |
| `compute-sanitizer` racecheck | 4 WAR reports on the 4-byte per-stage command word: the consumer read and the next producer write are ordered by the stage's `empty` mbarrier, which racecheck does not model (false positives); TMA stage buffers are not tracked by racecheck |

## Throughput

Credited MACs = m·n·k per attempt. The vLLM was resident (idle) and other GPU/CPU jobs ran on the
machine at the same time, so the SM clock was not locked; numbers are given with the clock they were
measured at and as a fraction of the MB1 register-only peak at that clock (919 MAC/clk/SM × 48).
The first table is from the merge (before the chunk change below).

| Run | Kernel-only | End-to-end | SM clock | Board power | Chunk (mean / max) | vs 96.0 | vs MB1 peak at clock |
|---|---|---|---|---|---|---|---|
| bench, 16384² × 4096, 10 s (729 attempts) | **84.8 T-MAC/s** | 80.1 T-MAC/s | 2273 MHz | 84 W | 6.48 / 9.18 ms | 88.3 % | 84.6 % |
| default job shape 131072² × 4096, 3 attempts | 84.9 T-MAC/s | 82.8 T-MAC/s | 2282 MHz | 90 W | ≤ 9.6 ms | 88.5 % | 84.4 % |

End-to-end includes the per-attempt A-side prep (0.51 ms at 16384², 3.3 ms at 131072²) and the
launch gaps.

### Chunk length (2026-09-26, after the grafts)

The bench now drives `run_chunk` and records every chunk (`--csv`), the abort latency, other CUDA
processes, host CPU load, idle power and the clock event reasons. Runs below: only the idle vLLM
besides the bench, host CPU 7–10 % busy unless noted, clock not locked; one timed attempt after a
warm-up at the default job shape (`--m 131072 --n 131072 --seconds 0`), 3 s at 16384².

| Run | SM clock | Chunks | Chunk ms mean / p50 / p99 / **max** | CTA tiles per chunk | Kernel-only | End-to-end (% of MB1 at clock) |
|---|---|---|---|---|---|---|
| 131072², before, 8 ms target (the old bench default the judge used) | 2333 MHz | 107 | 7.49 / 7.44 / 9.12 / **9.34** | ~4950 | 87.83 | 86.13 (83.7 %) |
| 131072², before, 6 ms target (old library default) | 2339 MHz | 140 | 5.72 / 5.87 / 6.67 / **6.77** | ~3700 | 87.85 | 86.07 (83.4 %) |
| 131072², **after**: 4.5 ms target + ceiling | 2317 MHz | 185 | 4.19 / 4.20 / 5.46 / **5.56** | 2858 (cap 2880) | 90.79 | 86.71 (84.8 %) |
| 131072², after, 20 ms target (the ceiling binds) | 2300 MHz | 183 | 4.28 / 4.16 / 6.15 / **6.92** | 2865 | 89.78 | 85.79 (84.6 %) |
| 131072², after, host CPU 94 % busy (other builds), clock pulled down | 1868 MHz | 234 | 4.28 / 4.32 / 5.06 / **5.86** | 2249 | 70.19 | 66.69 (80.9 %) |
| 16384², before, 6 ms | 2337 MHz | 679 | 4.13 / 4.06 / 6.83 / **7.73** | 2731–4096 | 89.83 | 83.82 (81.3 %) |
| 16384², **after** | 2341 MHz | 710 | 3.93 / 4.02 / 4.92 / **5.65** | 2731 | 89.95 | 83.29 (80.7 %) |

* The time per CTA tile per CTA varies from chunk to chunk: 61–94 µs at ~2330 MHz (mean 72) on the
  131072² job, and isolated chunks slowed by other load on the SoC reach 116 µs. The ceiling is sized
  on ~560 MAC/clk/SM at 1800 MHz: the slow chunks would take ~8 ms at that clock and the 116 µs
  outlier ~8.8 ms, both under the 10 ms rule. Every run above stayed ≤ 8 ms (worst 6.92 ms).
* Abort latency (flag raised at 25/50/75 % of an attempt, until `run_chunk` reports the abort):
  0.06–0.31 ms in these runs, 0.59 ms once with the host loaded.
* Power: 16 W idle before the run, 83–90 W mean at 131072² (~1.0 T-MAC/s per W kernel-only,
  1.2–1.3 per W over idle), 75–77 W at 16384²; no clock event reason was reported.
* The end-to-end rate at the measured clock does not change with the shorter chunks. The
  event-timed kernel-only rate does rise (85 → 89 % of MB1 at clock at 131072²), because the stream
  time outside the chunks' event windows grows from 1.7 % (5.7 ms chunks) to 4.4 % (4.2 ms chunks),
  i.e. ~0.1–0.2 ms per chunk boundary: kernel-only numbers are an upper bound, quote end-to-end.
* The bench's detection works: one run that collided with another agent's `gpu_e2e` test was
  flagged `SHARED: numbers not reliable` and discarded.

### Double-buffered A' (`--prepare 1`)

| Run | SM clock | set_attempt | Kernel-only | End-to-end (% of MB1 at clock) |
|---|---|---|---|---|
| 16384², 3 s, A side built by set_attempt | 2338 MHz | 0.51 ms | 90.30 | 83.88 (81.3 %) |
| 16384², 3 s, prepared during the previous attempt | 2337 MHz | 0.17 ms | 88.86 | 83.83 (81.3 %) |
| 131072², 2 s, built by set_attempt | 2319 MHz | 3.22 ms | 88.88 | 85.17 (83.3 %) |
| 131072², 2 s, prepared (host CPU 30 % busy during the run) | 2229 MHz | 0.01 ms | 84.59 | 81.09 (82.5 %) |

The swap removes the prep from `set_attempt`, but the prep kernels then run inside the chunks'
windows (kernel-only drops by the same amount): with the GEMM holding nearly the whole register
file they cannot overlap it, only fill its idle tails. No end-to-end gain is measurable in these
runs (the ideal at 131072² is 0.4 %), so `prepare_attempt` stays opt-in and the bench default is
off; it should be re-measured with the clock locked before the worker uses it.

### Development A/B (strategy C)

Interleaved A/B medians (3–4 rounds of 3 s each) during development:

| Variant (16384² × 4096) | % of MB1 peak at the measured clock |
|---|---|
| producer on thread 0, refill one k-tile late, 4 stages | 79.4 |
| same with 3 stages | 75.1 |
| no loads at all (compute + fold + epilogue only) | ~90 |
| **warp-specialized producer (setmaxnreg)** | 84.5 |
| + L2 prefetch of A and B from every CTA, 4/8/16 k-tiles ahead | 72.9 / 79.3 / 79.6 |
| + L2 hints: A evict_last / A evict_last + B evict_first (same session: 84.6 without) | 85.1 / 84.7 (within noise) |
| + row pitch padded by 64/128/256 B (same session: 84.4 without) | 83.1 / 82.8 / 84.5 (no gain) |
| **+ B prefetch by the band's first CTA row, 4 k-tiles ahead** (final; same session: 84.4 without) | 86.0 (8 ahead: 85.3) |
| band height 12 / 16 / 24 / 32 (warp-specialized kernel, noisy session) | 81.2 / **85.8** / 77.5 / 82.0 |

Without the fold or without the BLAKE3 epilogue the rate does not change measurably: the kernel is
bound by load latency. Raw TMA streaming of the same pattern reaches ~1.9–2.1 TB/s into shared
memory (`cuda/probes/tma_stream.cu`: consumers only wait and release), about twice what 85 T-MAC/s
needs.

## Device memory

`job_device_bytes(m, n, k, host_operands, dump, hit_capacity)` is the exact figure `spm_job_create`
checks against the 2 GiB budget. Default job (131072² × 4096, generated operands, 4096-entry hit
ring): **1,107,480,608 B (1056.2 MiB)**: A' 512 MiB + B'ᵀ 512 MiB + factors 32 MiB + pairs, prefix,
hit ring and counters. Host-supplied operands add A (512 MiB). Dump mode adds m·n/128 × 104 B and
is meant for G0-sized problems. The first `prepare_attempt` adds the spare A side
(m·k + m·128 + 2k + 4096 B: 528 MiB at the default shape, 1.56 GiB in total) if it fits the budget.

## Findings on sm_121a with CUDA 13.0

* TMA works: `cuTensorMapEncodeTiled` through `cudaGetDriverEntryPointByVersion` (no libcuda link),
  `cp.async.bulk.tensor` + mbarrier, SWIZZLE_64B/128B layouts as modelled
  (`cuda/probes/tma_swizzle.cu`).
* Spell the TMA destination `.shared::cta`. With `.shared::cluster`, ptxas emits a runtime check
  plus a call to `__cuda_syscall_cp_async_bulk_tensor_2d_tile_unicast`; the extern call also makes
  it ignore `setmaxnreg` (C7506) and cost 15 registers and a stack frame.
* A 9th (producer) warp puts 3 warps on one SM sub-partition and caps every thread at 168
  registers (spills); a whole producer warpgroup plus `setmaxnreg` is the way around it.
* TMA multicast works functionally in a 2-CTA cluster probe, but ptxas flags `.multicast::cluster`
  on sm_121a as a reduced-performance path, and since raw TMA bandwidth is not the limit it was not
  pursued.
* The GB10's CPU and GPU share one power budget: under heavy CPU load from other jobs the SM clock
  fell to 2.0–2.15 GHz (from ~2.3–2.4 GHz). Another CUDA process on the GPU time-slices with the
  kernel (a bench that collided with another bench measured 63 T-MAC/s).

## Reproduce

```sh
export CARGO_TARGET_DIR=$HOME/.cache/spark-pearl-miner/target-k-c   # the checkout path has spaces
git submodule update --init --depth 1 third_party/cutlass          # CuTe, for the layout proof
# build + unit tests (no GPU)
cargo test --release -p spm-gpu --features gpu
# G0 and the forced-hit / abort / pipeline / chunking tests (short GPU bursts, < 100 MiB except the
# abort test) plus the layout proof
SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu -- --test-threads=1 --nocapture
# layout proof only (host, no GPU; needs the CUTLASS submodule)
cargo test --release -p spm-gpu --test layout -- --nocapture
# throughput, 10 s at 16384^2 x 4096 (T-MAC/s, chunk percentiles, abort latency, SM clock, power,
# clock event reasons, other GPU processes, host CPU); --csv FILE writes one row per chunk
cargo run --release -p spm-gpu --features gpu --example bench -- --seconds 10
# default job shape (1.06 GiB on the device), one timed attempt; --prepare 1 for double buffering
cargo run --release -p spm-gpu --features gpu --example bench -- --m 131072 --n 131072 --seconds 0
# SASS gate on the library the build produced
python3 tools/check-sass.py
# ptxas figures as cargo warnings
SPM_PTXAS_VERBOSE=1 cargo build --release -p spm-gpu
# sanitizers
SPM_GPU_TESTS=1 compute-sanitizer --tool memcheck $CARGO_TARGET_DIR/release/deps/g0-<hash> g0_odd_shapes --test-threads=1
```

`SPM_NVCC_FLAGS` passes extra nvcc flags to the build (experiments), `SPM_CUDA_ARCH` changes the
target (default `sm_121a`).
