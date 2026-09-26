# Live protocol captures (redacted)

Each `capture-<pool>-authorize.jsonl` file is a real, timestamped exchange with a Pearl pool recorded by
`tools/spm-probe.py` on 2026-09-26 from São Paulo, Brazil. The probe only **authorizes and listens**; it never
sends `mining.submit`, so no proof and no share ever appears here. Before writing, the wallet address is
replaced by `<WALLET>`. Job digests are SHA-256 of the notify params (for dedup), and full notify messages are kept
because the 76-byte header and the target are public chain data.

| File | Pool / endpoint | Transport | Dialect | Notes |
|---|---|---|---|---|
| `capture-herominers-br-authorize.jsonl` | HeroMiners, `br.pearl.herominers.com:1200` | TLS (Let's Encrypt) | object, authorize-first | static diff 2,097,152; jobs every ~28 s |
| `capture-luckypool-br-authorize.jsonl` | LuckyPool, `pearl-br.luckypool.io:3360` | TLS only, self-signed `*.luckypool.io` (pinned) | object + `jsonrpc`, ack carries `type` | vardiff (`diff` field, 888,888 for a fresh worker) |
| `capture-kryptex-8048-authorize.jsonl` | Kryptex, `prl-br.kryptex.network:8048` | TLS (public CA) | stratum-v1 arrays (`subscribe` silent, `authorize` acked) | static diff 2,097,152; `target = 2^224/diff − 1` |

`crates/spm-proto/tests/pool_fixtures.rs` replays every file: our generated handshake must match the accepted
one, and every recorded job must parse. `SHA256SUMS` pins the files; regenerate it whenever a capture is refreshed.

# CPU oracle golden files

Each `golden-cpuref-<seed>.json` is produced by `crates/spm-cpuref` for `Problem::generate(m, n, k, header, seed)`
(the header is recorded as `header76`): shape, job key, Merkle roots, salted roots, both noise seeds, the first 8
tiles in full (base offsets, 16-word transcript, jackpot digest) and `tiles_blake3`, a BLAKE3 over every tile's
104-byte dump record in order. They are the fixed targets of gate G0.

| File | m × n × k | Tiles |
|---|---|---|
| `golden-cpuref-1.json` | 256 × 256 × 2048 | 512 |
| `golden-cpuref-2.json` | 256 × 512 × 4096 | 1024 |
| `golden-cpuref-3.json` | 512 × 256 × 4096 | 1024 |

`crates/spm-cpuref/tests/golden.rs` recomputes them (and seed 1 again through the zk-pow reference pieces). Only
after an intentional change: `SPM_UPDATE_GOLDEN=1 cargo test --release -p spm-cpuref --test golden`, then refresh
`SHA256SUMS`.
