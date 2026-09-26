# Viability (snapshot 2026-09-26)

> Verdict from the research phase: **GO as an open-source pilot; NO-GO as an income plan.** Numbers below come from `docs/_data/facts.toml`; refresh them monthly. Everything about the future is an estimate.

## 1. What one DGX Spark can earn

| Item | Value | Source |
|---|---|---|
| Network hashrate | ~54.8 EH/s (MAC/s units) | WhatToMine, HeroMiners API |
| Difficulty | ~29.4 M | WhatToMine |
| Block reward | ~2,305.8 PRL, decaying ~4 %/month, no halvings | `node/chaincfg/params.go` |
| Observed block time | ~151 s (target 194 s) | HeroMiners API |
| Gross yield | **0.0241 PRL per TH/s per day** (= 306.9 × reward ÷ difficulty) | derived, matches hashrate.no 0.0247 |
| PRL price | US$1.30 (R$6.78); ATL US$0.14 on 2026-07-23, ATH US$1.76 on 2026-09-23 | CoinGecko |

**Unit:** in Pearl, 1 "hash" = 1 int7×int7 multiply-accumulate of the noised GEMM, normalized to noise rank 128. `1 TH/s = 10¹² MAC/s = 2 INT8 TOPS` of useful GEMM. Pools credit `diff × 2³²` MACs per accepted share.

**GB10 expectation (unmeasured on this unit yet):** the INT8 tensor peak measured by the community is ~215 TOPS, so the hard ceiling is ~107 TH/s. Best sm_120-class kernels reach 85–94 % of peak. Inside a *safe* 75–85 W envelope (see the power-off risk) we plan on **65–85 TH/s credited**. A closed-source DGX Spark miner claims ~76 TH/s at ~99 W.

| Hashrate | PRL/day gross | US$/day @ 1.30 | US$/day @ 0.70 | US$/day @ 0.30 |
|---|---|---|---|---|
| 60 TH/s | 1.45 | 1.88 | 1.01 | 0.43 |
| 75 TH/s | 1.81 | 2.35 | 1.27 | 0.54 |
| 90 TH/s | 2.17 | 2.82 | 1.52 | 0.65 |

Costs: 100–130 W at the wall while mining → 2.4–3.1 kWh/day → R$1.9–3.4/day (R$0.80–1.10/kWh) ≈ US$0.37–0.66/day. Pool fee 0–1 %, developer fee 2 %.
**Net at 75 TH/s and US$1.30: ≈ US$1.4–1.9/day (≈ US$45–55/month). Break-even price: ≈ US$0.18–0.43/PRL.** PRL traded at US$0.14 in July 2026.

On a Spark that also serves LLMs (like the author's), mining only happens while the GPU is otherwise idle, so real numbers are lower.

## 2. What the developer fee earns

2 % of gross: **≈ 0.036 PRL/day ≈ 1.1 PRL/month ≈ US$1.4/month per Spark mining 24/7** at today's price and difficulty. About **70 always-on installs per US$100/month**; twice that at a 50 % duty cycle. The only comparable product has 2 stars and 3 downloads (2026-09-26), so demand is unproven. On the author's own Spark the fee is zero (mining wallet = fee wallet ⇒ auto-off).

## 3. Headwinds (all measured, none hypothetical)
- Difficulty **+36 % in 30 days**; PRL-per-TH fell **−32 % in 10 weeks**; miner software alone doubled per-card rates between June and September 2026.
- Block subsidy decays smoothly: ~2,206 PRL in 1 month, ~1,794 in 6 months, ~1,435 in 12 months.
- If block time returns to the 194 s target, daily emission drops another ~22 %.
- Thin, concentrated liquidity: ~US$2.9 M/day volume, ~87 % on SafeTrade (a 1.9/5 Trustpilot exchange with withdrawal complaints). Daily emission ≈ 60 % of daily volume (sell overhang).
- Pools concentrated: Kryptex ~45–50 %, pearlhash.xyz ~25–28 %.

## 4. Two risks that can end the product with days of notice
1. **FP8 / certificate-v4 hard fork.** Official branch `fp8-scheme`, PR #311 (open, no mainnet height yet). After `Fp8ForkHeight`, every int8/v3 miner produces invalid shares. Past fork heights were set hours to days before activation. GB10's FP8 `mma.sync` (`QMMA.16832`) is *probably* bit-exact with the B200-pinned arithmetic (measured only on an RTX PRO 6000 so far). Gate **G3** tests this early; the mainloop is templated so an FP8 backend can follow. ~70 % of the code (pools, failover, fee, GUI, power, packaging) survives the fork.
2. **Hard power-off of the DGX Spark under sustained GPU load** (~88–92 W GPU draw). NVIDIA acknowledged it as a known issue (2026-07-27) and has shipped no fix; this unit already runs the newest firmware. Mitigation: clock cap at 2200 MHz (boot-time unit, root once), a non-root power governor with a **Balanced** default (75 W target, 85 W hard stop), staged soaks. Cost: ~9 % hashrate.

## 5. Stop / re-scope criteria
- G0 (bit-exactness) fails, or the Balanced soak powers the unit off.
- A mainnet `Fp8ForkHeight` is set before v0.1.0 and G3 fails.
- PRL stays below the break-even price for the author's electricity tariff.

## 6. Not financial advice
Nothing here is investment advice. Mining income in Brazil is taxable (IN RFB 2291/2025 "DeCripto"); consult an accountant.
