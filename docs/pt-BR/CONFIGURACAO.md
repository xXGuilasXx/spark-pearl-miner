# Configuração

_English: [../en/CONFIGURATION.md](../en/CONFIGURATION.md)_

Tudo o que o usuário pode mudar fica num único arquivo TOML,
**`~/.config/spark-pearl-miner/config.toml`** (respeita `$XDG_CONFIG_HOME`). A GUI edita o mesmo
arquivo pelo `PUT /api/v1/config`; você também pode editá-lo à mão com o daemon rodando.

- **`schema_version = 1`**: o daemon recusa um arquivo de outra versão.
- **Validado**: chaves desconhecidas são erro, e também qualquer chave que pareça configuração de
  taxa (`fee`, `dev…`, `donation…`): a taxa do desenvolvedor é compilada no binário e não pode ser
  configurada (veja a tela Taxa e o `crates/spm-fee`). Um arquivo inválido na partida impede o
  daemon de subir, com o motivo; uma edição errada com ele rodando vira um alerta e as
  configurações anteriores continuam valendo.
- **Gravação atômica com backup**: cada gravação escreve um arquivo temporário, sincroniza e o
  renomeia por cima do `config.toml` (modo 0600); o arquivo anterior fica como `config.toml.bak`.
- **Recarga a quente**: o daemon confere o arquivo a cada 2 s. Mudanças de pool, carteira e worker
  reconectam só as pools afetadas; mudar os limites do failover reinicia as conexões com as pools;
  mudanças em `[api]` precisam reiniciar o daemon.
- **Auditado**: cada mudança vira uma linha JSON em `~/.local/state/spark-pearl-miner/audit.log`
  com a hora, a origem (`api`, `file`, `cli`), as chaves alteradas e se a carteira de pagamento
  mudou. Trocar a carteira também mostra um aviso na GUI até você confirmar.

## O arquivo padrão

É exatamente o que o daemon grava na primeira execução (um teste mantém esta página em dia):

```toml
# spark-pearl-miner configuration (schema_version 1).
# Edited by the GUI (http://127.0.0.1:4078) or by hand; changes are picked up while the
# daemon runs. The developer fee is not configurable: see FEE.md.

schema_version = 1

[miner]
wallet = ""
worker = "spark"
disclosure_accepted = false

[[pools]]
name = "HeroMiners BR"
host = "br.pearl.herominers.com"
port = 1200
tls = "auto"
dialect = "auto"
jsonrpc = "auto"
proof = "auto"
password = "x"
pattern = "auto"
enabled = true

[[pools]]
name = "LuckyPool BR"
host = "pearl-br.luckypool.io"
port = 3360
tls = "pinned"
spki_pin = "d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk="
dialect = "object"
jsonrpc = "on"
proof = "auto"
password = "x"
pattern = "auto"
enabled = true

[[pools]]
name = "Kryptex"
host = "prl.kryptex.network"
port = 8048
tls = "on"
dialect = "kryptex"
jsonrpc = "auto"
proof = "auto"
password = "x"
pattern = "auto"
enabled = true

[failover]
connect_timeout_s = 10
handshake_timeout_s = 15
first_job_timeout_s = 30
stall_soft_reconnect_s = 900
max_consecutive_invalid = 5
reject_ratio_max = 0.5
reject_window = 20
stale_ratio_max = 0.02
stale_window = 100
submit_ack_timeout_s = 30
max_ack_timeouts = 3
backoff_s = [
    5,
    10,
    20,
    40,
    80,
    120,
]
backoff_jitter_pct = 20
failback_probe_every_s = 300
failback_stable_s = 60
auth_retry_s = 600
quarantine_s = 600
drain_s = 5
reconnect_same_after_s = 60

[power]
profile = "balanced"
max_acknowledged = false

[coexistence]
mode = "exclusive"

[worker]
launch = "spawn"
simulate = false
sim_interval_ms = 1000

[api]
bind = "127.0.0.1"
port = 4078
lan = false

[gui]
language = "auto"
```

