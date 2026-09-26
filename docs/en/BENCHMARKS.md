# Benchmarks

All numbers are measured on the author's DGX Spark (GB10, 48 SMs, CC 12.1, driver 580.178.04, CUDA 13.0) and are reproducible with the scripts in `bench/`. Raw logs live in `docs/benchmarks/`.

## MB1 — register-only tensor-core peak (2026-09-26, `bench/mb1.sh`, binary sha256 `28bfcf4d…`)
`mma.sync.m16n8k32` with all operands in registers, 8 warps × 4 blocks per SM, 4 s per test, SM clock sampled through NVML.

| GPU clock | INT8 `IMMA.16832` | FP8 `QMMA.16832` (e4m3→f32) | MAC/clk/SM | max GPU power |
|---|---|---|---|---|
| stock (2462 MHz) | **108.6 T-MAC/s** (217 TOPS) | 107.0 T-MAC/s (214 TOPS) | 919 | 51 W |
| 2200 MHz (safety cap) | **96.0 T-MAC/s** (192 TOPS) | 95.9 T-MAC/s | 914 | 34 W |
| 2000 MHz | 85.0 T-MAC/s | 84.9 T-MAC/s | 893 | 26 W |
| 1800 MHz | 75.7 T-MAC/s | 75.8 T-MAC/s | 890 | 21 W |

What this means for PearlHash (1 pool TH/s = 10¹² int8 MAC/s):
- The hard ceiling is **~108 TH/s at stock clocks and ~96 TH/s at the 2200 MHz cap** we recommend against the known power-off issue.
- FP8 runs at the **same rate** as INT8, so the certificate-v4 (FP8) fork would not lower the ceiling on GB10.
- ~919 MAC/clk/SM is ~90 % of the 1024 MAC/clk/SM theoretical rate; the remaining 10 % is issue overhead of the register-only loop.
- Power with no memory traffic is low (51 W); a real kernel adds shared-memory and L2 traffic, so the 75 W Balanced target leaves headroom.
- Planning target for the v1 kernel: 80–90 % of the capped peak ⇒ **77–86 TH/s at 2200 MHz**.
