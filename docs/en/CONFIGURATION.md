# Configuration

_Português: [../pt-BR/CONFIGURACAO.md](../pt-BR/CONFIGURACAO.md)_

Everything the user can change lives in one TOML file, **`~/.config/spark-pearl-miner/config.toml`**
(`$XDG_CONFIG_HOME` is honoured; `spark-pearl-miner config path` prints it). The file has two
blocks:

- **Basic**: the wallet, the worker name, the language and the three pools. The GUI edits these
  (the setup and the gear icon, see the [manual](MANUAL.md#settings)); it shows each pool as
  `host:port` only and keeps the pool's other keys as written here.
- **Advanced**: a preset for the NVIDIA DGX Spark (failover timing, power profile, GPU sharing,
  worker and API). The GUI does not show it. It is already tuned; edit it by hand only if you know
  why (the manual has [recipes](MANUAL.md#recipes)).

The daemon writes the file with a comment above every key (meaning, unit, range and the tested
value). Every save from the GUI rewrites it with the same comments: **comments you add by hand are
not kept**. Check a hand edit with `spark-pearl-miner config check` (prints `OK: <path>`, or one
`key: problem` line per error and exit status 1; `--file PATH` checks another file). The systemd
unit runs the same check before it starts the daemon, and `spark-pearl-miner status` prints the
errors when the daemon is not running.

- **`schema_version = 1`**: the daemon refuses a file with another version.
- **Validated**: unknown keys are errors, and so is any key that looks like a fee setting (`fee`,
  `dev…`, `donation…`): the developer fee is compiled into the binary and cannot be configured
  (see `crates/spm-fee`; the GUI shows the fee line read-only). A file that fails validation at start-up stops the
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

This is exactly what the daemon writes on first start, comments included (a test keeps this page in
sync; `cargo run -q --release -p spm-api --example default_config` prints it):

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
# Advanced, optional: sign in to this pool with an account instead of the wallet (for example a
# Kryptex ID, so that pool pays out in BTC). The worker name is appended; the other pools keep
# the wallet. Example: login = "krxabc123"
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

Mining does not start until `miner.wallet` is set and `miner.disclosure_accepted` is `true`; the
setup wizard does both. The file never changes by itself after the first start: an existing
`config.toml` is kept as it is across upgrades (new defaults apply only to a new file).

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

The default slots are Kryptex → HeroMiners BR → LuckyPool BR. GUI presets (the list offered in the
Settings pool rows; typing a preset's `host:port` uses all of its keys):

| Preset | Host:port | TLS | Dialect / jsonrpc | Status |
|---|---|---|---|---|
| Kryptex | `prl-br.kryptex.network:8048` | on | kryptex / auto | verified live, default pool 1 |
| HeroMiners BR | `br.pearl.herominers.com:1200` | auto | auto / auto | verified live, default pool 2 |
| LuckyPool BR | `pearl-br.luckypool.io:3360` | pinned `d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=` | object / on | verified live, default pool 3 |
| HeroMiners US / US2 / DE / FR | `{us,us2,de,fr}.pearl.herominers.com:1200` | auto | auto / auto | not verified |
| LuckyPool EU | `pearl-eu1.luckypool.io:3360` | pinned (same key) | object / on | not verified |

A `host:port` typed in the GUI that is neither the saved entry of that row nor a preset becomes a
custom entry: `name` = the host, `tls`, `dialect`, `jsonrpc`, `proof`, `pattern` = `auto`,
`password` = `x`, `enabled` = `true`.

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
| `power.max_acknowledged` | `false` | `max` is refused without it: the file or the `PUT` fails validation (`max_not_acknowledged`) and the daemon checks again. There is no GUI for it: set it by hand, next to `profile = "max"`. Max is not recommended on the DGX Spark (measured 83–87 W and a 97.5 °C board). |

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
| `gui.language` | `"auto"` | `auto`, `en`, `pt-BR`. Set by the setup and the Settings dialog; the EN/PT switch in the header changes only that browser |

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
