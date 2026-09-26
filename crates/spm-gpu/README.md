# spm-gpu — GPU worker library (M5, strategy C)

`spm-gpu` builds `libspm_cuda` (CUDA C++ for sm_121a, `cuda/`) with nvcc and wraps its C ABI
(`cuda/include/spm_cuda.h`) in a safe Rust API. It computes, for a PearlHash V3 job, the noised
int8 operands and every hash tile's transcript, digest and bound comparison, bit-identical to
`spm-cpuref` (gate G0).

```rust
let mut job = Job::new(&JobConfig::new(m, n, k, Operands::Generated { seed }, commitment.b_noise_seed))?;
job.set_attempt(&commitment.a_noise_seed, &bound_le_bytes)?; // A side of the attempt
job.run_attempt()?;                                          // chunks of <= ~10 ms, abortable
let (hits, total) = job.hits()?;                             // (t_rows, t_cols, digest)
```

The commitment (job key, Merkle roots, V3 salting, seed chain) stays on the CPU in v0, so a job takes
the derived seeds: `b_noise_seed` at creation, `a_noise_seed` per attempt. `unsafe` lives only in
`src/ffi.rs`; the other modules `#![forbid(unsafe_code)]`.

## Pipeline

| Step | Where | What |
|---|---|---|
| job create | `cuda/prep` | B_Rᵀ (n×128), B_L pairs (k), B'ᵀ = Bᵀ + E_Bᵀ (n×k s8); Bᵀ is the SplitMix64 int7 stream of the seed (never stored) or the uploaded matrix (noised in place) |
| set_attempt | `cuda/prep` | A_L (m×128), A_R pairs (k), A' = A + E_A (m×k s8), optional ≤ 4 KiB prefix override of A (nonce patch) |
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
* Host side (`cuda/common/job.cu`): chunks sized adaptively to 6 ms (EMA of the time per tile,
  never below 4 tiles per CTA), two chunks in flight so the GPU does not idle between launches.

## Resource figures (nvcc 13.0.88, `-O3 -gencode arch=compute_121a,code=sm_121a`)

| | `gemm_hash_kernel<MmaS8>` |
|---|---|
| ptxas `-v` | **168 registers** (launch budget of 384 threads), **0 bytes spill stores / loads, 0 stack** |
| MMA warps | 232 registers after `setmaxnreg.inc`; highest register used in SASS: R192 |
| shared memory | 99,408 B dynamic per CTA (4 × 24 KiB stages + 1 KiB alignment + barriers); limit 101,376 |
| threads | 384 (8 MMA warps + 1 producer warpgroup), 1 CTA per SM |
| SASS | `IMMA.16832.S8.S8` ×128, `LDSM.16.M88.4` ×32, `UTMALDG.2D`, `UTMAPF.L2.2D`, `USETMAXREG`; **no `HMMA`**, no local memory (`LDL`/`STL`), no calls |

Prep kernels: noised operand 40 registers / 2 KiB smem, pairs 30, uniform factor 27, int7 fill 19;
all 0 spills.

## Gate G0 (bit-exactness), 2026-09-26

`SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --test-threads=1`

| Test | Result |
|---|---|
| m, n ∈ {256, 512, 1024} × k ∈ {2048, 4096} × 3 seeds (54 problems, 150,528 tiles), dump vs `spm_cpuref::transcripts`, `first_mismatch == None`, equal `tiles_digest`, GPU hit ring = oracle hits | **pass, 0 mismatches** (GPU phase 0.11 s) |
| odd shapes 192×320×2112, 64×64×2048, 320×192×4160, 128×512×2048 (partial CTA tiles, k mod 128 ≠ 0), host-supplied operands with ±64 entries, 1 KiB / 4 KiB nonce prefix | **pass** |
| noise stage: GPU A_L / A_R for 1000 A seeds and B_Rᵀ / B_L for 64 B seeds vs the official `generate_uniform_random_matrix` / `generate_permutation_matrix` (via `spm_cpuref::noise_factors`); E_A, E_Bᵀ, A', B'ᵀ and the generated A, Bᵀ vs the oracle | **pass** |
| forced hits: bound = smallest digest → exactly that tile; every GPU hit at an easy bound → `build_plain_proof` + `verify_v3` (`check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`) + `check_rank_penalty` | **135 PlainProofs verified**; tampered leaf data or shifted row indices fail |
| abort: flag set mid-chunk (16384² × 4096) | chunk stops, `run_chunk` returns 37–72 µs after the flag |
| pipelined 8-tile chunks, abort between calls, resume (2048² × 4096, dump) | dump and hits still equal the oracle |
| `compute-sanitizer` memcheck / synccheck | 0 errors |
| `compute-sanitizer` racecheck | 4 WAR reports on the 4-byte per-stage command word: the consumer read and the next producer write are ordered by the stage's `empty` mbarrier, which racecheck does not model (false positives); TMA stage buffers are not tracked by racecheck |

