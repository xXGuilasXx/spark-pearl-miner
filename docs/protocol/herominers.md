# HeroMiners — Pearl (PRL) stratum dialect (captured live 2026-09-26)

Endpoint used: `br.pearl.herominers.com:1200` (São Paulo, 177.54.145.109). One port carries **both TLS and plain TCP**; TLS is TLSv1.3 with a valid Let's Encrypt certificate (`CN = br.pearl.herominers.com`). 15 regional hosts exist (`us`, `us2`, `us3`, `ca`, `de`, `fr`, `es`, `fi`, `ru`, `hk`, `kr`, `sg`, `tr`, `au`, `br`), all on port 1200.

Capture method: `tools/spm-probe.py` — one authorize per connection, listen for jobs, **never submit**. Redacted fixture: `tests/fixtures/capture-herominers-br-authorize.jsonl` (wallet replaced by `<WALLET>`).

## Handshake (hypothesis H1 accepted on the first try)
No `mining.subscribe`. The first message is an **object** authorize:
```json
{"id":1,"method":"mining.authorize","params":{"wallet":"<prl1...>","worker":"<name>","agent":"<miner/version>"}}
```
Reply (≈1 s later):
```json
{"id":1,"error":null,"result":true}
```
Jobs start immediately after the ack. (`solo:` prefix on the wallet selects solo mode, per the pool site.)

## Job (`mining.notify`, params is an object)
```json
{"id":null,"method":"mining.notify","params":{
  "job_id":"00000000_2097152",        // <8-hex counter per session>_<share difficulty>
  "header":"<152 hex = 76-byte IncompleteBlockHeader: version u32 LE | prev_block 32 | merkle_root 32 | timestamp u32 LE | nbits u32 LE>",
  "target":"00000000000007fff8000000...",  // 64 hex, big-endian 256-bit share target = floor(0xFFFF * 2^208 / diff)
  "height":119365,
  "cert_version":3}}
```
- Starting difficulty **2,097,152** (`target = 0x7fff8 << 184`, compact `0x1a07fff8`). `fixedDiffEnabled` is `false` in the pool API (vardiff/static handled pool-side).
- A new job arrived every **~21–35 s** during the capture (template refresh), each with a new `job_id` counter; block changes also produce a new job.
- `cert_version` is present (3 = V3 salted seeds since mainnet height 99,000). Treat `≥ 4` as "update required".

## Share (`mining.submit`) — confirmed live in M6 (2026-09-26)
Object form (LuckyPool-style): `{"wallet":..,"worker":..,"job_id":..,"plain_proof":"<base64(bincode(PlainProof))>"}`. Our miner had 4 accepted, 0 rejected with **`plain_proof`** (plain encoding) at diff 2,097,152 (`docs/benchmarks/m6-20260926T222707Z-first-shares.md`). 6block's miner uses the field `plain_proof_zst` (base64 of zstd-compressed bincode) for HeroMiners; the encoder still learns the working field per pool (≤ 3 format rejects).

## Pool facts (API `/api/stats`, 2026-09-26)
Fee 0 %, scheme `prop`, min payout 1 PRL (1e8 units), payments hourly, `solo:` prefix, stale-share penalty tiers: ≤ 2 % → 0, ≤ 5 % → 50 %, > 30 % → 100 % + 3600 s ban (after 1000 shares). Merge mining of MDL was advertised in June 2026 but is dead as of 2026-09-26 (notices commented out, `modelos.herominers.com` does not resolve); see `docs/en/DUAL-MINING.md`.
