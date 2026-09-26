# LuckyPool — Pearl (PRL) stratum dialect (captured live 2026-09-26)

Endpoint used: `pearl-br.luckypool.io:3360` (São Paulo, 172.237.62.147). Ports 3360/3361/3362 start at difficulty 2M/4M/8M with **vardiff**. **TLS is required in practice**: the plain-TCP handshake got no reply on 3360. The certificate is **self-signed** (`CN = *.luckypool.io`, valid 2026-02-07 → 2036-02-05):

- SHA-256 fingerprint: `5C:C5:FE:72:6B:DD:0D:63:9A:EA:B3:BC:67:C0:8D:4B:60:8E:38:7E:BE:D5:B1:74:A5:2C:F4:23:53:FC:41:FE`
- SPKI pin (base64): `d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=`

The miner pins this SPKI for `*.luckypool.io` instead of disabling verification (`tls = "pinned"` in the pool preset); a changed pin is a hard error with a clear GUI message.

## Handshake (object, authorize-first, with `"jsonrpc":"2.0"`)
```json
{"id":1,"jsonrpc":"2.0","method":"mining.authorize","params":{"wallet":"<prl1...>","worker":"<name>","agent":"<miner/version>"}}
```
Reply:
```json
{"error":null,"id":1,"result":true,"type":"plain"}
```
`type` tells which proof encoding the pool expects on this session (`plain` = base64(bincode(PlainProof))).

## Job (`mining.notify`)
```json
{"id":null,"method":"mining.notify","params":{
  "cert_version":3, "diff":888888, "header":"<152 hex>", "height":119375,
  "job_id":"e159af1e_888888", "target":"<64 hex big-endian>"}}
```
Same fields as HeroMiners plus `diff` (the vardiff level; a fresh worker starts at 888,888). `job_id = <8 hex>_<diff>`.

## Share (`mining.submit`) — to be confirmed in M6
`{"wallet":..,"worker":..,"job_id":..,"plain_proof":"<base64(bincode)>"}` (accepted shares confirmed by other open miners; CPPminer adds `"hs"`).

## Pool facts
Fee 1 %, PROP, min payout 1 PRL (default 5). CPU test port `pearl-cpu-eu1.luckypool.io:3370` (diff ~26,000) for fast end-to-end checks.