A mineração só começa quando `miner.wallet` está preenchida e `miner.disclosure_accepted` é
`true`; o assistente de configuração faz as duas coisas.

## `[miner]`

| Chave | Padrão | Regra |
|---|---|---|
| `wallet` | `""` | endereço da mainnet Pearl: bech32m, minúsculas, HRP `prl`, versão de witness 1, programa de 32 bytes (`prl1p…`, 63 caracteres). Obrigatória para minerar. Quando é o endereço do próprio desenvolvedor, a taxa se desliga sozinha. |
| `worker` | `"spark"` | `[A-Za-z0-9_-]{1,32}` |
| `disclosure_accepted` | `false` | você leu o aviso da taxa e de energia (o assistente marca) |

## `[[pools]]` (de 1 a 3 entradas)

A ordem é a prioridade: a primeira entrada é a pool 1. Uma quarta entrada é recusada.

| Chave | Padrão | Valores |
|---|---|---|
| `name` | `""` | rótulo livre, até 40 caracteres |
| `host` | – | nome do host ou endereço IP |
| `port` | – | 1–65535 |
| `tls` | `"auto"` | `auto` (TLS primeiro; TCP simples só quando o servidor não fala TLS, nunca depois de um erro de certificado; o resultado fica guardado por `host:porta` por 7 dias), `on`, `off`, `pinned` (TLS conferido só contra o `spki_pin`, para pools autoassinadas) |
| `spki_pin` | – | SHA-256 em base64 da chave pública do servidor; só com `tls = "pinned"` |
| `dialect` | `"auto"` | `auto` (hosts da Kryptex → `kryptex`, o resto → `object`), `object` (HeroMiners, LuckyPool), `kryptex`, `kryptex-v2` (provas em gzip; não confirmado ao vivo) |
| `jsonrpc` | `"auto"` | acrescenta `"jsonrpc":"2.0"` às requisições: `auto` (ligado para `*.luckypool.io`, padrão do dialeto nos outros), `on`, `off` |
| `proof` | `"auto"` | codificação da prova: `auto` (começa com `plain_proof`, troca depois de 3 rejeições de formato e guarda o campo que funcionou por pool), `plain`, `zstd` (`plain_proof_zst`) |
| `password` | `"x"` | senha do stratum, até 64 caracteres imprimíveis (a Kryptex aceita também `d=<N>`) |
| `pattern` | `"auto"` | padrão do bloco de hash: `auto` (8×16), `official` (alternativa 2×64, marco M15) |
| `enabled` | `true` | uma posição desativada nunca é contatada |

Predefinições da GUI:

| Predefinição | Host:porta | TLS | Dialeto / jsonrpc |
|---|---|---|---|
| HeroMiners BR / US / US2 / DE / FR | `{br,us,us2,de,fr}.pearl.herominers.com:1200` | auto | auto |
| LuckyPool BR | `pearl-br.luckypool.io:3360` | pinned `d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=` | object / on |
| LuckyPool EU (ainda não verificada) | `pearl-eu1.luckypool.io:3360` | pinned (a mesma chave) | object / on |
| Kryptex | `prl.kryptex.network:8048` | on | kryptex |

## `[failover]`

Padrões do `docs/pt-BR/ARQUITETURA.md` ("Failover"). Cada valor tem faixa conferida.

