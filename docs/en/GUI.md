# GUI, local API and command line

_Português: [../pt-BR/GUI.md](../pt-BR/GUI.md)_

The daemon serves a small web GUI and a REST + SSE API on **`http://127.0.0.1:4078`**, loopback
only. The GUI is plain HTML, CSS and ES modules embedded in the binary (no build step, no CDN,
under 200 KB), in English and Brazilian Portuguese.

## Opening the GUI

```
spark-pearl-miner gui              # opens the browser, already logged in
spark-pearl-miner gui --print-url  # prints the login URL instead (for SSH)
```

`gui` starts the user service if it is not running. The URL carries the API token in its
fragment (`#token=…`), which browsers never send to the server; the page exchanges it once for a
session and removes it from the address bar. You can also paste the token from
`~/.config/spark-pearl-miner/api-token` into the login form.

**From another computer**, forward the same port number over SSH and open the printed URL there:

```
ssh -L 4078:127.0.0.1:4078 user@spark
spark-pearl-miner gui --print-url   # on the Spark
```

The port must stay 4078 on both ends: the daemon checks the `Host` header (see below).

The launcher `packaging/spark-pearl-miner.desktop` runs `spark-pearl-miner gui`:

```
install -Dm0644 packaging/spark-pearl-miner.desktop ~/.local/share/applications/spark-pearl-miner.desktop
```

## Security of the local API

| Measure | What it does |
|---|---|
| Loopback only | The listener binds `127.0.0.1` (or `::1`). `api.lan = true` is refused: LAN access needs TLS, which this build does not implement. Use SSH forwarding. |
| Token file | `~/.config/spark-pearl-miner/api-token`, 256 random bits, mode 0600, created on first start. Whoever can read it controls the miner. |
| Session cookie | `POST /api/v1/session {"token": …}` returns an `HttpOnly; SameSite=Strict; Path=/` cookie (another random 256-bit value, kept in memory: a restart logs everyone out) and a CSRF value. Wrong tokens are answered more slowly each time. |
| CSRF | Every `POST`/`PUT`/`DELETE` under `/api/` must carry the session's value in `X-SPM-CSRF`, otherwise **403**. |
| Host / Origin | `Host` must be `127.0.0.1:4078`, `localhost:4078` or `[::1]:4078` and an `Origin`, if present, the same origin; anything else is **403** (defeats DNS rebinding). |
| No session | Any `/api/*` call without a valid cookie is **401**. |
| Headers | `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` on the API. |
| Text only | Strings that come from pools (errors, job ids) are rendered with `textContent`; the GUI never uses `innerHTML`. |
| Audit | Every configuration change is appended to `~/.local/state/spark-pearl-miner/audit.log` with its source (`api`, `file`, `cli`); a changed payout wallet raises a banner until you confirm it. |

The developer fee is read-only everywhere: `/api/v1/fee` has no write method, and
`PUT /api/v1/config` refuses any key that looks fee-related (`fee`, `dev…`, `donation…`) with
`422 fee_not_configurable`.

## Screens

**Setup wizard** (first start, or *Setup* in the menu): language → wallet (checked live as a
bech32m `prl1p…` address) → worker name (`[A-Za-z0-9_-]{1,32}`) → pools → fee disclosure and
consent → power profile (Max needs a typed acknowledgement) → GPU sharing mode → summary →
**Save and start mining**.

The pool editor (wizard and *Pools*) has three slots in priority order, each with a preset
(HeroMiners BR/US/US2/DE/FR, LuckyPool BR with the pinned key, LuckyPool EU, Kryptex 8048 TLS, or
custom), host, port, TLS (`auto`, `on`, `off`, `pinned`) and an *Advanced* section (label,
dialect, `jsonrpc` member, proof encoding, password, hash-tile pattern). Slots can be reordered,
added (up to three) and removed. **Test connection** checks DNS, TCP and TLS only; ticking
*also test the login* asks for confirmation and then logs in once with your wallet and waits for
a job (nothing is submitted).

| Screen | Contents |
|---|---|
| Dashboard | state, active pool, credited hashrate (10 s), shares accepted/rejected/stale/discarded, GPU worker state, what the GPU works for (your pool, a fee slice, idle), uptime, wallet, alerts; Start / Pause / Resume / Stop |
| Pools | a status chip per slot (ACTIVE, standby, waiting to retry, login refused…), the last error in plain language with the raw pool text, counters, learned TLS and proof field, **Switch now** / **Pin** / **Unpin**, the failover timeline, and the pool editor with **Save & Apply** |
| Failover | every threshold of the failover manager with its default; *Restore defaults* |
| Performance & Power | power profile, the clock-cap instructions (`spark-pearl-miner install-clock-cap`), GPU sharing mode, worker launch mode, CPU simulation switch, GPU telemetry from `nvidia-smi` |
| Fee | the fee line, every compiled-in constant (read-only), the constants hash, and what was measured: fee so far, last 24 h, time mined for the developer, debt, next slice, dev shares |
| Logs | live log (SSE), level filter, **Export diagnostics** (logs, status, pools and settings as JSON with wallet addresses redacted) |
| About | version, commit, SHA-256 of the running binary, fee constants hash, licenses, how to verify a release, the non-affiliation statement |

