# Decisions log

| Date | Decision | By |
|---|---|---|
| 2026-09-26 | Repo `xXGuilasXx/spark-pearl-miner`, Apache-2.0, docs EN (primary) + PT-BR | user |
| 2026-09-26 | Fixed, disclosed 2.00 % developer fee; constants only in `crates/spm-fee/src/lib.rs` | user |
| 2026-09-26 | Default pools: HeroMiners BR → LuckyPool BR → Kryptex; dev-fee session on HeroMiners (Kryptex/LuckyPool fallbacks); worker name `devfee` | user |
| 2026-09-26 | One authorized (never submitting) session on HeroMiners with the user's wallet to capture the dialect | user |
| 2026-09-26 | On the author's Spark the miner runs as the 4th `spark-modo` runtime `miner` (exclusive GPU lease); public repo stays generic, integration in `contrib/spark-modo/` | user |
| 2026-09-26 | vLLM may be stopped for GPU windows with a warning before each stop; GPU clock locked at 2200 MHz during tests | user |
| 2026-09-26 | Toolchain installed at user level (rustup 1.98.1, CUTLASS v4.8.0 submodule, official zk-pow @3fe2267) | user |
| 2026-09-26 | Public from the first commit with a pre-alpha banner; commits authored as `xXGuilasXx <guilasamaral@gmail.com>` | user |
| 2026-09-26 | The initial fee address was an exchange deposit address; a self-custody wallet (official `oyster`/`oystercli`) is being created to replace it before v0.1.0 | user |
| 2026-09-26 | New Rust host + CUDA library instead of forking CPPminer (3 designs, 3 judges unanimous); daemon without CUDA + disposable `gpu-worker`; embedded web GUI on 127.0.0.1:4078 | design panel, accepted |
| 2026-09-26 | Every subagent runs on Opus 5.5 (`CLAUDE_CODE_SUBAGENT_MODEL`) | user |
| 2026-09-26 | New self-custody fee wallet created with the official `oystercli` (SPV); `DEV_WALLET = prl1pkqp…s90n` | user |
| 2026-09-26 | GPU clock cap 2000 MHz for the Balanced default (supersedes the 2200 MHz test lock above): G1 soak #1 at 2200 MHz reached 87 W and a 97.5 °C board; at 2000 MHz the miner sustains 73.9 T-MAC/s at ~63 W, GPU 72 °C | measured |
| 2026-09-27 | Default pool order Kryptex (8048, TLS) → HeroMiners BR → LuckyPool BR (supersedes the order above): Kryptex is the owner's recommended pool, all three verified live with 0 rejects; existing `config.toml` files are never rewritten | project plan (one revertable commit) |
| 2026-09-27 | Lean GUI: a 3-step setup (language, wallet, fee + Start), one dashboard, a Settings dialog (wallet, worker, language, three `host:port` pools); the Pools, Failover, Power, Fee, Logs and About screens leave the GUI but their API endpoints and CLI commands stay | project plan |
| 2026-09-27 | One commented `config.toml` written by `Config::to_toml()` (no `toml_edit`): Basic block (GUI) + Advanced DGX Spark preset; comments added by hand are not kept across a GUI save | project plan |
| 2026-09-27 | One-command installer (`packaging/install.sh`, no root) and release tarball with SHA256SUMS (`packaging/make-release.sh`); reproducible, attested builds come later | project plan |
| 2026-09-27 | Illustrated user manual EN/PT-BR (`docs/{en,pt-BR}/MANUAL.md`) with screenshots taken from a simulated miner by `tools/screenshots.sh` | user request |
