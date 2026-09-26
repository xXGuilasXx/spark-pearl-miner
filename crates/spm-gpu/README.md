# spm-gpu — libspm_cuda and its safe Rust wrapper

`build.rs` compiles `cuda/{common,prep,gemm,job}/*.cu` with nvcc for `sm_121a` (GB10) into
`libspm_cuda.a`; `src/` exposes it to the gpu-worker. This is the M5 v0 kernel, **strategy B of the
kernel panel (CuTe/CUTLASS 4.8 atoms)**; see `docs/en/KERNEL.md` for the contract and
`crates/spm-cpuref/README.md` for the algorithm it reproduces bit for bit.

## Layout

| Path | What |
|---|---|
| `cuda/common/blake3.cuh` | BLAKE3 compression, keyed single-block hash, unkeyed 128-byte hash (job key) and the noise-hash message builder; host + device. Adapted from the ISC `pearl-gemm/csrc/blake3` (notice kept in the header and in `NOTICE`). |
| `cuda/common/splitmix.cuh` | SplitMix64 fill, identical to `spm_cpuref::fill_int7` (A and Bᵀ are generated on the GPU, nothing is uploaded). |
| `cuda/common/u256.cuh` | little-endian U256 `<=` for the bound check. |
| `cuda/prep/` | A_L / B_Rᵀ uniform factors, the A_R / B_L permutation pairs, and the noised s8 operands A' = A + E_A, B'ᵀ = Bᵀ + E_Bᵀ. |
| `cuda/gemm/hash_gemm_config.cuh` | the CuTe pieces: MMA policy (int8 now, FP8 e4m3 behind `SPM_ENABLE_FP8_POLICY`), permuted TiledMMA, swizzled smem, cp.async and LDSM copy atoms. |
| `cuda/gemm/hash_gemm_cute.cu` | the fused kernel: GEMM + per-slice transcript + keyed BLAKE3 + bound compare, dump and mining variants. |
| `cuda/job/spm_job.cu` | the C ABI of `cuda/include/spm_cuda.h`: job buffers, B/A side prep, chunked launches, hit ring, dump and debug read-back. |
| `cuda/tests/layout_check.cu` | host-only proof that every thread's accumulators form exactly one hash tile. |
| `src/ffi.rs` | the only module with `unsafe` (every block has a SAFETY comment); the rest of the crate denies/forbids it. |
| `src/job.rs` | safe API: `Job::{create, patch_a, set_attempt, run_chunk, run, read_hits, read_dump, read_debug, info}`. |
| `tests/g0.rs` | gate G0 (feature `gpu`, `SPM_GPU_TESTS=1`). |
| `examples/bench.rs` | throughput / power / chunk-time / abort-latency bench. |

## The kernel (strategy B)

* **CTA tile 128 × 128 × 64, 4 warps of 64 × 64, 2 CTAs per SM.** The atom is
  `SM80_16x8x32_S32S8S8S32_TN` (`mma.sync.m16n8k32.s32.s8.s8.s32`). The TiledMMA uses a 2 × 2 warp
  layout with the permutation `M: (16,2,4):(1,64,16)`, `N: (8,2,8):(1,64,8)`, so warp `w` owns rows
  `64·(w%2) + [0,64)` and cols `64·(w/2) + [0,64)`. With the m16n8 accumulator layout lane `l` then
  holds rows `l/4 + {0,8,…,56}` × cols `2(l%4) + {0,1,8,9,…,56,57}` of its warp tile — exactly the
  8 × 16 hash tile of base `(64·row_block + l/4, 64·col_block + 2(l%4))`. `cuda/tests/layout_check.cu`
  checks this for all 128 threads (and that accumulator 0 is the tile base, which the epilogue
  uses), plus the A/B fragment coverage, before the kernel relies on it.
* **Pipeline:** 3-stage `cp.async.cg` (`SM80_CP_ASYNC_CACHEGLOBAL`, 16 B per thread) into
  `Swizzle<2,4,3>` K-major smem (48 KiB per CTA), `ldmatrix.x4` (`SM75_U32x4_LDSM_N`) into
  double-buffered register fragments, one `__syncthreads` per k-tile. Only `floor(k/128)·128`
  columns are streamed: the last `k mod 128` columns never enter the transcript.
