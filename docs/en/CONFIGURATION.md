# Configuration

_Português: [../pt-BR/CONFIGURACAO.md](../pt-BR/CONFIGURACAO.md)_

Everything the user can change lives in one TOML file, **`~/.config/spark-pearl-miner/config.toml`**
(`$XDG_CONFIG_HOME` is honoured). The GUI edits the same file through `PUT /api/v1/config`; you can
also edit it by hand while the daemon runs.

- **`schema_version = 1`**: the daemon refuses a file with another version.
- **Validated**: unknown keys are errors, and so is any key that looks like a fee setting (`fee`,
  `dev…`, `donation…`): the developer fee is compiled into the binary and cannot be configured
  (see the Fee screen and `crates/spm-fee`). A file that fails validation at start-up stops the
  daemon with the reason; a bad hand edit while it runs is reported as an alert and the previous
  settings stay in force.
- **Atomic writes with a backup**: each save writes a temporary file, syncs it and renames it over
  `config.toml` (mode 0600); the previous file is kept as `config.toml.bak`.
- **Hot reload**: the daemon checks the file every 2 s. Pool, wallet and worker changes reconnect
  only the affected pools; changed failover thresholds restart the pool connections; `[power]`
  and `[coexistence]` apply at once; `[api]` changes need a daemon restart.
- **Audited**: every change is appended to `~/.local/state/spark-pearl-miner/audit.log` as one
  JSON line with the time, the source (`api`, `file`, `cli`), the changed keys and whether the
  payout wallet changed. A wallet change also raises a GUI banner until you confirm it.

## The default file

This is exactly what the daemon writes on first start (a test keeps this page in sync):

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
host = "prl-br.kryptex.network"
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
metrics_url = "http://127.0.0.1:8001/metrics"
poll_ms = 200
idle_s = 5
busy_sm_pct = 10

[worker]
launch = "spawn"
simulate = false
sim_interval_ms = 1000

[api]
bind = "127.0.0.1"
port = 4078
lan = false
trust_local_user = true

[gui]
language = "auto"
```

Mining does not start until `miner.wallet` is set and `miner.disclosure_accepted` is `true`; the
setup wizard does both.

## `[miner]`

| Key | Default | Rule |
|---|---|---|
| `wallet` | `""` | Pearl mainnet address: bech32m, lowercase, HRP `prl`, witness version 1, 32-byte program (`prl1p…`, 63 characters). Required to mine. When it is the developer's own address the fee switches itself off. |
| `worker` | `"spark"` | `[A-Za-z0-9_-]{1,32}` |
| `disclosure_accepted` | `false` | you read the fee/power disclosure (the wizard sets it) |

## `[[pools]]` (1 to 3 entries)

The order is the priority: the first entry is pool 1. A fourth entry is refused.

| Key | Default | Values |
|---|---|---|
| `name` | `""` | free label, up to 40 characters |
| `host` | – | host name or IP address |
| `port` | – | 1–65535 |
| `tls` | `"auto"` | `auto` (TLS first; plain TCP only when the server does not speak TLS, never after a certificate error; the result is remembered per `host:port` for 7 days), `on`, `off`, `pinned` (TLS checked against `spki_pin` only, for self-signed pools) |
| `spki_pin` | – | base64 SHA-256 of the server's public key; only with `tls = "pinned"` |
| `dialect` | `"auto"` | `auto` (Kryptex hosts → `kryptex`, everything else → `object`), `object` (HeroMiners, LuckyPool), `kryptex`, `kryptex-v2` (gzip proofs; not confirmed live) |
| `jsonrpc` | `"auto"` | add `"jsonrpc":"2.0"` to requests: `auto` (on for `*.luckypool.io`, dialect default otherwise), `on`, `off` |
| `proof` | `"auto"` | proof encoding: `auto` (starts with `plain_proof`, switches after 3 format rejects and remembers the working field per pool), `plain`, `zstd` (`plain_proof_zst`) |
| `password` | `"x"` | stratum password, up to 64 printable characters (Kryptex also accepts `d=<N>`) |
| `pattern` | `"auto"` | hash-tile pattern: `auto` (8×16), `official` (2×64 fallback, milestone M15) |
| `enabled` | `true` | a disabled slot is never contacted |

GUI presets:

| Preset | Host:port | TLS | Dialect / jsonrpc |
|---|---|---|---|
| HeroMiners BR / US / US2 / DE / FR | `{br,us,us2,de,fr}.pearl.herominers.com:1200` | auto | auto |
| LuckyPool BR | `pearl-br.luckypool.io:3360` | pinned `d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=` | object / on |
| LuckyPool EU (not verified yet) | `pearl-eu1.luckypool.io:3360` | pinned (same key) | object / on |
| Kryptex | `prl-br.kryptex.network:8048` | on | kryptex |

## `[failover]`

Defaults from `docs/en/ARCHITECTURE.md` ("Failover"). Each value is range-checked.

| Key | Default | Meaning |
|---|---|---|
| `connect_timeout_s` | 10 | DNS and TCP connect, each (1–120) |
| `handshake_timeout_s` | 15 | TLS handshake and the login reply (1–120) |
| `first_job_timeout_s` | 30 | first job after the login (1–600) |
| `stall_soft_reconnect_s` | 900 | no job for this long: reconnect once, then fail over (10–7200) |
| `max_consecutive_invalid` | 5 | invalid rejects in a row that count as a reject storm |
| `reject_ratio_max` / `reject_window` | 0.5 / 20 | more than this share of rejects (stale and low-difficulty excluded) over the window is a storm |
| `stale_ratio_max` / `stale_window` | 0.02 / 100 | more than 2 % stale over 100 shares fails the pool |
| `submit_ack_timeout_s` / `max_ack_timeouts` | 30 / 3 | submits without an answer in a row that fail the pool |
| `backoff_s` | 5, 10, 20, 40, 80, 120 | waits after consecutive failures (the last repeats), 1–16 steps of 1–3600 s |
| `backoff_jitter_pct` | 20 | ± jitter on every wait |
| `failback_probe_every_s` | 300 | while a lower pool mines, probe the best higher pool this often |
| `failback_stable_s` | 60 | the probed pool must stay healthy this long before the switch |
| `auth_retry_s` | 600 | retry a refused login after this long |
| `quarantine_s` | 600 | pause a pool that sent ban text |
| `drain_s` | 5 | the old pool keeps receiving in-flight shares this long after a planned switch |
| `reconnect_same_after_s` | 60 | a pool that was mining longer than this gets one reconnect before failover |

## `[power]`

Enforced by the daemon's power governor (details: [POWER-THERMAL](POWER-THERMAL.md)). The daemon
samples the GPU at 10 Hz through NVML (read-only, never a CUDA context), or through
`nvidia-smi --query-gpu` at 2 Hz when NVML cannot be loaded, and steers the worker's duty cycle to
the profile's target. With no telemetry at all it does not run a real GPU worker.

| Key | Default | Values |
|---|---|---|
| `power.profile` | `"balanced"` | `eco` (60 W target, 70 W stop), `balanced` (75 W target, 85 W stop), `max` (88 W target, 92 W stop). A change applies at once; going down, the previous stop stays for 2 s while the power comes down. After an unclean stop of the previous run (`running.marker` left behind) that run uses one profile lower and raises an alert. |
| `power.max_acknowledged` | `false` | `max` is refused without it: the file or the `PUT` fails validation (`max_not_acknowledged`) and the daemon checks again. The GUI sets it only after the typed acknowledgement. |

## `[coexistence]`

How the miner shares the GPU with an LLM server (details: [COEXISTENCE](COEXISTENCE.md)). The
memory guard applies in every mode: the worker is not started unless `MemAvailable` minus its
2 GiB budget leaves 20 GiB (and memory pressure is at most 10 %), and it is released below 16 GiB
available or above 10 % pressure.

| Key | Default | Values |
|---|---|---|
| `coexistence.mode` | `"exclusive"` | `exclusive` (no gating), `yield` (pause the worker, context kept, while vLLM has requests running or waiting; resume after `idle_s` of idle), `yield-release` (same, but the worker process exits while busy and a new one is started when idle), `spark-modo` (the spark-modo `miner` runtime starts and stops the worker: the daemon never spawns it, whatever `worker.launch` says, and reports "controlled by spark-modo") |
| `coexistence.metrics_url` | `"http://127.0.0.1:8001/metrics"` | vLLM's Prometheus endpoint, plain `http://` only (`yield`, `yield-release`) |
| `coexistence.poll_ms` | 200 | metrics poll period (100–250) |
| `coexistence.idle_s` | 5 | continuous idle before mining starts or resumes (1–600) |
| `coexistence.busy_sm_pct` | 10 | when the metrics are unavailable: another compute process at or above this SM utilization counts as busy (1–100); with no signal at all the GPU counts as busy |

