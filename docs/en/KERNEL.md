# GPU kernel contract (M5, gate G0)

The algorithm is fixed by `crates/spm-cpuref/README.md` ("The algorithm the GPU must replicate"); this page adds the
engineering contract for the sm_121a implementation and how it is judged.

## Status (2026-09-26)
- **Strategy C is merged** (`cuda/gemm/gemm_tma.cuh`, `cuda/common/job.cu`, `crates/spm-gpu`): persistent 48 CTAs,
  TMA + mbarrier ring with a warp-specialized producer (`setmaxnreg`), L2 band raster, 168 registers / 0 spills,
  `IMMA.16832.S8.S8`, no `HMMA`. G0 passes; see `crates/spm-gpu/README.md` for the resource figures, the test list and
  every measurement with its SM clock.
- **A and B are reference branches**, not built: `ref/kernel-a-cpasync` (cp.async, 3 stages) and `ref/kernel-b-cute`
  (CuTe atoms, 2 CTAs/SM). Their good ideas were hand-ported into C (the ABIs differ):
  - adaptive chunk target lowered from 6 ms to 4.5 ms, with a hard ceiling of 8 ms of work at an 1800 MHz clock
    (60 CTA tiles per CTA at k = 4096): the max chunk at the default job shape went from 9.34 ms (8 ms target) /
    6.77 ms (6 ms) to 5.56 ms at ~2320 MHz, and never above 6.92 ms even with the ceiling binding;
  - `tools/check-sass.py` (from A), retargeted to `gemm_hash_kernel` (`IMMA.16832.S8.S8`, `LDSM`, `UTMALDG`, `SYNCS`
    present; `HMMA`, `STL`, `LDL` absent; no stack or local memory), run by `tools/merge-check.sh`;
  - `cuda/tests/layout_check.cu` (from B), rewritten for C's hand-written fragments: a host-side proof, on the kernel's
    own `__host__ __device__` lane geometry and CuTe's m16n8k32 layouts for `MmaS8` and `MmaE4M3`, that each lane's 128
    accumulators are exactly one 8 × 16 hash tile (`cargo test -p spm-gpu --test layout`, no GPU);
  - bench instrumentation (from B): per-chunk CSV, p50/p99/max, measured abort latency, other GPU processes, host CPU,
    idle-power delta, T-MAC/s per W, clock event reasons;
  - G0 cases from B (ragged / long k, 1000 random noise seed pairs) and A (hit-ring overflow, chunking invariance);
  - double-buffered A' (`prepare_attempt`, an M10 item), opt-in: bit-exact, but no measurable end-to-end gain yet,
    because the GEMM's register footprint leaves the prep kernels only the idle tails of the chunks.

### Left for v1 (M10)
- Measure with the vLLM stopped and the clock locked at 2200 MHz (every number so far is from a shared, unlocked
  machine: 81–85 % of the MB1 peak at the measured clock end-to-end, 85–89 % kernel-only).
- The stream time outside the chunks' event windows: 1.7 % of an attempt with 5.7 ms chunks, 4.4 % with 4.2 ms
  (~0.1–0.2 ms per chunk boundary). Find where it goes (launch of the 99 KB-smem kernel, the per-chunk memset and
  status copy, smem carveout changes) and remove it; quote end-to-end rates until then.
- The chunk-to-chunk spread of the time per CTA tile (61–94 µs at ~2330 MHz on 131072² × 4096) and the isolated
  slower chunks under SoC load: understand them, then revisit the 4.5 ms target and the ceiling.
- Re-measure `prepare_attempt` with the clock locked; adopt it in the worker only if it pays.
- Per-tile epoch check in the kernel (a job switch without draining), and the worker integration (`crates/spm-worker`).
- `MmaE4M3` instantiation once the v4 fold is specified (M12); the layout proof already covers its fragments.

## Targets
- **Correctness first (G0):** for every G0 problem (m, n ∈ {256, 512, 1024}, k ∈ {2048, 4096}, 3 seeds) the GPU's
  per-tile `(t_rows, t_cols, transcript[16], digest)` records must equal `spm_cpuref::transcripts()` exactly, in the
  reference tile order (`t_rows` ascending outer, `t_cols` ascending inner). Compare with
  `spm_cpuref::first_mismatch` / `tiles_digest`.
- **Throughput:** measured in credited MACs per second (`m·n·k` per full pass). Ceilings measured on this unit
  (`docs/en/BENCHMARKS.md`): 108.6 T-MAC/s at stock, 96.0 at the 2200 MHz cap. Planning target ≥ 80 % of the peak at the
  clock in use; publish numbers only with the SM clock they were measured at.
- **Resource limits:** ≤ 101,376 B shared memory per block, ≤ 232 registers/thread with **0 spills** (`-Xptxas -v`),
  SASS must contain `IMMA.16832.S8.S8` and `LDSM`, never `HMMA` (enforced by `tools/check-sass.py` in
  `tools/merge-check.sh`); whole job budget ≤ 2 GiB of device memory
  (A' 512 MiB + B'ᵀ 512 MiB at 131072², plus factors and the hit ring); no floating point anywhere in the PoW path.
- **Cancellation:** the abort flag is polled at least once per launch chunk; a chunk must finish in ≤ 10 ms (v0) so a
  `Pause`/`Release` from the daemon is honoured quickly (the owner's vLLM orchestration depends on it). Strategy C also
  polls it before every CTA tile (measured: `run_chunk` reports an abort 0.06–0.3 ms after the flag), sizes chunks to
  4.5 ms and caps them at 8 ms of work at an 1800 MHz clock.

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
- `cuda/tests/layout_check.cu` (+ `crates/spm-gpu/tests/layout.rs`): host-side proof of the fragment ↔ hash-tile
  mapping, and `tools/check-sass.py` for the SASS rules.
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
grafted in v1. Outcome: C won and was merged; the grafts are listed under Status above.

## FP8 readiness (M12)
Template the mainloop on the MMA op so the same `ldmatrix`/fragment layout can drive
`mma.sync.m16n8k32.kind::f8f6f4.f32.e4m3.e4m3.f32` (`QMMA.16832`, same peak rate as INT8 on GB10 per MB1), with one
FP32 accumulation chain per output in ascending K and no split-K, for the certificate-v4 fork. Strategy C's mainloop is
templated on `MmaS8` / `MmaE4M3`; the layout proof checks that `MmaE4M3` (CuTe `SM120_16x8x32_TN` e4m3) has the same
fragment mapping, so only the v4 fold and the instantiation remain.
