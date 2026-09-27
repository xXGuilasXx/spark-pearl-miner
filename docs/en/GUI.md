# API and security reference

_Português: [../pt-BR/GUI.md](../pt-BR/GUI.md)_

> **Looking for how to use the GUI?** The illustrated [user manual](MANUAL.md) covers every screen,
> button and message. This page is the technical reference: how the GUI is served and secured, the
> REST + SSE API, the command line and the service.

The daemon serves a small web GUI and a REST + SSE API on **`http://127.0.0.1:4078`**, loopback
only. The GUI is plain HTML, CSS and ES modules embedded in the binary (no build step, no CDN,
under 200 KB), in English and Brazilian Portuguese.

## Opening the GUI

```
spark-pearl-miner gui              # opens the browser, already logged in
spark-pearl-miner gui --print-url  # prints the login URL instead (for SSH)
```

On the Spark itself, logged in as the account that runs the daemon, just open
**`http://127.0.0.1:4078/`**: no token, no login screen (see [Local access](#local-access-without-a-token)).

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

The app-menu entry **Spark Pearl Miner** (`packaging/spark-pearl-miner.desktop`, installed by
`packaging/install.sh` with the absolute path of the binary) runs `spark-pearl-miner gui`.

## Security of the local API

| Measure | What it does |
|---|---|
| Loopback only | The listener binds `127.0.0.1` (or `::1`). `api.lan = true` is refused: LAN access needs TLS, which this build does not implement. Use SSH forwarding. |
| Token file | `~/.config/spark-pearl-miner/api-token`, 256 random bits, mode 0600, created on first start. Whoever can read it controls the miner. |
| Session cookie | `POST /api/v1/session {"token": …}` returns an `HttpOnly; SameSite=Strict; Path=/` cookie (another random 256-bit value, kept in memory: a restart logs everyone out) and a CSRF value. Wrong tokens are answered more slowly each time. |
| CSRF | Every `POST`/`PUT`/`DELETE` under `/api/` must carry the session's value in `X-SPM-CSRF`, otherwise **403**. |
| Host / Origin | `Host` must be `127.0.0.1:4078`, `localhost:4078` or `[::1]:4078` and an `Origin`, if present, the same origin; anything else is **403** (defeats DNS rebinding). |
| Local user | With `api.trust_local_user = true` (the default), a loopback connection whose socket belongs to the daemon's own UID needs no token: `GET /api/v1/session` opens a session by itself and other `GET`s answer without a cookie. Mutations still need the cookie and `X-SPM-CSRF`. See [Local access](#local-access-without-a-token). |
| No session | Any other `/api/*` call without a valid cookie is **401**. |
| Headers | `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` on the API. |
| Text only | Strings that come from pools (errors, job ids) are rendered with `textContent`; the GUI never uses `innerHTML`. |
| Audit | Every configuration change is appended to `~/.local/state/spark-pearl-miner/audit.log` with its source (`api`, `file`, `cli`); a changed payout wallet raises a banner until you confirm it. |

The developer fee is read-only everywhere: `/api/v1/fee` has no write method, and
`PUT /api/v1/config` refuses any key that looks fee-related (`fee`, `dev…`, `donation…`) with
`422 fee_not_configurable`.

### Local access without a token

On the machine that runs the daemon, the same user account is let in without the token: opening
`http://127.0.0.1:4078/` goes straight to the dashboard. For each connection from `127.0.0.0/8` or
`::1` with no session cookie, the daemon looks up the client's socket in `/proc/net/tcp` (or
`/proc/net/tcp6`) and compares the UID that owns it with its own. Only on a match:

* `GET /api/v1/session` opens a session exactly as a token login does (`HttpOnly; SameSite=Strict`
  cookie plus a CSRF value), so the GUI starts without a login screen;
* other `GET` calls answer without a cookie (handy for `curl` on the box);
* `POST`/`PUT`/`DELETE` still need the session cookie **and** `X-SPM-CSRF`. A hostile web page
  open in your own browser runs under your UID too, which is why the UID check alone never
  authorizes a change. The session bootstrap itself is only answered to the GUI's own fetch
  (`Sec-Fetch-Site: same-origin`). Any *program* running as your account, though, can open a
  session this way and then change anything, exactly as it could by reading your token file.

It is refused when the browser says the request comes from another page (`Sec-Fetch-Site` other
than `same-origin` or `none`), when it carries `Forwarded`, `X-Forwarded-For` or `X-Real-IP` (a
proxy), and always subject to the Host/Origin allowlist above.

**Other accounts** on the same machine still need the token: the Spark is a multi-user box and
`127.0.0.1` is shared by every account, while the token file is readable only by you. Access over
the network (LAN or Tailscale, later) needs the token too. One thing to know: a tunnel or proxy that
*you* run under your own account (for example `ssh -L` logged in as you, or a `socat` you started)
connects as your UID, so whoever can use it is let in without the token. For `ssh -L` that is no
more than your SSH login already gives; do not expose such a proxy to other people.

To require the token everywhere, set `trust_local_user = false` under `[api]` in
`~/.config/spark-pearl-miner/config.toml` and restart the daemon.

## Screens (overview)

The GUI has a single route, `#/dashboard`; any other hash redirects to it. What it shows depends on
the state (each is described, with pictures, in the [manual](MANUAL.md)):

| View | When | Manual |
|---|---|---|
| Setup (3 steps: language → wallet → developer fee + **Start mining**) | `status.setup_required` (no wallet, or the fee not accepted) | [First run](MANUAL.md#first-run) |
| Dashboard (one sentence + one button, cards for rate, shares, power and this Spark, the failover line, alerts, footer) | after the setup | [Dashboard](MANUAL.md#dashboard) |
| Settings dialog (gear: wallet, worker, language, three `host:port` pool rows) | on demand | [Settings](MANUAL.md#settings) |
| Sign-in | a session that is not trusted (another account, a foreign tunnel, `trust_local_user = false`) | [Sign-in](MANUAL.md#login) |

The setup and the Settings dialog read a fresh `GET /config`, change only their own fields and
`PUT` it back, so every other key (and the hidden keys of an unchanged pool row) is kept. Live data:
the `stats` SSE event once a second, plus `GET /pools` every 2 s and on `fsm`/`timeline` events.

Display flags (client side only, never saved): `?lang=en|pt-BR` picks the page language for that
load. `?shot=…` (`wizard-2`, `wallet-error`, `wizard-3`, `wizard-3-presets`, `settings`,
`settings-error`, `settings-wallet-confirm`, `stop-confirm`, `timeline`, `alerts`) opens a dialog or
a section for `tools/screenshots.sh`; it works only while `status.worker.simulated` is true and
never writes anything.

Pause, pool switch/pin, the pool login test, the logs and the fee measurements are no longer
screens; they stay in the API below and the command line ([manual §7](MANUAL.md#cli)).

## The failover demo

On any machine, without a pool or a GPU:

```
cargo run --release -p spark-pearl-miner --example failover_demo -- --port <free port> \
    [--dir DIR] [--wallet prl1p…] [--pool1-down-s 30] [--all-down]
```

`--port` is required and cannot be 4078 (the port of a real miner). It starts three mock pools on
localhost (pool 1 refuses connections for `--pool1-down-s` seconds), the daemon with the simulated
CPU worker and a placeholder wallet, and prints the GUI URL. The failover line shows pool 2 in use
within seconds, shares accepted, and the return to pool 1 after the probe cadence (shortened in the
demo to 20 s + 10 s; the defaults are 300 s + 60 s). `--all-down` keeps every pool refusing. The same scenario runs in CI as
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
| `gpu-worker --attach <sock> [--sim]` | the GPU worker (the CUDA worker; `--sim` runs the CPU simulation instead); the daemon starts it |
| `status [--json]` | what the daemon is doing (control socket) |
| `start`, `stop`, `pause`, `resume` | controls (control socket) |
| `gui [--print-url]` | open the GUI (see above) |
| `fee-test [--connect] [--pace-ms N]` | one developer-fee cycle through the real scheduler with compressed time: PreWarm → StartSlice → EndSlice and the measured fee. Offline by default; `--connect` really logs in to the first reachable dev pool at PreWarm (never submits) |
| `version`, `--version` | version, commit, fee constants hash and the fee line |
| `install-clock-cap` | prints the `sudo` command for the boot-time 2000 MHz clock cap; changes nothing |
| `config check [--file PATH]` | validates `config.toml` as the daemon does at start: `OK: <path>`, or one `key: problem` line per error (exit status 1) |
| `config path` | prints the absolute path of `config.toml` |

The CLI talks to the daemon over `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` (0600 in a
0700 directory; connections from other users are refused after an `SO_PEERCRED` check).

## Running it as a service

`packaging/install.sh` installs everything (see the [README](../../README.md#install)): the binary
in `~/.local/bin`, `packaging/systemd/user/spark-pearl-miner.service` in `~/.config/systemd/user/`,
the app-menu entry, and `systemctl --user enable --now spark-pearl-miner`. By hand:

```
install -Dm0755 target/release/spark-pearl-miner ~/.local/bin/spark-pearl-miner
install -Dm0644 packaging/systemd/user/spark-pearl-miner.service ~/.config/systemd/user/spark-pearl-miner.service
systemctl --user daemon-reload
systemctl --user enable --now spark-pearl-miner
sudo loginctl enable-linger "$USER"   # keep it running without a desktop session
```

The unit runs `spark-pearl-miner config check` before the daemon (`ExecStartPre`), so an invalid
`config.toml` shows its reason in `journalctl --user -u spark-pearl-miner`; after 5 failed starts in
120 s systemd stops retrying. The unit restarts the daemon on failure. **Stop** in the GUI makes the worker exit, which frees
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
