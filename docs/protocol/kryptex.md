# Kryptex — Pearl (PRL) stratum dialect (captured live 2026-09-26)

Endpoint used: `prl.kryptex.network:8048` (**TLS**, TLSv1.3, publicly trusted certificate). On the plain port **7048 the pool did not answer** subscribe or authorize in our probe, so the preset is 8048/TLS. Regional hosts `prl-{eu,us,br,sg,hk,ru,ae}.kryptex.network` (`prl-br` resolves to the US IP, ~140 ms from Brazil).

## Handshake (stratum-v1 arrays, `"jsonrpc":"2.0"` accepted)
```json
{"id":1,"jsonrpc":"2.0","method":"mining.subscribe","params":["<miner/version>"]}      // no reply (silent)
{"id":2,"jsonrpc":"2.0","method":"mining.authorize","params":["<prl1...>.<worker>","x"]}
```
Reply to the authorize: `{"id":2,"result":true,"error":null}`, immediately followed by a job. Password `d=<N>` selects a static difficulty (min 2,097,152).

## Job (`mining.notify`, params object)
```json
{"id":null,"method":"mining.notify","params":{"header":"<152 hex>","height":119376,"job_id":"a7460708_2097152","target":"<hex>", ...}}
```
Same object layout as HeroMiners/LuckyPool (`job_id = <8 hex>_<diff>`); `cert_version` presence to be confirmed from the fixture.

## Share (`mining.submit`) — from open-source clients (ascend_prl, pearl-metal-miner), to be confirmed in M6
`{"worker":"<prl1...>.<worker>","job_id":..,"plain_proof":"<base64(bincode)>"}`. Optional "v2" session: authorize with an object `{"wallet":"<addr>.<worker>","agent":..,"type":"v2"}`; if the ack echoes `type:"v2"`, every submit carries `base64(gzip(bincode))` in the same field.

## Pool facts
PPS+ 2 % (SOLO 1 %), min payout 1 PRL hourly, no vardiff (static difficulty via password), largest pool (~45–50 % of the network).
