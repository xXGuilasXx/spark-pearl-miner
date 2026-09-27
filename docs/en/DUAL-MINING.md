# Dual mining on the DGX Spark — verdict (2026-09-26)

**Recommendation: no energy-consuming dual mining.** The only zero-energy add-on (NOCK merged mining) is worth cents and only makes sense while the miner is on its LuckyPool fallback. Baseline: PRL at ~80 TH/s ≈ 1.85–1.94 PRL/day ≈ US$2.4–2.7/day (HeroMiners, 0 % fee).

## 1. Merged mining (same shares, 0 W extra)
| Option | Status | Yield at 80 TH/s | Notes |
|---|---|---|---|
| HeroMiners + MDL | **dead** | ~0.34 MDL/day ≈ US$0.0001 | MDL notices commented out on the pool page, `modelos.herominers.com` NXDOMAIN, MDL ≈ US$0.0004 with ~US$1k/day volume |
| LuckyPool + NOCK (Nockchain AI-PoW) | live | ~0.37 NOCK/day ≈ **US$0.011/day** | wallet string `PRL_ADDR+NOCK_ADDR[.worker]` (native base58 Nockchain address); 50 NOCK minimum ≈ 134 days; AI difficulty doubled in a week |
| Kryptex | none | — | — |

Moving the *primary* pool to LuckyPool to get NOCK loses money: its 1 % PRL fee (US$0.024–0.028/day) exceeds the NOCK income. On the **fallback only** the fee is already paid, so NOCK is pure upside. Implementation: an optional "NOCK address" field applied only to LuckyPool slots (M9, low priority).

## 2. CPU dual mining (RandomX / XMR) — measured here
`xmrig --bench=1M` (offline, no pool), GPU idle: 20 threads 7.45 kH/s; 10 × Cortex-X925 5.30 kH/s; 10 × Cortex-A725 3.37 kH/s. At ~US$0.036 per kH/s/day (XMR), gross US$0.27 / 0.19 / 0.12 per day.
- **X925 load heats the SoC from ~50 °C to 84 °C in ~30 s and 87.4 °C at 60 s, still rising**, inside the range where the DGX Spark's embedded controller hard-powers-off (community reports ~87–98 °C hotspot) — before adding the PRL GPU heat on the same die. **Disqualified.**
- A725-only settles at ~56–57 °C, but a 5 % PRL loss (US$0.12–0.135/day) already erases its whole gross; the CPU and GPU share one ~140 W SoC budget that NVML cannot see, so the GPU governor cannot account for CPU watts. **Not worth it** (marginal at best, only with a wall meter, an `acpitz` stop at ~80 °C and a 24 h A/B test).

## 3. Second GPU algorithm
No GPU coin beats PRL on this chip (scaled from RTX 5090: Quantus ≈ US$1.5–1.9/day at 100 % of the GPU; every memory-bound coin ≤ US$0.40/day given 273 GB/s). Time-slicing only splits GPU time (net ≈ −US$0.25 to −US$1.10/day at 50/50); true co-running is blocked because the PRL CTA uses ~90 % of the SM register file and ~72 KB of shared memory; the 75 W governor would take any extra watts away from PRL; and no closed dual miner ships for aarch64. **No.**

## 4. What to do with spare power instead
A higher PRL clock would be worth about +5–10 % per 100–200 MHz (≈ +US$0.12–0.27/day), more than every dual option combined, but it is on hold: G1 soak #1 at 2200 MHz already reached 87 W and a 97.5 °C board, so the Balanced default is 2000 MHz and 2300–2400 MHz would sit inside the power-off band. Treat as an estimate; revisit only with new power-off evidence.

Energy assumptions: R$0.80–1.10/kWh ≈ US$0.15–0.21; 1 W sustained ≈ R$0.019–0.026/day.
