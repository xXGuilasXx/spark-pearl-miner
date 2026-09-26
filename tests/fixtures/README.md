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
| `capture-kryptex-8048-authorize.jsonl` | Kryptex, `prl.kryptex.network:8048` | TLS (public CA) | stratum-v1 arrays (`subscribe` silent, `authorize` acked) | static diff 2,097,152; `target = 2^224/diff − 1` |

`crates/spm-proto/tests/pool_fixtures.rs` replays every file: our generated handshake must match the accepted
one, and every recorded job must parse. `SHA256SUMS` pins the files; regenerate it whenever a capture is refreshed.