## The failover demo

On any machine, without a pool or a GPU:

```
cargo run --release -p spark-pearl-miner --example failover_demo -- --port 4078
```

It starts two mock pools on localhost (pool 1 refuses connections for 30 s), the daemon with the
simulated CPU worker, and prints the GUI URL. The Pools screen shows pool 2 ACTIVE within
seconds, shares accepted, and the return to pool 1 after the probe cadence (shortened in the
demo to 20 s + 10 s; the defaults are 300 s + 60 s). The same scenario runs in CI as
`crates/spm/tests/failover.rs`.

## API reference

All paths are under `/api/v1`. Pools are numbered **1 to 3** in URLs.

| Method and path | Session | CSRF | What |
|---|---|---|---|
| `POST /session` `{"token"}` | – | – | log in: sets the cookie, returns `{"csrf"}` |
| `GET /session` | ✓ | – | the CSRF value again (page reload) |
| `DELETE /session` | ✓ | ✓ | log out |
| `GET /status` | ✓ | – | state, pool, hashrate, shares, worker, fee phase, alerts |
| `GET /config`, `PUT /config` | ✓ | PUT | the configuration (see [CONFIGURATION.md](CONFIGURATION.md)); PUT validates, saves and applies |
| `GET /pools` | ✓ | – | slots, manager state, pin, probe, timeline |
| `POST /pools/test` `{"pool", "confirm"}` | ✓ | ✓ | DNS + TCP + TLS; with `confirm: true` also login + first job |
| `POST /pools/{i}/switch` | ✓ | ✓ | switch now (also pins) |
| `POST /pools/{i}/pin` `{"pinned"}` | ✓ | ✓ | pin or unpin |
| `POST /mining/{start,stop,pause,resume}` | ✓ | ✓ | controls; Stop also releases the GPU worker |
| `POST /wallet/ack` | ✓ | ✓ | dismiss the "wallet changed" notice |
| `GET /fee` | ✓ | – | read-only fee constants, hash and measured values |
| `GET /gpu` | ✓ | – | worker device and `nvidia-smi` telemetry |
| `GET /logs?since=&limit=&redact=1` | ✓ | – | recent log lines (INFO and above) |
| `GET /about` | ✓ | – | version, commit, binary SHA-256 |
| `GET /events` | ✓ | – | Server-Sent Events, below |

SSE events: `stats` (the status, once a second), `share` (accepted/rejected, user or dev),
`fsm` (manager and slot state changes), `timeline` (failover log lines), `fee` (PreWarm,
StartSlice, EndSlice, Abort), `alert`, `log`, `config` (a configuration change and its source).

## Command line

`spark-pearl-miner` (a symlink named `spm` works the same:
`ln -s ~/.local/bin/spark-pearl-miner ~/.local/bin/spm`).

| Command | What |
|---|---|
| `daemon [--no-api]` | run the daemon (the user service does this) |
| `gpu-worker --attach <sock> [--sim]` | the GPU worker; this build only has the CPU simulation (`--sim`); the CUDA worker is milestone M5 |
| `status [--json]` | what the daemon is doing (control socket) |
| `start`, `stop`, `pause`, `resume` | controls (control socket) |
| `gui [--print-url]` | open the GUI (see above) |
| `fee-test [--connect] [--pace-ms N]` | one developer-fee cycle through the real scheduler with compressed time: PreWarm → StartSlice → EndSlice and the measured fee. Offline by default; `--connect` really logs in to the first reachable dev pool at PreWarm (never submits) |
| `version`, `--version` | version, commit, fee constants hash and the fee line |
| `install-clock-cap` | prints the `sudo` commands for the boot-time 2200 MHz clock cap; changes nothing |

The CLI talks to the daemon over `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` (0600 in a
0700 directory; connections from other users are refused after an `SO_PEERCRED` check).

## Running it as a service

```
install -Dm0755 target/release/spark-pearl-miner ~/.local/bin/spark-pearl-miner
install -Dm0644 packaging/systemd/user/spark-pearl-miner.service ~/.config/systemd/user/spark-pearl-miner.service
systemctl --user daemon-reload
systemctl --user enable --now spark-pearl-miner
sudo loginctl enable-linger "$USER"   # keep it running without a desktop session
```

The unit restarts the daemon on failure. **Stop** in the GUI makes the worker exit, which frees
its CUDA context; a worker left paused for a minute is released the same way. On a DGX Spark with
`spark-modo`, the worker runs only as the `miner` runtime: see
[`contrib/spark-modo/README.pt-BR.md`](../../contrib/spark-modo/README.pt-BR.md).

## GPU worker supervision

The daemon listens on `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` (0600). In launch mode
`spawn` it starts `spark-pearl-miner gpu-worker --attach <sock>` itself; in `external` mode
(spark-modo) it waits for the worker to attach. The worker sends a heartbeat every 500 ms; five
seconds of silence is a failure. Failures back off 5 s, 30 s, then 2 min, and three failures
within 10 minutes stop mining with the alert "possible hardware fault" until you press Start.
Every hit is verified locally (by the worker and again by the daemon) before it is submitted, and
only on the session whose job produced it.
