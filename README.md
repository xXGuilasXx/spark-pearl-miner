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
`dev fee 2.00% → prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh @ br.pearl.herominers.com:1200 (HeroMiners), worker "devfee", 120 s slices, only while mining`
All fee constants live in one file, `crates/spm-fee/src/lib.rs`; CI fails if this README disagrees with it. No remote configuration, no obfuscation, no packed binaries. The fee switches itself off when your wallet is the fee wallet.

## Honest expectations
A GB10 is expected to reach roughly 65–85 TH/s (credited, pool units) inside a safe 75–85 W envelope, which at September 2026 network conditions is about 1.6–2.0 PRL/day gross. Difficulty rose 36 % in the 30 days before this was written and the block subsidy decays about 4 % per month. A closed-source DGX Spark miner already exists; this project's claim is _open-source and auditable_, not _first_. Read `docs/en/VIABILITY.md` before spending money on hardware or electricity.

## Requirements (target)
DGX OS 7.x (Ubuntu 24.04, aarch64), CUDA 13.0 driver ≥ 580, Rust ≥ 1.88 to build, a Pearl wallet address (`prl1…`, bech32m). Optional: `sudo` once to install the boot-time GPU clock cap.

## License
Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE) (ISC: Pearl Research Labs and The Decred developers; BSD-3: NVIDIA CUTLASS).
