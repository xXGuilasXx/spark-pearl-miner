# Contributing

- **Branches**: every change goes on a branch (`feat/<milestone>-<topic>`, `fix/<topic>`, `docs/<topic>`); merge into `main` only when tests pass. `main` must always build and be publishable.
- **Clean room**: never open, copy or adapt code from `akoya-miner` (no license) or any closed miner binary. Permissively-licensed references (MIT/Apache/ISC/BSD) may be adapted with their notices kept in the file header and in `NOTICE`.
- **Consensus code is never re-implemented**: `zk-pow` and `pearl-blake3` are consumed as pinned git dependencies. Bumping the pin requires re-running the bit-exact suite.
- **Fee constants** live only in `crates/spm-fee/src/lib.rs`; run `tools/check-fee-consistency.py` after touching them or the READMEs.
- **Docs** are English first with a Portuguese (pt-BR) mirror; shared numbers come from `docs/_data/facts.toml`.
- **Build notes**: `cargo build --release` needs Rust ≥ 1.88 and CUDA 13 for the GPU crate. If your checkout path contains spaces, set `CARGO_TARGET_DIR` to a space-free directory (jemalloc's configure, pulled in by `zk-pow`, refuses prefixes with spaces); a local, untracked `.cargo/config.toml` with `[build] target-dir = "..."` also works.
- **Never mine in CI**, never contact pools from CI, never commit unredacted captures (the probe tools redact wallets automatically; run `sha256sum -c tests/fixtures/SHA256SUMS`).
