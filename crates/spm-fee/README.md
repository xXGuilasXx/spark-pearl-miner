# spm-fee

`dev fee 2.00% → prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n @ br.pearl.herominers.com:1200 (HeroMiners), worker "devfee", 120 s slices, only while mining`

This crate holds the developer fee of spark-pearl-miner and nothing else:

- `src/lib.rs` holds **every** fee constant (rate, wallet, worker, pools, schedule). The READMEs and FEE docs must repeat the line above exactly; `tools/check-fee-consistency.py` fails CI otherwise.
- `src/scheduler.rs` holds `FeeScheduler`, the deterministic debt scheduler that decides when the GPU works for the developer. It is pure logic: the daemon injects the clock and reports what the GPU did. It has no threads, no timers and no I/O. The only thing it persists is `PersistedFeeState`, as serde JSON.

## The fee wallet is not configurable

The fee wallet is not configurable; forks can recompile, official releases are attested.

There is no override path of any kind for `DEV_WALLET`, `FEE_BPS`, `DEV_POOLS`, `DEV_WORKER` or the schedule: no CLI flag, no environment variable, no config key, no API or IPC call, no setter. They are compile-time `pub const` items. They can be read through `fee_constants()` (a plain struct, by value) and summarized by `constants_hash()`, a BLAKE3 hash of `canonical_constants()` that `--version` prints. The only wallet the scheduler accepts is the user's own, and it is used for one thing: the fee turns itself off when the user mines to the developer wallet. The persisted state contains no wallet, pool, rate or schedule field. `tests/policy.rs` enforces all of this. A unit test pins the exact wallet string and the constants hash, so changing either one is a deliberate edit.

## Constants

| Constant | Value | Meaning |
|---|---|---|
| `FEE_BPS` | 200 | 2.00 % of *active mining time* |
| `DEV_WALLET` | `prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n` | self-custody Pearl P2TR address (bech32m, checked by a test) |
| `DEV_WORKER` | devfee | worker name on the dev session |
| `DEV_POOLS` | br.pearl.herominers.com:1200, us.pearl.herominers.com:1200, de.pearl.herominers.com:1200, pearl-br.luckypool.io:3360, prl-br.kryptex.network:8048 | dev pools in preference order (HeroMiners region first, then fallbacks) |
| `SLICE_SECS` | 120 | length of one dev slice |
| `DEBT_NUM` / `DEBT_DEN` | 200 / 9800 | dev seconds owed per user second |
| `FIRST_SLICE_MIN_SECS` | 1800 | earliest first slice of a run (30 min) |
| `FIRST_SLICE_MAX_SECS` | 5400 | latest random first-slice point (90 min) |
| `DEBT_CAP_SECS` | 3600 | maximum unpaid debt |
| `SUSPEND_REJECT_RATIO` | 0.5 | suspend when more than this share of dev submits is rejected... |
| `SUSPEND_WINDOW_SHARES` | 20 | ...over the last 20 dev submits... |
| `SUSPEND_SECS` | 3600 | ...for 1 h |
| `PREWARM_SECS` | 10 | the dev session is opened this long before a slice |
| `DEV_AUTH_TIMEOUT_SECS` | 60 | give up on a dev login after this long |
| `DEV_RETRY_SECS` | 300 | wait after a failed or dropped dev session |
| `FEE_WINDOW_SECS` | 86400 | rolling window (24 h of active time) for the 2.00 % ceiling |
| `MIN_SLICE_SECS` | 30 | shortest slice the ceiling may leave |
| `CATCHUP_DEBT_SECS` | 240 | debt above this is an outage backlog |
| `MIN_SLICE_GAP_SECS` | 1800 | minimum user hashing between two slices (30 min) |

## How the scheduler works