## `[worker]`

| Key | Default | Values |
|---|---|---|
| `worker.launch` | `"spawn"` | `spawn` (the daemon starts the worker) or `external` (spark-modo starts it; the daemon waits on `worker.sock`) |
| `worker.simulate` | `false` | run the **CPU simulation** worker instead of the CUDA one. It uses the official reference miner at m = n = 256, k = 2048 and finds shares only at trivial difficulty (the mock pool). No fee debt accrues while simulating. |
| `worker.sim_interval_ms` | 1000 | pause between simulated attempts (50–60000) |

## `[api]`, `[gui]`

| Key | Default | Rule |
|---|---|---|
| `api.bind` | `"127.0.0.1"` | a loopback address only |
| `api.port` | 4078 | 1–65535 (needs a restart) |
| `api.lan` | `false` | `true` is refused: LAN access needs TLS, not implemented in this build; use `ssh -L 4078:127.0.0.1:4078` |
| `api.trust_local_user` | `true` | connections from this machine by the same user account need no token (the peer's UID is checked); everything else keeps the token. `false` requires the token everywhere (needs a restart). See [GUI.md](GUI.md#local-access-without-a-token) |
| `gui.language` | `"auto"` | `auto`, `en`, `pt-BR` (the browser's choice in the header wins for that browser) |

## Other files

| Path | What |
|---|---|
| `~/.config/spark-pearl-miner/config.toml` (+ `.bak`) | this file |
| `~/.config/spark-pearl-miner/api-token` | API token, 0600 |
| `~/.local/state/spark-pearl-miner/state.json` | what the daemon learned: developer-fee debt scheduler state (no fee constant is stored), working proof field per pool, TLS auto results, whether mining was running, clean-shutdown flag |
| `~/.local/state/spark-pearl-miner/audit.log` | configuration changes |
| `~/.local/state/spark-pearl-miner/running.marker` | written (fsync'd) when the daemon starts, removed on a clean stop; found at start, it means the previous run crashed or lost power |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` | CLI control socket (0600) |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` | GPU worker socket (0600) |
| `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack` | the worker's last pause/resume acknowledgement (`paused <seq>` / `running <seq>`) |
