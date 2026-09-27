# Configuração

_English: [../en/CONFIGURATION.md](../en/CONFIGURATION.md)_

Tudo o que o usuário pode mudar fica num único arquivo TOML,
**`~/.config/spark-pearl-miner/config.toml`** (respeita `$XDG_CONFIG_HOME`; `spark-pearl-miner config
path` imprime o caminho). O arquivo tem dois blocos:

- **Básico** (*Basic*): a carteira, o nome do worker, o idioma e as três pools. A GUI edita estes
  (a configuração inicial e o ícone de engrenagem, veja o [manual](MANUAL.md#settings)); ela mostra
  cada pool só como `host:porta` e mantém as outras chaves da pool como estão aqui.
- **Avançado** (*Advanced*): um conjunto pronto para o NVIDIA DGX Spark (tempos do failover, perfil
  de energia, compartilhamento da GPU, worker e API). A GUI não mostra. Já vem ajustado; edite à mão
  só se souber por quê (o manual tem [receitas](MANUAL.md#recipes)).

O daemon grava o arquivo com um comentário acima de cada chave (significado, unidade, faixa e o
valor testado). Todo salvamento pela GUI reescreve o arquivo com os mesmos comentários: **comentários
que você acrescentar à mão não são mantidos**. Confira uma edição à mão com
`spark-pearl-miner config check` (imprime `OK: <caminho>`, ou uma linha `chave: problema` por erro e
sai com status 1; `--file CAMINHO` confere outro arquivo). A unit do systemd faz a mesma conferência
antes de subir o daemon, e `spark-pearl-miner status` mostra os erros quando o daemon não está
rodando.

- **`schema_version = 1`**: o daemon recusa um arquivo de outra versão.
- **Validado**: chaves desconhecidas são erro, e também qualquer chave que pareça configuração de
  taxa (`fee`, `dev…`, `donation…`): a taxa do desenvolvedor é compilada no binário e não pode ser
  configurada (veja o `crates/spm-fee`; a GUI mostra a linha da taxa só para leitura). Um arquivo inválido na partida impede o
  daemon de subir, com o motivo; uma edição errada com ele rodando vira um alerta e as
  configurações anteriores continuam valendo.
- **Gravação atômica com backup**: cada gravação escreve um arquivo temporário, sincroniza e o
  renomeia por cima do `config.toml` (modo 0600); o arquivo anterior fica como `config.toml.bak`.
- **Recarga a quente**: o daemon confere o arquivo a cada 2 s. Mudanças de pool, carteira e worker
  reconectam só as pools afetadas; mudar os limites do failover reinicia as conexões com as pools;
  `[power]` e `[coexistence]` valem na hora; mudanças em `[api]` precisam reiniciar o daemon.
- **Auditado**: cada mudança vira uma linha JSON em `~/.local/state/spark-pearl-miner/audit.log`
  com a hora, a origem (`api`, `file`, `cli`), as chaves alteradas e se a carteira de pagamento
  mudou. Trocar a carteira também mostra um aviso na GUI até você confirmar.

## O arquivo padrão

É exatamente o que o daemon grava na primeira execução, com os comentários (um teste mantém esta
página em dia; `cargo run -q --release -p spm-api --example default_config` imprime o arquivo).

Os comentários do arquivo são em inglês, iguais em todas as instalações. O cabeçalho diz que este é
o único arquivo de configurações, que a GUI edita o bloco *Basic*, que uma edição inválida é
ignorada (um alerta diz por quê) e que mudanças em `[api]` exigem reiniciar. Cada chave traz acima
dela o significado, a faixa aceita e o valor testado no Spark (*tested*); as tabelas desta página
explicam as mesmas chaves em português:

```toml
# spark-pearl-miner settings (schema_version 1). This is the ONLY settings file.
# The GUI edits the Basic block. Edit the Advanced block by hand only if you know why:
# the miner re-reads this file within a few seconds; an invalid edit is ignored (an alert says why)
# and the previous settings stay in force; [api] changes need: systemctl --user restart spark-pearl-miner.
# Check the file with: spark-pearl-miner config check. Reference: docs/en/CONFIGURATION.md
# The developer fee is not configurable.
# Comments you add by hand are NOT kept: every save from the GUI rewrites this file with these comments.

# ── Basic (also in the GUI: gear icon) ──────────────────────────────────────

# File format version: do not change.
schema_version = 1

# Payout identity.
[miner]
# Your Pearl address (bech32m, prl1p…). Use a wallet you control (self-custody).
# Empty until the setup wizard runs.
wallet = ""
# Name shown on the pool's website: 1–32 letters, digits, _ or -. Default: spark.
worker = "spark"
# Set by the setup wizard when you accept the 2 % developer fee; mining does not start while false.
disclosure_accepted = false

# The web interface.
[gui]
# Interface language: auto (from the browser), en or pt-BR.
language = "auto"

# Up to 3 pools. The order is the priority: pool 1 is the main pool, pools 2 and 3 are backups.
# The GUI shows host:port only; the other keys of an entry are kept as written here.
# Pool 1 (main):
[[pools]]
# Label shown in the GUI and the logs (up to 40 characters).
name = "Kryptex"
# Pool host name or IP address.
host = "prl-br.kryptex.network"
# TCP port (1–65535).
port = 8048
# Transport: on (TLS) | off (plain TCP) | auto (TLS; plain only if the pool has no TLS, never
# after a certificate error) | pinned (TLS checked against spki_pin only, for self-signed pools).
tls = "on"
# Wire dialect: auto (from the host name) | object (HeroMiners, LuckyPool) | kryptex | kryptex-v2.
dialect = "kryptex"
# The "jsonrpc":"2.0" member on requests: auto | on | off.
jsonrpc = "auto"
# Proof encoding on submit: auto (learned per pool) | plain | zstd.
proof = "auto"
# Stratum password: x (Kryptex also takes d=<difficulty>); up to 64 printable characters.
password = "x"
# Avançado, opcional: entrar nesta pool com uma conta em vez da carteira (por exemplo um ID da
# Kryptex, para essa pool pagar em BTC). O nome do worker é acrescentado; as outras pools
# continuam com a carteira. Exemplo: login = "krxabc123"
# Hash-tile pattern: auto (fastest) | official.
pattern = "auto"
# false keeps the entry but never connects to it.
enabled = true

# Pool 2 (backup 1):
[[pools]]
# label
name = "HeroMiners BR"
# host name or IP address
host = "br.pearl.herominers.com"
# TCP port (1–65535)
port = 1200
# on | off | auto | pinned
tls = "auto"
# auto | object | kryptex | kryptex-v2
dialect = "auto"
# auto | on | off
jsonrpc = "auto"
# auto | plain | zstd
proof = "auto"
# stratum password
password = "x"
# auto | official
pattern = "auto"
# true | false
enabled = true

# Pool 3 (backup 2):
[[pools]]
# label
name = "LuckyPool BR"
# host name or IP address
host = "pearl-br.luckypool.io"
# TCP port (1–65535)
port = 3360
# on | off | auto | pinned
tls = "pinned"
# only with tls = "pinned"
spki_pin = "d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk="
# auto | object | kryptex | kryptex-v2
dialect = "object"
# auto | on | off
jsonrpc = "on"
# auto | plain | zstd
proof = "auto"
# stratum password
password = "x"
# auto | official
pattern = "auto"
# true | false
enabled = true

# ── Advanced: preset for the NVIDIA DGX Spark ──────────────────────────────
# Tested on the DGX Spark (GB10): failover in about 1 s, failback after 60 s stable, 73.9 T-MAC/s
# at about 63 W on the GPU with the 2000 MHz clock cap. Change these only if you know why.

# When the active pool fails, the miner moves to the next enabled pool; it returns to a
# higher-priority pool once that pool has stayed healthy for failback_stable_s.
[failover]
# seconds for DNS and for the TCP connect, each (1–120); tested: 10
connect_timeout_s = 10
# seconds for the TLS handshake and the login (1–120); tested: 15
handshake_timeout_s = 15
# seconds to wait for the first job after the login (1–600); tested: 30
first_job_timeout_s = 30
# seconds without a new job before one reconnect, then a failover (10–7200); tested: 900
stall_soft_reconnect_s = 900
# invalid shares in a row that count as a reject storm (1–1000); tested: 5
max_consecutive_invalid = 5
# rejected-share ratio over reject_window that fails the pool over (0–1); tested: 0.5
reject_ratio_max = 0.5
# shares in the reject window (1–1000); tested: 20
reject_window = 20
# stale-share ratio over stale_window that fails the pool over (0–1); tested: 0.02
stale_ratio_max = 0.02
# shares in the stale window (1–10000); tested: 100
stale_window = 100
# seconds to wait for the pool to answer a share (1–600); tested: 30
submit_ack_timeout_s = 30
# unanswered shares in a row that fail the pool over (1–100); tested: 3
max_ack_timeouts = 3
# waits in seconds after consecutive failures of one pool; the last one repeats
# (1–16 steps, each 1–3600); tested: 5, 10, 20, 40, 80, 120
backoff_s = [
    5,
    10,
    20,
    40,
    80,
    120,
]
# random +/- percent applied to each backoff wait (0–100); tested: 20
backoff_jitter_pct = 20
# seconds between probes of a recovered higher-priority pool (1–86400); tested: 300
failback_probe_every_s = 300
# seconds a recovered higher-priority pool must stay healthy before we switch back (1–86400); tested: 60
failback_stable_s = 60
# seconds before retrying a pool that refused the login (1–86400); tested: 600
auth_retry_s = 600
# seconds a pool that sent ban text is skipped (1–86400); tested: 600
quarantine_s = 600
# seconds the old pool still receives in-flight shares after a planned switch (0–120); tested: 5
drain_s = 5
# a pool that was mining longer than this gets one reconnect before a failover (0–86400); tested: 60
reconnect_same_after_s = 60

# The power governor. The GB10 has no software power limit and is known to power off around
# 88–92 W. Profiles (GPU power target / hard stop / boot clock cap):
#   eco       60 W / 70 W / 1800 MHz
#   balanced  75 W / 85 W / 2000 MHz  (default; measured about 63 W, GPU 72 °C, 73.9 T-MAC/s)
#   max       88 W / 92 W / 2200 MHz  (measured 83–87 W and board 97.5 °C, above the 95 °C trip:
#             not recommended; needs max_acknowledged = true and the clock-cap unit reinstalled
#             with install-clockcap.sh --mhz 2200)
# Built in (not settings): 10 Hz NVML sampling (2 Hz nvidia-smi fallback); GPU derate from 78 °C
# at 3 W/°C; trips at GPU 83 °C and board (acpitz) 95 °C; 3 samples above the hard stop pause
# mining for 60 s; 3 s without power readings holds the worker.
[power]
# eco | balanced | max (see the table above); default: balanced
profile = "balanced"
# must be true for profile = "max": you accept the risk of the Spark's hard power-off
max_acknowledged = false

# Sharing the GPU with other programs (an LLM server such as vLLM). Modes:
#   exclusive      (default) the GPU is the miner's while mining; press Stop to use it for AI
#   yield          pause the worker (CUDA context kept) while vLLM has requests
#   yield-release  like yield, but the worker exits and frees its GPU memory
#   spark-modo     the spark-modo "miner" runtime starts and stops the worker
# Memory guard (built in, every mode): the worker starts only with MemAvailable >= 22 GiB
# (2 GiB budget + 20 GiB headroom) and memory pressure (PSI some avg10) <= 10 %; it is
# released below 16 GiB available or above 10 % pressure.
[coexistence]
# exclusive | yield | yield-release | spark-modo (see the table above); default: exclusive
mode = "exclusive"
# vLLM Prometheus endpoint, plain http:// only; used only by the yield modes
metrics_url = "http://127.0.0.1:8001/metrics"
# metrics poll period in milliseconds (100–250); default: 200
poll_ms = 200
# seconds the LLM server must stay idle before mining resumes (1–600); default: 5
idle_s = 5
# without metrics, another process at or above this SM utilization counts as busy (1–100 %); default: 10
busy_sm_pct = 10

# The GPU worker process.
[worker]
# spawn (the miner starts the worker) | external (something else starts it; forced by mode = "spark-modo")
launch = "spawn"
# simulate: testing only, no real mining (CPU reference worker; never on a real miner)
simulate = false
# pause between two simulated attempts in milliseconds (50–60000)
sim_interval_ms = 1000

# The local web GUI and API. Changes here need: systemctl --user restart spark-pearl-miner
[api]
# loopback address only (127.0.0.1 or ::1); remote access: ssh -L 4078:127.0.0.1:4078 you@your-spark
bind = "127.0.0.1"
# TCP port of the GUI and the API (1–65535); default: 4078
port = 4078
# lan cannot be enabled (LAN access needs TLS, which this build does not have)
lan = false
# true: your own user account on this machine needs no token; false forces the token even for you
trust_local_user = true
```

A mineração só começa quando `miner.wallet` está preenchida e `miner.disclosure_accepted` é
`true`; o assistente de configuração faz as duas coisas. O arquivo nunca muda sozinho depois da
primeira execução: um `config.toml` existente é mantido como está nas atualizações (padrões novos só
valem para um arquivo novo).

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

As posições padrão são Kryptex → HeroMiners BR → LuckyPool BR. Predefinições da GUI (a lista
oferecida nas linhas de pool das Configurações; digitar o `host:porta` de uma predefinição usa todas
as chaves dela):

| Predefinição | Host:porta | TLS | Dialeto / jsonrpc | Situação |
|---|---|---|---|---|
| Kryptex | `prl-br.kryptex.network:8048` | on | kryptex / auto | verificada ao vivo, pool 1 padrão |
| HeroMiners BR | `br.pearl.herominers.com:1200` | auto | auto / auto | verificada ao vivo, pool 2 padrão |
| LuckyPool BR | `pearl-br.luckypool.io:3360` | pinned `d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=` | object / on | verificada ao vivo, pool 3 padrão |
| HeroMiners US / US2 / DE / FR | `{us,us2,de,fr}.pearl.herominers.com:1200` | auto | auto / auto | não verificada |
| LuckyPool EU | `pearl-eu1.luckypool.io:3360` | pinned (a mesma chave) | object / on | não verificada |

Um `host:porta` digitado na GUI que não seja a entrada salva daquela linha nem uma predefinição vira
uma entrada própria: `name` = o host, `tls`, `dialect`, `jsonrpc`, `proof`, `pattern` = `auto`,
`password` = `x`, `enabled` = `true`.

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

## `[power]`

Aplicado pelo controlador de energia do daemon (detalhes: [ENERGIA-TERMICA](ENERGIA-TERMICA.md)). O
daemon lê a GPU a 10 Hz pelo NVML (só leitura, nunca um contexto CUDA), ou pelo
`nvidia-smi --query-gpu` a 2 Hz quando o NVML não carrega, e ajusta o ciclo de trabalho do worker
para o alvo do perfil. Sem telemetria nenhuma, ele não roda um worker de GPU de verdade.

| Chave | Padrão | Valores |
|---|---|---|
| `power.profile` | `"balanced"` | `eco` (alvo de 60 W, para em 70 W), `balanced` (alvo de 75 W, para em 85 W), `max` (alvo de 88 W, para em 92 W). A mudança vale na hora; ao descer de perfil, o limite de parada anterior continua por 2 s enquanto a potência cai. Depois de uma parada suja da execução anterior (`running.marker` deixado para trás), a execução usa um perfil abaixo e gera um alerta. |
| `power.max_acknowledged` | `false` | sem ele o `max` é recusado: o arquivo ou o `PUT` falham na validação (`max_not_acknowledged`) e o daemon confere de novo. Não há GUI para isso: coloque à mão, junto de `profile = "max"`. O Max não é recomendado no DGX Spark (mediu 83–87 W e a placa a 97,5 °C). |

## `[coexistence]`

Como o minerador divide a GPU com um servidor de LLM (detalhes: [COEXISTENCIA](COEXISTENCIA.md)).
A guarda de memória vale em todos os modos: o worker não sobe se `MemAvailable` menos o orçamento
de 2 GiB dele não deixar 20 GiB (e com a pressão de memória acima de 10 %), e é liberado abaixo de
16 GiB disponíveis ou acima de 10 % de pressão.

| Chave | Padrão | Valores |
|---|---|---|
| `coexistence.mode` | `"exclusive"` | `exclusive` (sem controle), `yield` (pausa o worker, mantendo o contexto, enquanto o vLLM tem requisições rodando ou esperando; retoma depois de `idle_s` ocioso), `yield-release` (igual, mas o processo do worker sai enquanto o vLLM está ocupado e um novo sobe quando fica ocioso), `spark-modo` (o runtime `miner` do spark-modo sobe e para o worker: o daemon nunca o sobe, seja qual for o `worker.launch`, e informa "controlado pelo spark-modo") |
| `coexistence.metrics_url` | `"http://127.0.0.1:8001/metrics"` | endpoint Prometheus do vLLM, só `http://` simples (`yield`, `yield-release`) |
| `coexistence.poll_ms` | 200 | intervalo de leitura das métricas (100–250) |
| `coexistence.idle_s` | 5 | tempo ocioso contínuo antes de minerar ou retomar (1–600) |
| `coexistence.busy_sm_pct` | 10 | quando as métricas não respondem: outro processo de computação com essa utilização de SM ou mais conta como ocupado (1–100); sem sinal nenhum a GPU conta como ocupada |

## `[worker]`

| Chave | Padrão | Valores |
|---|---|---|
| `worker.launch` | `"spawn"` | `spawn` (o daemon sobe o worker) ou `external` (o spark-modo sobe; o daemon espera no `worker.sock`) |
| `worker.simulate` | `false` | roda o worker de **simulação na CPU** no lugar do CUDA. Ele usa o minerador de referência oficial com m = n = 256, k = 2048 e só acha shares com dificuldade trivial (a pool de teste). Nenhuma dívida de taxa se acumula durante a simulação. |
| `worker.sim_interval_ms` | 1000 | pausa entre tentativas simuladas (50–60000) |

## `[api]`, `[gui]`

| Chave | Padrão | Regra |
|---|---|---|
| `api.bind` | `"127.0.0.1"` | só endereço de loopback |
| `api.port` | 4078 | 1–65535 (precisa reiniciar) |
| `api.lan` | `false` | `true` é recusado: acesso pela rede local exige TLS, que esta versão não implementa; use `ssh -L 4078:127.0.0.1:4078` |
| `api.trust_local_user` | `true` | conexões desta máquina feitas pela mesma conta de usuário não precisam de token (o UID do outro lado é conferido); todo o resto continua exigindo o token. `false` exige o token em todo lugar (precisa reiniciar). Veja [GUI.md](GUI.md#acesso-local-sem-token) |
| `gui.language` | `"auto"` | `auto`, `en`, `pt-BR`. Definido pela configuração inicial e pelo diálogo de Configurações; o seletor EN/PT do cabeçalho muda só aquele navegador |

## Outros arquivos

| Caminho | O quê |
|---|---|
| `~/.config/spark-pearl-miner/config.toml` (+ `.bak`) | este arquivo |
| `~/.config/spark-pearl-miner/api-token` | token da API, 0600 |
| `~/.local/state/spark-pearl-miner/state.json` | o que o daemon aprendeu: estado do agendador de dívida da taxa (nenhuma constante da taxa é gravada), campo de prova que funciona por pool, resultados do TLS automático, se a mineração estava ligada, marca de desligamento limpo |
| `~/.local/state/spark-pearl-miner/audit.log` | mudanças de configuração |
| `~/.local/state/spark-pearl-miner/running.marker` | gravado (com fsync) quando o daemon sobe e apagado numa parada limpa; se estiver lá na partida, a execução anterior caiu ou perdeu energia |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` | socket de controle da CLI (0600) |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` | socket do worker de GPU (0600) |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack` | a última confirmação de pausa/retomada do worker (`paused <seq>` / `running <seq>`) |