| Chave | Padrão | Significado |
|---|---|---|
| `connect_timeout_s` | 10 | DNS e conexão TCP, cada um (1–120) |
| `handshake_timeout_s` | 15 | negociação TLS e a resposta do login (1–120) |
| `first_job_timeout_s` | 30 | primeiro trabalho depois do login (1–600) |
| `stall_soft_reconnect_s` | 900 | sem trabalho por esse tempo: reconecta uma vez, depois troca de pool (10–7200) |
| `max_consecutive_invalid` | 5 | rejeições inválidas seguidas que contam como tempestade |
| `reject_ratio_max` / `reject_window` | 0,5 / 20 | mais que essa fração de rejeições (sem contar obsoletas e de dificuldade baixa) na janela é tempestade |
| `stale_ratio_max` / `stale_window` | 0,02 / 100 | mais de 2 % de obsoletas em 100 shares derruba a pool |
| `submit_ack_timeout_s` / `max_ack_timeouts` | 30 / 3 | envios seguidos sem resposta que derrubam a pool |
| `backoff_s` | 5, 10, 20, 40, 80, 120 | esperas depois de falhas seguidas (a última se repete), de 1 a 16 passos de 1–3600 s |
| `backoff_jitter_pct` | 20 | variação ± em cada espera |
| `failback_probe_every_s` | 300 | enquanto uma pool de prioridade menor minera, sonda a melhor pool acima com essa frequência |
| `failback_stable_s` | 60 | a pool sondada precisa ficar saudável esse tempo antes da troca |
| `auth_retry_s` | 600 | tenta de novo um login recusado depois desse tempo |
| `quarantine_s` | 600 | pausa uma pool que mandou texto de banimento |
| `drain_s` | 5 | a pool antiga ainda recebe as shares em andamento por esse tempo depois de uma troca planejada |
| `reconnect_same_after_s` | 60 | uma pool que minerava há mais que isso ganha uma reconexão antes do failover |

## `[power]`, `[coexistence]`, `[worker]`

| Chave | Padrão | Valores |
|---|---|---|
| `power.profile` | `"balanced"` | `eco`, `balanced` (alvo de 75 W, para em 85 W), `max` (precisa de `max_acknowledged = true`, que a GUI só marca depois da confirmação digitada). Aplicado pelo controlador de energia (marco M11). |
| `coexistence.mode` | `"exclusive"` | `spark-modo` (o worker só como o runtime `miner` do spark-modo), `yield`, `yield-release` (os dois se comportam como `exclusive` até o M11), `exclusive` |
| `worker.launch` | `"spawn"` | `spawn` (o daemon sobe o worker) ou `external` (o spark-modo sobe; o daemon espera no `worker.sock`) |
| `worker.simulate` | `false` | roda o worker de **simulação na CPU** no lugar do CUDA. Ele usa o minerador de referência oficial com m = n = 256, k = 2048 e só acha shares com dificuldade trivial (a pool de teste). Nenhuma dívida de taxa se acumula durante a simulação. |
| `worker.sim_interval_ms` | 1000 | pausa entre tentativas simuladas (50–60000) |

## `[api]`, `[gui]`

| Chave | Padrão | Regra |
|---|---|---|
| `api.bind` | `"127.0.0.1"` | só endereço de loopback |
| `api.port` | 4078 | 1–65535 (precisa reiniciar) |
| `api.lan` | `false` | `true` é recusado: acesso pela rede local exige TLS, que esta versão não implementa; use `ssh -L 4078:127.0.0.1:4078` |
| `gui.language` | `"auto"` | `auto`, `en`, `pt-BR` (a escolha no cabeçalho vale para aquele navegador) |

## Outros arquivos

| Caminho | O quê |
|---|---|
| `~/.config/spark-pearl-miner/config.toml` (+ `.bak`) | este arquivo |
| `~/.config/spark-pearl-miner/api-token` | token da API, 0600 |
| `~/.local/state/spark-pearl-miner/state.json` | o que o daemon aprendeu: estado do agendador de dívida da taxa (nenhuma constante da taxa é gravada), campo de prova que funciona por pool, resultados do TLS automático, se a mineração estava ligada, marca de desligamento limpo |
| `~/.local/state/spark-pearl-miner/audit.log` | mudanças de configuração |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` | socket de controle da CLI (0600) |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` | socket do worker de GPU (0600) |
