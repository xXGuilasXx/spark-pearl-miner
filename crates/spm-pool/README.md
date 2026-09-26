# spm-pool — pool failover manager

Backend of the P0 request: up to three user pools in priority order, automatic failover
1 → 2 → 3 and automatic return to the best pool. The whole policy is one pure function:

```rust
pub fn step(state: &mut State, event: Event, now: Instant) -> Vec<Action>;
```

No sockets, no threads, no clock. The daemon owns the I/O, feeds events stamped with its
monotonic clock (`Instant`, milliseconds) and executes the returned actions in order. The same
inputs always give the same outputs (backoff jitter comes from the seed passed to `State::new`),
so every scenario is tested by warping `now`.

## Slot lifecycle

```
Idle ─Resolve→ Resolving ─Connect{tls}→ Connecting ─(tls)→ TlsHandshake ─Authorize→ Authorizing
                                            ▲                 │ TlsProtocolError (Auto only):
                                            └── plain retry ──┘ Close + Connect{tls:false}
Authorizing ─ack→ AwaitingJob ─first job→ Active   (failover target)
                                        → Standby  (probe: healthy, not mined)
Active ─planned switch→ Draining{≤5 s} → Idle
any failure → Backoff{until,n} (5,10,20,40,80,120 s ±20 %) | ConfigError{msg,retry_at} (auth, 10 min)
            | Quarantined{until} (ban text, 10 min) → Idle when the timer expires
Disabled: not configured or switched off in the GUI.
```

Per-stage timeouts: resolve 10 s, connect 10 s, TLS 15 s, authorize 15 s, first job 30 s.
`TlsMode::Auto` tries TLS first and falls back to plain TCP only on a TLS *protocol* error; the
working transport is cached per `host:port` once authorized. Certificate or pin errors never fall
back.

## Manager states

| State | Meaning | GPU |
|---|---|---|
| `Starting` | bringing up the first slot (pin first, else highest priority) | idle |
| `Mining{active}` | one slot is `Active` | hashing |
| `FailingOver{from,to}` | `from` failed, `to` coming up (`from == to`: same-pool reconnect) | idle |
| `AllDown{since}` | nothing usable; round-robin retries honouring backoff; first job resumes | idle |
| `Paused{UserStop \| UnsupportedScheme \| RejectEverywhere}` | frozen: every session closed | idle |
| `Paused{Yield \| Health}` | sessions stay open, failover keeps running | idle |

## Failure triggers on the active slot

DNS/connect/TLS failure or timeout · auth rejected (→ `ConfigError`, retried every 10 min) · no
job 30 s after authorize · EOF/reset (one immediate reconnect to the same pool if it had been
active > 60 s, else failover) · 900 s without a job (soft reconnect first, then failover) · ≥ 5
consecutive invalid rejects or > 50 % of the last 20 (stale and low-diff excluded) · 3 submit-ack
timeouts in a row (30 s each) · stale > 2 % of the last 100 shares · ban text (→ 10 min
quarantine) · `cert_version` ≠ 3 (→ `Paused{UnsupportedScheme}`, alert "network upgrade – update
required"; on a probe only that slot is disabled).

## Group policy

* On failure move to the next usable slot after the failed one, with wrap-around. Each slot is
  tried at most once per failover; then `AllDown`.
* A reject storm on the pool we moved to after a reject storm → `Paused{RejectEverywhere}` and the
  alert "update the miner" (the miner is the likely culprit, not the pools).
* Failback: while a lower-priority slot is active, the best usable higher-priority slot is probed
  every 300 s (`Probe` + normal lifecycle, no submits). After 60 s of stable health the manager
  switches (`SetActive`), the old session drains for 5 s (in-flight hits are still submitted on it)
  and is closed. The GPU never idles during a planned switch.
* `UserSwitch{slot}` / `UserPin{Some(slot)}` switch make-before-break and pin the slot: no
  automatic failback to other slots until `UserPin{None}`; failover on failure still happens and
  failback then returns to the pinned slot.
* `ConfigChanged{pools}` resets the edited slots; if the active slot changed (or none is active) the
  selection restarts from the top immediately.

## Driving it from the daemon

1. Build `State::new(FailoverConfig::default(), pools, seed)` and call `step(.., Event::Tick, now)`.
2. Execute actions in order: `Resolve`/`Connect`/`Authorize`/`Close` on the slot's session,
   `SetActive` + `StartGpu`/`StopGpu` on the work arbiter (switch at the next attempt boundary),
   `Submit`/`DiscardStale` for hits, `Alert`/`Log` to the GUI and journal.
3. Keep **one** timer: each `ScheduleTick{at}` replaces the previous one; deliver `Event::Tick`
   when it fires. Extra ticks are harmless.
4. Report transport outcomes as events. After `Close{slot}` drop every late event of that
   connection. A `Connect{tls: true}` means: TCP connect, report `Connected`, then run the TLS
   handshake and report `TlsOk`/`TlsProtocolError`/`TlsOtherError`.
5. Sessions opened after `Probe{slot}` are probes: never submit on them and never feed their jobs
   to the GPU until `SetActive` names them; still report their jobs with `JobReceived`.
6. Every GPU hit becomes `HitFound{slot, job_id}` with the slot whose job produced it; the reducer
   answers exactly one `Submit` (the slot is active or draining and `job_id` is its current job)
   or `DiscardStale`. Pending submits time out on their own; `SubmitAckTimeout` is optional.
7. Helpers: `Event::job_received(slot, &spm_proto::Job)`, `Event::from_submit_reply(slot, &Reply)`
   (maps ban text to `BanText`), `classify_reject`, `is_ban_text`.

The developer-fee session (M8) is not a slot of this reducer: the reducer only ever addresses the
three user slots.

## Tests

`cargo test --release -p spm-pool` runs the unit tests, the time-warped scenarios in
`tests/scenarios.rs` (fake pools answering after 50 ms) and the proptest in `tests/invariants.rs`,
which checks after every step: at most one active slot; a submit only on the originating session
whose current job is the hit's job; every hit answered exactly once; only user slots addressed;
no session re-created while draining; draining ≤ 5 s; a failure ends in a new active slot or
`AllDown` within `FailoverConfig::failover_bound()`; manager, GPU and slot states agree.
