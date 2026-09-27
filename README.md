# spark-pearl-miner

**Open-source Pearl (PRL) miner built for the NVIDIA DGX Spark (GB10, `sm_121`, aarch64).**
Unofficial. Not affiliated with, sponsored by, or endorsed by NVIDIA or Pearl Research Labs.
_Português: [README.pt-BR.md](README.pt-BR.md)_

> **Status: pre-alpha (planning / bring-up).** Nothing here mines yet. Follow [TODO.md](TODO.md).

## What it is
- A Proof-of-Useful-Work (PearlHash, int7×int7→int32 GEMM) miner whose CUDA kernel targets the GB10's `mma.sync` INT8 tensor path natively (`sm_121a`), verified bit-exactly against the official `zk-pow` reference before any share is submitted.
- A daemon that never holds a CUDA context, a disposable `gpu-worker` process that does, and a local web GUI (EN/PT-BR) to set the **wallet** and **up to 3 pools** with **automatic failover**.
- Built for the DGX Spark's realities: the known hard power-off under sustained GPU load (clock cap + non-root governor), unified-memory pressure, and coexistence with a resident vLLM.

## Developer fee (disclosed)
`dev fee 2.00% → prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n @ br.pearl.herominers.com:1200 (HeroMiners), worker "devfee", 120 s slices, only while mining`
All fee constants live in one file, `crates/spm-fee/src/lib.rs`; CI fails if this README disagrees with it. **The fee wallet is not configurable**: there is no flag, environment variable, config key or API that can change it, and the GUI shows it read-only. Official releases are reproducibly built and attested so you can verify you run the original. No remote configuration, no obfuscation, no packed binaries. The fee switches itself off when your wallet is the fee wallet.

## Donations
If this project is useful to you, PRL donations are welcome at the same address:

`prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n`

## Pools
The miner ships with three pool presets and automatic failover between them: HeroMiners BR, LuckyPool BR and Kryptex (TLS on port 8048); see `docs/en/CONFIGURATION.md`. If you still have to pick one: I have mined PRL on **Kryptex** and never had a problem with its payouts. Signing up through my referral link costs you nothing and supports this project:

https://pool.kryptex.com/?ref=b2cfe3e2 (referral link)

## Honest expectations
A GB10 is expected to reach roughly 65–85 TH/s (credited, pool units) inside a safe 75–85 W envelope, which at September 2026 network conditions is about 1.6–2.0 PRL/day gross. Difficulty rose 36 % in the 30 days before this was written and the block subsidy decays about 4 % per month. A closed-source DGX Spark miner already exists; this project's claim is _open-source and auditable_, not _first_. Read `docs/en/VIABILITY.md` before spending money on hardware or electricity.

## Requirements (target)
DGX OS 7.x (Ubuntu 24.04, aarch64), CUDA 13.0 driver ≥ 580, Rust ≥ 1.88 to build, a Pearl wallet address (`prl1…`, bech32m). Optional: `sudo` once to install the boot-time GPU clock cap.

## Documentation
- [Architecture](docs/en/ARCHITECTURE.md) · [Kernel contract](docs/en/KERNEL.md) · [Benchmarks](docs/en/BENCHMARKS.md) · [Viability](docs/en/VIABILITY.md)
- [Power & thermal](docs/en/POWER-THERMAL.md) · [Coexistence with a resident LLM](docs/en/COEXISTENCE.md) · [Dual mining verdict](docs/en/DUAL-MINING.md)
- [Pool protocols](docs/protocol/) · [Decisions](docs/en/DECISIONS.md) · [TODO](TODO.md)

## License
Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE) (ISC: Pearl Research Labs and The Decred developers; BSD-3: NVIDIA CUTLASS).