## Throughput

Credited MACs = m·n·k per attempt. The vLLM was resident (idle) and other GPU/CPU jobs ran on the
machine at the same time, so the SM clock was not locked; numbers are given with the clock they were
measured at and as a fraction of the MB1 register-only peak at that clock (919 MAC/clk/SM × 48).

| Run | Kernel-only | End-to-end | SM clock | Board power | Chunk (mean / max) | vs 96.0 | vs MB1 peak at clock |
|---|---|---|---|---|---|---|---|
| bench, 16384² × 4096, 10 s (729 attempts) | **84.8 T-MAC/s** | 80.1 T-MAC/s | 2273 MHz | 84 W | 6.48 / 9.18 ms | 88.3 % | 84.6 % |
| default job shape 131072² × 4096, 3 attempts | 84.9 T-MAC/s | 82.8 T-MAC/s | 2282 MHz | 90 W | ≤ 9.6 ms | 88.5 % | 84.4 % |

End-to-end includes the per-attempt A-side prep (0.51 ms at 16384², 3.3 ms at 131072²) and the
launch gaps. Interleaved A/B medians (3–4 rounds of 3 s each) during development:

| Variant (16384² × 4096) | % of MB1 peak at the measured clock |
|---|---|
| producer on thread 0, refill one k-tile late, 4 stages | 79.4 |
| same with 3 stages | 75.1 |
| no loads at all (compute + fold + epilogue only) | ~90 |
| **warp-specialized producer (setmaxnreg)** | 84.5 |
| + L2 prefetch of A and B from every CTA, 4/8/16 k-tiles ahead | 72.9 / 79.3 / 79.6 |
| + L2 hints (A evict_last, B evict_first) | 84.7 (no gain) |
| + row pitch padded by 64/128/256 B | 83.1 / 82.8 / 84.5 (no gain) |
| **+ B prefetch by the band's first CTA row, 4 k-tiles ahead** (final) | 86.0 (8 ahead: 85.3) |
| band height 12 / 16 / 24 / 32 (final kernel) | 81.2 / **85.8** / 77.5 / 82.0 |

Without the fold or without the BLAKE3 epilogue the rate does not change measurably: the kernel is
bound by load latency. Raw TMA streaming of the same pattern reaches ~1.9–2.1 TB/s into shared
memory (`cuda/probes/tma_stream.cu`: consumers only wait and release), about twice what 85 T-MAC/s
needs.

## Device memory

`job_device_bytes(m, n, k, host_operands, dump, hit_capacity)` is the exact figure `spm_job_create`
checks against the 2 GiB budget. Default job (131072² × 4096, generated operands, 4096-entry hit
ring): **1,107,480,608 B (1056.2 MiB)**: A' 512 MiB + B'ᵀ 512 MiB + factors 32 MiB + pairs, prefix,
hit ring and counters. Host-supplied operands add A (512 MiB). Dump mode adds m·n/128 × 104 B and
is meant for G0-sized problems.

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
# build + unit tests (no GPU)
cargo test --release -p spm-gpu --features gpu
# G0 and the forced-hit / abort / pipeline tests (short GPU bursts, < 50 MiB except the abort test)
SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --test-threads=1 --nocapture
# throughput, 10 s at 16384^2 x 4096 (prints T-MAC/s, SM clock, power, chunk times, % of 96)
cargo run --release -p spm-gpu --features gpu --example bench -- --seconds 10
# default job shape (1.06 GiB on the device)
cargo run --release -p spm-gpu --features gpu --example bench -- --m 131072 --n 131072 --seconds 3
# ptxas figures as cargo warnings
SPM_PTXAS_VERBOSE=1 cargo build --release -p spm-gpu
# SASS opcode check
nvcc -O3 -std=c++17 -I cuda/include -gencode arch=compute_121a,code=sm_121a -Xptxas -v \
     -c cuda/gemm/gemm_tma.cu -o /tmp/gemm_tma.o
cuobjdump -sass /tmp/gemm_tma.o | grep -oE 'IMMA\.[A-Z0-9.]+|HMMA[A-Z0-9.]*|LDSM[A-Z0-9.]*|UTMALDG[A-Z0-9.]*|STL|LDL' | sort | uniq -c
# sanitizers
SPM_GPU_TESTS=1 compute-sanitizer --tool memcheck $CARGO_TARGET_DIR/release/deps/g0-<hash> g0_odd_shapes --test-threads=1
```

`SPM_NVCC_FLAGS` passes extra nvcc flags to the build (experiments), `SPM_CUDA_ARCH` changes the
target (default `sm_121a`).