- **Debt accrues only while hashing for the user.** Each second hashed for the user's pool adds 200/9800 s of debt. The arithmetic is exact, in units of 1/9800 s. Paused, yielding (vLLM), idle, `--benchmark` and `--mock` time adds nothing. One slice is owed after 5880 s (98 min) of hashing: 5880 s for the user plus 120 s for the developer is 6000 s, and 120/6000 = 2.00 %.
- **First slice.** When mining starts, a random point in [30, 90] min is drawn from the persisted seed and a per-run counter. The first slice starts there if at least 120 s of debt is owed by then, otherwise as soon as it is. On a fresh install the debt only reaches 120 s after 98 min, so the random point matters when debt was carried over from an earlier run. If a restart finds a pending slice time that is still ahead, it keeps that time, so crash loops cannot postpone the fee. Pausing and resuming within a run never re-draws the point.
- **PreWarm, then slice.** `PreWarm` comes 10 s before a slice (the conditions are evaluated with 10 more seconds of hashing), so the daemon opens and authorizes the dev session in advance. `StartSlice` comes only after the dev login succeeded (the XMRig rule: fee time counts only after the dev login) and at least 10 s after `PreWarm`. A slice pays the debt down by the time really hashed and ends with `EndSlice` after 120 s.
- **Abort.** A refused login, no login within 60 s, a dropped dev session, a suspension or a stop yields `Abort(reason)`. Only dev time really hashed counts; the rest of the debt is kept, and the next try waits 300 s. The user session never stops while the dev pool is down.
- **24 h ceiling.** In any 24 h of active mining time (86400 s of user plus dev hashing; pauses are not part of the window), dev time stays at or below 2.00 % (1728 s). Without this, whole 120 s slices every 6000 s would put 15 slices (1800 s, 2.08 %) into some windows. The scheduler shortens a slice when needed, but never below 30 s. The first slice after 98 min sits exactly at the ceiling. Over a month of continuous hashing the measured fee is 1.998 %.
- **Outage backlog.** Debt is capped at 3600 s, so a long dev-pool outage never turns into a long dev run. Debt above 240 s is a backlog. It is repaid in full 120 s slices, each after at least 30 min of user hashing (at most 120 s per 32 min, 6.25 % locally). The whole 3600 s cap takes about 21 h to repay. Repaying a backlog is the one case where some 24 h windows go above 2.00 %. This cannot be avoided: new debt accrues at exactly the ceiling rate, so a backlog can only be repaid above it. The cap and the gap bound it.
- **Reject-ratio suspension.** If more than half of the last 20 dev submits are rejected, the fee is suspended for 1 h (`fee_suspended_until`). A running slice is aborted, and no debt accrues during that hour. This fails in the user's favour.
- **Auto-off.** When the user's wallet is `DEV_WALLET` (trimmed, case-insensitive), the scheduler is disabled: no debt, no slices.
- **Persistence.** `persisted()` returns everything that must survive a restart: exact debt, next slice time, the gap counter, the 24 h window, the recent dev submits, the suspension and the accounting. `restore()` clamps bad input (debt to the cap, window to 24 h). Paid time leaves the debt as it is hashed, so a restart mid-slice neither loses debt nor pays twice. The next slice after a restart still waits for the first-slice point and the 30 min gap.
- **Accounting.** `stats()` reports user and dev hashing seconds, accepted and rejected dev shares, `measured_fee_pct` = dev / (user + dev) × 100, the same ratio over the 24 h window, an estimated `next_slice_at`, the debt, the suspension and slice counters.

## Daemon contract

About once per second: report the last interval with `on_activity` (or `on_user_hashing` / `on_dev_hashing`), then call `poll(now)` until it returns `None`. Pass dev-session events as they happen (`on_dev_authorized`, `on_dev_authorize_failed`, `on_dev_session_lost`, `on_dev_share`). Save `persisted()` after every action and periodically. `now` is any seconds clock that keeps running across restarts, such as UNIX time.

## Tests

`cargo test --release -p spm-fee` runs deterministic simulations at 1 s ticks:

- 30 days of continuous hashing measure 2.00 % ± 0.02 %.
- Random pauses: every rolling 24 h window is at or below 2.0 %, paid plus owed equals 200/9800 of user time exactly, and nothing accrues during pauses.
- Injected dev failures keep the same guarantees.
- A restart before and in the middle of a slice loses no debt and pays nothing twice.
- A 10 h outage: the user keeps mining, and the backlog is repaid in 120 s slices 30 min apart.
- A 3-day outage stops exactly at the 3600 s cap, which is then repaid in 120 s slices.
- A failed dev login aborts without fee time.
- Reject-ratio suspension.
- Auto-off for the dev wallet.
- The first slice lands in [30, 90] min.
- `PreWarm` precedes every `StartSlice` by 10 s.
- The policy checks (no override path, README numbers match `src/lib.rs`).