* **Transcript:** after every second k-tile (one r = 128 slice) each thread XORs its 128 cumulative
  i32 accumulators (4 independent XOR chains) and does `t[s mod 16] = rotl13(t[s mod 16]) ^ fold`.
  The 16 words live in local memory (L1) indexed by the running slot — one load and one store per
  slice — which keeps the mainloop at 230 registers; the register shift-register variant
  (`-DSPM_TRANSCRIPT_IN_REGS=1`, 232 registers) measured ~3 % slower.
* **Epilogue:** `digest = BLAKE3_keyed(a_noise_seed, t[0..16])` (one compression), then either the
  104-byte `TileResult::dump_bytes` record at its reference-order index (dump mode) or
  `digest <= bound` (LE U256) → `atomicAdd` slot in the hit ring (mining mode).
* **Raster:** CTA tiles are walked in groups of 8 CTA rows (`SPM_GROUP_M`), so the group's A' strips
  stay in the 24 MiB L2 while B'ᵀ strips stream past (group 4 measured 74 T-MAC/s, 8 → 87, 16 → 85).
* **Chunks and cancellation:** a chunk is a contiguous range of CTA tiles; its size adapts to
  ~7 ms from the measured rate (a slowdown is followed immediately, a speed-up gradually).
  `spm_job_run` reads the caller's abort flag (atomic acquire load) before every chunk, so an abort
  takes effect within the running chunk and a later `spm_job_run` resumes exactly where the attempt
  stopped. Keeping two chunks queued (with a per-chunk gate so a queued chunk could skip itself on
  abort) was built and measured: at stock clocks the GB10 is power-bound, so removing the ~0.1 ms
  host round trip between chunks only made the chunks ~2 % slower and gained no throughput, and it
  was dropped for the simpler loop.
* **FP8 readiness (M12):** the kernel is templated on `HashGemmConfig<Policy, Stages>`; the policy
  supplies the MMA op, operand types and the accumulator → 32-bit view the fold uses.
  `Fp8E4M3Policy` (`SM120_16x8x32_TN<e4m3, e4m3, f32>`, QMMA.16832, same fragment layouts) is
  declared but not instantiated.

## Resource usage (nvcc 13.0.88, `-O3 -gencode arch=compute_121a,code=sm_121a`)

`ptxas -v` (`SPM_PTXAS_VERBOSE=1 cargo build --release -p spm-gpu`):

| Kernel | Registers | Stack frame | Spill stores / loads | Smem |
|---|---|---|---|---|
| `hash_gemm_kernel<Int8, 3 stages, mining>` | **230** | 72 B (transcript in L1) | **0 / 0** | 49,152 B dynamic + 1 KiB reserved |
| `hash_gemm_kernel<Int8, 3 stages, dump>` | 230 | 72 B | 0 / 0 | same |
| same, `-DSPM_TRANSCRIPT_IN_REGS=1` | 232 | 0 | 0 / 0 | same |
| `noised_kernel` | 40 | 0 | 0 / 0 | 2 KiB static |
| `pairs_kernel` / `uniform_kernel` / `fill_kernel` | 29 / 27 / 12 | 0 | 0 / 0 | — |

The register budget is enforced with `__maxnreg__(232)` (`SPM_GEMM_MAXNREG`); occupancy is 2 CTAs
(8 warps) per SM (`cudaOccupancyMaxActiveBlocksPerMultiprocessor`, 2 × 50,176 B ≤ 102,400 B smem
per SM).

SASS (`cuobjdump -sass` of `hash_gemm_cute.o`), per kernel instantiation:

| Opcode | Count |
|---|---|
| `IMMA.16832.S8.S8` | 64 (2 k-blocks × 32 MMAs of the k-tile loop body) |
| `LDSM.16.M88.4` | 24 |
| `LDGSTS.E.BYPASS.LTC128B.128` (cp.async.cg) | 24 |
| `BAR.SYNC.DEFER_BLOCKING` | 2 |
| `HMMA` / `QMMA` | **0** |

## G0 and the other GPU tests

`SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu -- --test-threads=1` (≈ 8 s, the GPU
part a fraction of it, < 20 MiB of device memory), all passing on 2026-09-26:

* `g0_every_shape_is_bit_exact`: m, n ∈ {256, 512, 1024} × k ∈ {2048, 4096} × seeds {1, 2, 3} —
  **54/54 problems, every tile's `(t_rows, t_cols, transcript, digest)` equal to
  `spm_cpuref::transcripts`** (`first_mismatch == None`, equal `tiles_digest`), and the job key the
  library computes equals the CPU commitment's.
