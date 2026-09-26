# GPU kernel contract (M5, gate G0)

The algorithm is fixed by `crates/spm-cpuref/README.md` ("The algorithm the GPU must replicate"); this page adds the
engineering contract for the sm_121a implementation and how it is judged.

## Targets
- **Correctness first (G0):** for every G0 problem (m, n ∈ {256, 512, 1024}, k ∈ {2048, 4096}, 3 seeds) the GPU's
  per-tile `(t_rows, t_cols, transcript[16], digest)` records must equal `spm_cpuref::transcripts()` exactly, in the
  reference tile order (`t_rows` ascending outer, `t_cols` ascending inner). Compare with
  `spm_cpuref::first_mismatch` / `tiles_digest`.
- **Throughput:** measured in credited MACs per second (`m·n·k` per full pass). Ceilings measured on this unit
  (`docs/en/BENCHMARKS.md`): 108.6 T-MAC/s at stock, 96.0 at the 2200 MHz cap. Planning target ≥ 80 % of the peak at the
  clock in use; publish numbers only with the SM clock they were measured at.
- **Resource limits:** ≤ 101,376 B shared memory per block, ≤ 232 registers/thread with **0 spills** (`-Xptxas -v`),
  SASS must contain `IMMA.16832.S8.S8` and `LDSM`, never `HMMA`; whole job budget ≤ 2 GiB of device memory
  (A' 512 MiB + B'ᵀ 512 MiB at 131072², plus factors and the hit ring); no floating point anywhere in the PoW path.
- **Cancellation:** the abort flag is polled at least once per launch chunk; a chunk must finish in ≤ 10 ms (v0) so a
  `Pause`/`Release` from the daemon is honoured quickly (the owner's vLLM orchestration depends on it).

## Work decomposition
1. **Job setup (once per job):** generate A_base and Bᵀ (SplitMix64 fill from `spm_cpuref::problem`, or upload from
   host), Merkle roots on the CPU (v0) → `job_key`, `bound_b`, `b_noise_seed`; GPU builds B_L/B_Rᵀ, the permutation
   pairs and **B'ᵀ = Bᵀ + E_Bᵀ** (s8, exact), kept for the whole job.
2. **Attempt (per nonce):** host patches the nonce into A chunk 0 and updates `root_a` incrementally → `bound_a`,
   `a_noise_seed`; GPU builds A_L, the A pairs and **A' = A + E_A** (s8); then the fused GEMM.
3. **Fused GEMM + hash (the kernel):** C' = A'·B'ᵀ in i32 with `mma.sync.m16n8k32.s8.s8.s32`; after every r = 128
   wide k-slice, each hash tile XOR-folds its **cumulative** 128 accumulators into one u32 and does
   `t[s mod 16] = rotl13(t[s mod 16]) ^ fold`; after the last slice: `digest = blake3_keyed(a_noise_seed, t[0..16] LE)`,
   compare `U256_LE(digest) ≤ bound`; hits go to a mapped ring `(t_rows, t_cols, digest)`. In **dump mode** every
   tile writes its 104-byte `TileResult::dump_bytes` record instead.
4. **Hash tile ↔ fragments:** rows `t_rows + {0, 8, …, 56}` × cols `t_cols + {0, 1, 8, 9, …, 56, 57}`. In a 64 × 64
   warp tile made of 4 × 8 `m16n8` fragments, thread `lane` holds accumulator rows `{lane/4, lane/4 + 8}` and
   columns `{2·(lane%4), 2·(lane%4)+1}` of each fragment — so the 128 outputs of one hash tile are exactly the
   accumulators of **one thread**: the per-slice fold is thread-local, no shuffles, no shared memory.
5. **BLAKE3 in the epilogue:** one keyed compression of a single 64-byte block per tile (port the ISC
   `csrc/blake3/blake3.cuh` from the official monorepo — keep the ISC notice — or implement the compression function;
   it must match `blake3::keyed_hash`).

## Harness (what every implementation must ship)
- `crates/spm-gpu/tests/g0.rs`: runs only when `SPM_GPU_TESTS=1`; for each G0 shape builds `spm_cpuref::Problem`,
  runs the GPU in dump mode and asserts `first_mismatch == None`; also a forced-hit check: pick the smallest digest
  tile, verify the CPU-built `PlainProof` for it passes `verify_v3`.
- `spark-pearl-miner`-independent CLI or test `bench`: 10 s at m = n = 16384, k = 4096 (fits the vLLM-resident rule),
  printing credited T-MAC/s, the SM clock (NVML), and the chunk time.
- `cuda/gemm/debug_dump.cu` or equivalent dump path; `ptxas -v` numbers recorded in the crate README.

## Strategies evaluated by the panel (each in its own branch, judged on G0 then throughput)
- **A — `cp.async` classic:** CTA 128 × 256 × 64, 3 stages, 8 warps (2 × 4 of 64 × 64), `ldmatrix`, swizzled smem.
- **B — CuTe/CUTLASS atoms:** `SM80_16x8x32_S32S8S8S32_TN` + CuTe `cp.async` pipeline, CTA 128 × 128 × 64, 4 stages,
  2 CTAs/SM.
- **C — TMA + mbarrier persistent:** `cp.async.bulk.tensor` for B with an mbarrier ring, persistent 48 CTAs, L2-band
  raster, CTA 128 × 256 × 64 (the v1 design attempted directly).

Judging: (1) G0 pass/fail is a hard gate; (2) credited T-MAC/s at the same clock; (3) chunk cancel latency; (4) code
quality, license hygiene (no `akoya-miner` code ever), readability. The winner is merged; good ideas from the others are
grafted in v1.

## FP8 readiness (M12)
Template the mainloop on the MMA op so the same `ldmatrix`/fragment layout can drive
`mma.sync.m16n8k32.kind::f8f6f4.f32.e4m3.e4m3.f32` (`QMMA.16832`, same peak rate as INT8 on GB10 per MB1), with one
FP32 accumulation chain per output in ascending K and no split-K, for the certificate-v4 fork.