* `ragged_and_long_k_are_bit_exact`: 128×256×2112 (k mod 128 = 64), 256×128×2176 (17 slices, the
  slot wraps mid-transcript), 128×128×6144 (48 slices).
* `noise_factors_and_operands_match_zk_pow`: 1000 fresh (a_noise_seed, b_noise_seed) pairs — A_L,
  B_Rᵀ, both pair tables and both noised operands equal `spm_cpuref::noise_factors` (the official
  zk-pow generators) and `add_noise`.
* `host_matrices_and_nonce_patch_are_bit_exact`: host-supplied A/Bᵀ with ±64 entries, and a nonce
  patched into A chunk 0 through both sources.
* `chunked_and_aborted_runs_give_the_same_tiles`: 6 synchronous chunks of 3 CTA tiles, and
  abort/resume between chunks.
* `abort_mid_run_resumes_exactly`: `run` aborted from another thread at ten different moments and
  resumed, in dump mode (bit-exact) and mining mode (exactly the oracle's hits, none duplicated).
* `forced_hits_become_verified_plain_proofs`: mining mode at an easy nbits on 512×512×2048,
  256×512×4096 and 1024×256×2048 — the GPU's hit set equals `Oracle::find_hits`, and **all 383 GPU
  hits become PlainProofs that pass `verify_v3` (with and without the nbits override) and
  `check_rank_penalty`**; a proof with a mutated row index is rejected; with the bound set to the
  smallest digest the GPU reports exactly that tile.

compute-sanitizer 2025.3.1 (CUDA 13.0) on the ragged/long-k problems, mining mode, chunked and
aborted/resumed runs and the host/patch paths: memcheck (with leak check) 0 errors,
racecheck 0 hazards, synccheck 0 errors, initcheck 0 errors.

## Throughput

Measured on 2026-09-26 at **stock clocks** (locking the clock needs root) with the owner's vLLM
resident but idle. Only runs that the bench reported as exclusive (no other process held a CUDA
context during the run) with the host CPU mostly idle are listed. Under load the SM clock follows
the SoC power budget — busy host CPUs moved it between ~1820 and ~2290 MHz in my runs — so every rate
is given with the clock it was measured at and as a fraction of the MB1 register-only peak at that
clock (919 MAC/clk/SM × 48 SMs).

**Harness run** — `cargo run --release -p spm-gpu --example bench -- --seconds 10 --per-chunk`,
m = n = 16384, k = 4096, 734 full attempts in 10.0 s (host CPU 9 % busy):

| Metric | Value |
|---|---|
| Credited rate over kernel GPU time | **85.73 T-MAC/s** at **2230 MHz** average SM clock (47 NVML samples, no clock-event reason) |
| Against the peaks | 89.3 % of 96.0 T-MAC/s (MB1 at the 2200 MHz cap); **87.2 %** of the MB1 register-only peak at 2230 MHz (98.4) |
| Credited rate over wall time, incl. A-side prep | 80.58 T-MAC/s (0.33 ms prep and ~0.1 ms host round trip per chunk on a 12.8 ms attempt) |
| Chunks | 1625, median 8064 CTA tiles; p50 **6.15 ms**, p99 8.78 ms, max 9.26 ms, none over 10 ms |
| Abort latency | 2.1 / 2.4 / 3.8 ms (flag raised from another thread mid-attempt → `run` returns) |
| GPU power | 90.5 W average, 94.6 W peak (16.3 W before the run, so +74 W); 0.95 T-MAC/s per GPU watt; 69 °C max |

**Default job shape** — `--m 131072 --n 131072 --seconds 0.5` (1 warm-up + 1 timed attempt):

| Metric | Value |
|---|---|
| Device memory of the job | **1,107,480,580 B (1056.2 MiB)** with the GPU fill; +512 MiB (A kept on the device) with host matrices |
| B-side prep (at create) / A-side prep (per attempt) | 3.81 ms / 2.81 ms |
| One attempt (7.04 × 10¹³ credited MACs) | 812.6 ms of GPU time, 0.84 s wall |
| Credited rate | **86.60 T-MAC/s** kernel, **84.03 T-MAC/s** wall incl. prep, at 2206 MHz (89.0 % of the MB1 peak at that clock) |
| Chunks | 126 per attempt, mean 6.45 ms, max **9.16 ms** |
| Abort latency | 3.1 / 2.6 / 5.5 ms |
| GPU power | 92.0 W average, 94.0 W peak (+74.5 W over the 17.5 W before the run) |

**Variants** (3–6 s runs, same conditions; kernel GPU time):

| Variant | T-MAC/s @ SM clock | % of MB1 peak at that clock |
|---|---|---|
| **default:** 3 stages (48 KiB), 2 CTAs/SM, L1 transcript, raster group 8 | 85.7–87.0 @ 2216–2238 MHz | 86.3–88.6 % |
| transcript in registers (`-DSPM_TRANSCRIPT_IN_REGS=1`, 232 regs) | 83.3 @ 2240 MHz | 84.3 % |
| raster group 4 / 16 (`-DSPM_GROUP_M=`) | 74.2 @ 2228 / 85.1 @ 2204 MHz | 75.5 % / 87.5 % |
| slice-major k-loop (two k-tiles unrolled per fold) | 85.7 @ 2220 / 81.7 @ 2135 MHz | 87.5 % / 86.7 % |
| 4 stages (64 KiB per CTA: only 1 CTA/SM fits the 100 KiB of smem per SM; `-DSPM_GEMM_STAGES=4`) | 74.7 @ 2348 MHz | 72.1 % |
| two chunks queued (paired against one at a time, alternating attempts in one process) | kernel −1.5…−2.6 %, wall −1.2…+0.4 % | — (power-bound: dropped) |

**Power.** At stock clocks the kernel holds the GPU at ~85–91 W with peaks of 93–98 W, i.e. about
1 T-MAC/s per GPU watt; the rate is set by the power budget more than by the instruction mix (that is
why closing the host gaps between chunks gained nothing). This sits inside the ~88–92 W band of the
known DGX Spark power-off issue (`docs/en/VIABILITY.md`), so sustained mining needs the 2200 MHz cap or
the Balanced governor (M11); no run here lasted more than 10 s.

## Reproduce

```bash
# the checkout path has a space: keep the target dir outside it
export CARGO_TARGET_DIR=$HOME/.cache/spark-pearl-miner/target
git submodule update --init --depth 1 third_party/cutlass

# build + ptxas report
SPM_PTXAS_VERBOSE=1 cargo build --release -p spm-gpu

# layout proof (host only)
nvcc -std=c++17 -I third_party/cutlass/include -I cuda/include -o /tmp/layout_check \
     cuda/tests/layout_check.cu && /tmp/layout_check

# SASS check
cuobjdump -sass $CARGO_TARGET_DIR/release/build/spm-gpu-*/out/hash_gemm_cute.o \
  | grep -oE "(IMMA|HMMA|QMMA|LDSM|LDGSTS)[A-Z0-9._]*" | sort | uniq -c

# G0 and the other GPU tests (short, < 2 GiB; fine with the vLLM resident)
SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu -- --test-threads=1 --nocapture

# sanitizers on the small problems
SPM_GPU_TESTS=1 compute-sanitizer --tool racecheck \
  $CARGO_TARGET_DIR/release/deps/g0-<hash> ragged chunked --test-threads=1

# throughput (~10 s at 16384² × 4096; the bench says whether another process used the GPU)
cargo run --release -p spm-gpu --example bench -- --seconds 10 --per-chunk --csv chunks.csv
# default job shape: memory, prep, chunk times, abort latency (one ~0.85 s attempt, ~1.06 GiB)
cargo run --release -p spm-gpu --example bench -- --m 131072 --n 131072 --seconds 0.5

# variants: SPM_NVCC_FLAGS="-DSPM_TRANSCRIPT_IN_REGS=1" | "-DSPM_GROUP_M=16" | "-DSPM_GEMM_STAGES=4"
# (4 stages need 64 KiB of smem per CTA, so only 1 CTA/SM fits)
```

## Limits

* m and n must be multiples of 128 (the CTA tile), k a multiple of 64 in [128, 65536], and
  (m/128)·(n/128) < 2³¹; config52 must be our configuration (r = 128, 8×16 pattern) — anything else
  is `SPM_E_SHAPE`.
* Dump mode stores 104 bytes per tile and is meant for G0-sized problems (the 2 GiB budget refuses
  it at production shapes).
* `SPM_SOURCE_FILL` keeps one override region of A (≤ 4096 bytes, the nonce); `SPM_SOURCE_HOST`
  keeps A on the device (+m·k bytes) and writes patches into it.
* A CUDA error (`SPM_E_CUDA`) after a launch may leave the context unusable; the worker should
  treat it as a compute fault and exit.
