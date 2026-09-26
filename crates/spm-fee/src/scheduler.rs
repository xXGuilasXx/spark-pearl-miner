//! Deterministic developer-fee debt scheduler.
//!
//! Pure logic: the caller injects the clock (`now`, whole seconds of a clock that keeps running
//! across restarts, e.g. UNIX time) and reports what the GPU did; the scheduler answers with
//! [`FeeAction`]s. No threads, no timers, no I/O. The only data that outlives the process is
//! [`PersistedFeeState`] (serde JSON), which carries no fee constant.
//!
//! Life cycle, driven by the daemon's work arbiter about once per second:
//! 1. `on_mining_started(now)` arms the first slice of this run at a random point in
//!    `[FIRST_SLICE_MIN_SECS, FIRST_SLICE_MAX_SECS]` (deterministic from the seed).
//! 2. `on_user_hashing(secs)` (or `on_activity`) for every second the GPU hashed for the user:
//!    debt grows by `secs * DEBT_NUM / DEBT_DEN`, capped at `DEBT_CAP_SECS`. Nothing else accrues.
//! 3. `poll(now)` until it returns `None`. `PreWarm` asks for the dev session to be opened and
//!    authorized; `StartSlice` (at least `PREWARM_SECS` later, only after the dev login succeeded)
//!    moves the GPU to the dev job; `on_dev_hashing(secs)` pays the debt down by the time really
//!    hashed; `EndSlice` or `Abort` hands the GPU back to the user.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{
    fee_disabled_for, CATCHUP_DEBT_SECS, DEBT_CAP_SECS, DEBT_DEN, DEBT_NUM, DEV_AUTH_TIMEOUT_SECS,
    DEV_RETRY_SECS, FEE_BPS, FEE_WINDOW_SECS, FIRST_SLICE_MAX_SECS, FIRST_SLICE_MIN_SECS, MIN_SLICE_GAP_SECS,
    MIN_SLICE_SECS, PREWARM_SECS, SLICE_SECS, SUSPEND_REJECT_RATIO, SUSPEND_SECS, SUSPEND_WINDOW_SHARES,
};

/// Format version of [`PersistedFeeState`].
pub const STATE_VERSION: u32 = 1;

// Debt is kept exactly, in units of 1/DEBT_DEN second, so 1 s ticks accrue without rounding.
const SLICE_UNITS: u64 = SLICE_SECS * DEBT_DEN;
const CAP_UNITS: u64 = DEBT_CAP_SECS * DEBT_DEN;
const CATCHUP_UNITS: u64 = CATCHUP_DEBT_SECS * DEBT_DEN;

/// What the daemon must do after [`FeeScheduler::poll`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeAction {
    /// Open and authorize the dev session now; a slice follows in about `PREWARM_SECS`.
    PreWarm,
    /// Move the GPU to the dev session's job and report the time with `on_dev_hashing`.
    StartSlice,
    /// The slice is paid: move the GPU back to the user session and close the dev session.
    EndSlice,
    /// Cancel the pending or running slice: back to the user session, close the dev session.
    /// Only dev time really hashed after the login counts; the rest of the debt is kept.
    Abort(AbortReason),
}

/// Why a slice was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbortReason {
    /// The dev pool refused the login (all dev pools failed).
    AuthorizeFailed,
    /// No login result within `DEV_AUTH_TIMEOUT_SECS` of PreWarm.
    AuthorizeTimeout,
    /// The dev session dropped.
    SessionLost,
    /// Too many dev submits were rejected; the fee is suspended.
    Suspended,
    /// The user stopped mining.
    MiningStopped,
}

/// What the GPU did during an interval (see [`FeeScheduler::on_activity`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Activity {
    /// Hashing for the user's pool: the only activity that accrues debt.
    UserHashing,
    /// Hashing for the dev session inside a slice: pays debt down.
    DevHashing,
    /// Paused by the user.
    Paused,
    /// Yielding the GPU to vLLM or another workload.
    Yielding,
    /// No job / no pool.
    Idle,
    /// `--benchmark`: no pool, no fee.
    Benchmark,
    /// `--mock`: no GPU work, no fee.
    Mock,
}

/// Coarse state for UIs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeePhase {
    /// The user's wallet is the dev wallet: no fee at all.
    Disabled,
    /// Accruing debt, waiting for the next slice.
    Waiting,
    /// Reject-ratio suspension in force.
    Suspended,
    /// Dev session being opened and authorized.
    PreWarm,
    /// Dev slice in progress.
    Slice,
}

/// Accounting snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct FeeStats {
    pub enabled: bool,
    pub phase: FeePhase,
    /// Seconds the GPU hashed for the user.
    pub user_hash_secs: u64,
    /// Seconds the GPU hashed for the dev session after a successful dev login.
    pub dev_hash_secs: u64,
    pub dev_shares_accepted: u64,
    pub dev_shares_rejected: u64,
    /// `dev / (user + dev) * 100` over the whole history.
    pub measured_fee_pct: f64,
    /// Same ratio over the trailing `FEE_WINDOW_SECS` of active mining time.
    pub window_fee_pct: f64,
    /// Estimated start of the next slice, assuming continuous hashing from now on.
    pub next_slice_at: Option<u64>,
    /// Unpaid dev time, seconds.
    pub debt_secs: f64,
    pub fee_suspended_until: Option<u64>,
    pub slices_paid: u64,
    pub slices_aborted: u64,
}

/// One run of equal activity in the rolling window (active time only; pauses are not stored).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSegment {
    pub dev: bool,
    pub secs: u64,
}

/// Everything that must survive a restart. Contains no wallet, pool, rate or schedule constant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedFeeState {
    pub version: u32,
    pub seed: u64,
    /// Number of runs that armed a first slice (varies the random first-slice point per run).
    pub runs: u64,
    /// Unpaid dev time: `debt_secs + debt_frac / DEBT_DEN` seconds.
    pub debt_secs: u64,
    pub debt_frac: u64,
    /// Earliest time of the next slice (same clock as `now`).
    pub next_slice_at: Option<u64>,
    /// User hashing since the last slice (enforces `MIN_SLICE_GAP_SECS` across restarts).
    pub user_secs_since_slice: u64,
    pub user_hash_secs: u64,
    pub dev_hash_secs: u64,
    pub dev_shares_accepted: u64,
    pub dev_shares_rejected: u64,
    pub slices_paid: u64,
    pub slices_aborted: u64,
    pub fee_suspended_until: Option<u64>,
    /// Most recent dev submit results, oldest first (`true` = accepted).
    pub recent_dev_submits: Vec<bool>,
    /// Trailing `FEE_WINDOW_SECS` of active time, oldest first.
    pub window: Vec<WindowSegment>,
}

impl PersistedFeeState {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Trailing window of active mining time, as run-length segments.
#[derive(Clone, Debug, Default)]
struct Window {
    segs: VecDeque<WindowSegment>,
    total: u64,
    dev: u64,
}

impl Window {
    fn push(&mut self, dev: bool, secs: u64) {
        if secs == 0 {
            return;
        }
        match self.segs.back_mut() {
            Some(last) if last.dev == dev => last.secs = last.secs.saturating_add(secs),
            _ => self.segs.push_back(WindowSegment { dev, secs }),
        }
        self.total = self.total.saturating_add(secs);
        if dev {
            self.dev = self.dev.saturating_add(secs);
        }
        while self.total > FEE_WINDOW_SECS {
            let excess = self.total - FEE_WINDOW_SECS;
            let Some(front) = self.segs.front_mut() else { break };
            let take = front.secs.min(excess);
            front.secs -= take;
            self.total -= take;
            if front.dev {
                self.dev -= take;
            }
            if front.secs == 0 {
                self.segs.pop_front();
            }
        }
    }

    /// Dev seconds among the oldest `k` seconds of the window.
    fn dev_in_oldest(&self, mut k: u64) -> u64 {
        let mut dev = 0;
        for s in &self.segs {
            if k == 0 {
                break;
            }
            let take = s.secs.min(k);
            if s.dev {
                dev += take;
            }
            k -= take;
        }
        dev
    }

    /// (dev seconds, window length) after appending `user` then `dev` seconds.
    fn after_append(&self, user: u64, dev: u64) -> (u64, u64) {
        let total = self.total + user + dev;
        if total <= FEE_WINDOW_SECS {
            return (self.dev + dev, total);
        }
        let drop = total - FEE_WINDOW_SECS;
        let from_hist = drop.min(self.total);
        let from_new_dev = (drop - from_hist).saturating_sub(user);
        let dropped_dev = self.dev_in_oldest(from_hist) + from_new_dev;
        (self.dev + dev - dropped_dev, FEE_WINDOW_SECS)
    }

    /// True when appending `user` then `dev` seconds keeps the window at or below FEE_BPS.
    fn allows(&self, user: u64, dev: u64) -> bool {
        let (d, len) = self.after_append(user, dev);
        d.saturating_mul(10_000) <= u64::from(FEE_BPS).saturating_mul(len)
    }

    /// Longest dev run (≤ `cap`) that can follow `user` more seconds without the window ever
    /// exceeding FEE_BPS. The excess is non-decreasing in the run length, so the check at the end
    /// of the run covers every window that ends inside it, and a binary search is exact.
    fn max_dev_run(&self, user: u64, cap: u64) -> u64 {
        if !self.allows(user, 0) {
            return 0;
        }
        let (mut lo, mut hi) = (0, cap);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.allows(user, mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    PreWarm { since: u64, authorized: bool },
    Slice { planned: u64, paid: u64 },
}

/// The developer-fee debt scheduler. See the module documentation.
#[derive(Clone, Debug)]
pub struct FeeScheduler {
    enabled: bool,
    seed: u64,
    runs: u64,
    clock: u64,
    armed_this_run: bool,
    next_slice_at: Option<u64>,
    debt_units: u64,
    user_since_slice: u64,
    user_hash_secs: u64,
    dev_hash_secs: u64,
    accepted: u64,
    rejected: u64,
    slices_paid: u64,
    slices_aborted: u64,
    suspended_until: Option<u64>,
    submits: VecDeque<bool>,
    window: Window,
    phase: Phase,
    pending: VecDeque<FeeAction>,
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl FeeScheduler {
    /// Fresh state. `user_wallet` is the user's own payout wallet, used only for the auto-off rule
    /// (the fee is disabled when it is the dev wallet). `seed` should come from the OS RNG once
    /// and is persisted.
    pub fn new(seed: u64, user_wallet: &str) -> Self {
        Self {
            enabled: !fee_disabled_for(user_wallet),
            seed,
            runs: 0,
            clock: 0,
            armed_this_run: false,
            next_slice_at: None,
            debt_units: 0,
            user_since_slice: 0,
            user_hash_secs: 0,
            dev_hash_secs: 0,
            accepted: 0,
            rejected: 0,
            slices_paid: 0,
            slices_aborted: 0,
            suspended_until: None,
            submits: VecDeque::with_capacity(SUSPEND_WINDOW_SHARES),
            window: Window::default(),
            phase: Phase::Idle,
            pending: VecDeque::new(),
        }
    }

    /// Rebuilds a scheduler from persisted state (the debt is clamped to the cap, the window to
    /// `FEE_WINDOW_SECS`). A slice that was running when the state was saved is not resumed: what
    /// it paid is already off the debt, the rest stays owed.
    pub fn restore(state: PersistedFeeState, user_wallet: &str) -> Self {
        let mut s = Self::new(state.seed, user_wallet);
        let frac = state.debt_frac.min(DEBT_DEN - 1);
        s.debt_units = state.debt_secs.saturating_mul(DEBT_DEN).saturating_add(frac).min(CAP_UNITS);
        s.runs = state.runs;
        s.next_slice_at = state.next_slice_at;
        s.user_since_slice = state.user_secs_since_slice;
        s.user_hash_secs = state.user_hash_secs;
        s.dev_hash_secs = state.dev_hash_secs;
        s.accepted = state.dev_shares_accepted;
        s.rejected = state.dev_shares_rejected;
        s.slices_paid = state.slices_paid;
        s.slices_aborted = state.slices_aborted;
        s.suspended_until = state.fee_suspended_until;
        let skip = state.recent_dev_submits.len().saturating_sub(SUSPEND_WINDOW_SHARES);
        s.submits.extend(state.recent_dev_submits.iter().skip(skip));
        for seg in state.window {
            s.window.push(seg.dev, seg.secs);
        }
        s
    }

    /// State to persist. Save it after every action returned by `poll` and periodically; a slice
    /// is never paid twice because paid time leaves the debt as it is hashed.
    pub fn persisted(&self) -> PersistedFeeState {
        PersistedFeeState {
            version: STATE_VERSION,
            seed: self.seed,
            runs: self.runs,
            debt_secs: self.debt_units / DEBT_DEN,
            debt_frac: self.debt_units % DEBT_DEN,
            next_slice_at: self.next_slice_at,
            user_secs_since_slice: self.user_since_slice,
            user_hash_secs: self.user_hash_secs,
            dev_hash_secs: self.dev_hash_secs,
            dev_shares_accepted: self.accepted,
            dev_shares_rejected: self.rejected,
            slices_paid: self.slices_paid,
            slices_aborted: self.slices_aborted,
            fee_suspended_until: self.suspended_until,
            recent_dev_submits: self.submits.iter().copied().collect(),
            window: self.window.segs.iter().copied().collect(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// True while the GPU should be hashing for the dev session.
    pub fn in_slice(&self) -> bool {
        matches!(self.phase, Phase::Slice { .. })
    }

    /// End of the reject-ratio suspension, if one is in force.
    pub fn fee_suspended_until(&self) -> Option<u64> {
        self.suspended_until.filter(|&until| self.clock < until)
    }

    /// Mining (for a real user pool) started or resumed. Arms the first slice of this run once:
    /// a pending slice time that is still ahead (within the first-slice window) is kept, so crash
    /// loops cannot postpone the fee; otherwise a new random point is drawn.
    pub fn on_mining_started(&mut self, now: u64) {
        self.clock = now;
        if !self.enabled || self.armed_this_run {
            return;
        }
        self.armed_this_run = true;
        let keep = matches!(self.next_slice_at,
            Some(t) if t > now && t <= now.saturating_add(FIRST_SLICE_MAX_SECS));
        if !keep {
            let span = FIRST_SLICE_MAX_SECS - FIRST_SLICE_MIN_SECS + 1;
            let offset = FIRST_SLICE_MIN_SECS + splitmix64(self.seed ^ splitmix64(self.runs)) % span;
            self.next_slice_at = Some(now.saturating_add(offset));
            self.runs = self.runs.saturating_add(1);
        }
    }

    /// The user stopped mining: a pending or running slice is aborted (paid time stays paid).
    pub fn on_mining_stopped(&mut self, now: u64) {
        self.clock = now;
        if !matches!(self.phase, Phase::Idle) {
            let a = self.abort(now, AbortReason::MiningStopped);
            self.pending.push_back(a);
        }
    }

    /// Reports an interval of GPU activity; only `UserHashing` accrues and only `DevHashing`
    /// (inside a slice) pays. Paused, yielding, idle, benchmark and mock time do nothing.
    pub fn on_activity(&mut self, activity: Activity, secs: u64) {
        match activity {
            Activity::UserHashing => self.on_user_hashing(secs),
            Activity::DevHashing => self.on_dev_hashing(secs),
            Activity::Paused | Activity::Yielding | Activity::Idle | Activity::Benchmark | Activity::Mock => {}
        }
    }

    /// `secs` seconds hashed for the user: debt += secs * DEBT_NUM / DEBT_DEN (exact), up to the cap.
    /// Nothing accrues during a reject-ratio suspension.
    pub fn on_user_hashing(&mut self, secs: u64) {
        if secs == 0 {
            return;
        }
        self.user_hash_secs = self.user_hash_secs.saturating_add(secs);
        if !self.enabled {
            return;
        }
        self.window.push(false, secs);
        self.user_since_slice = self.user_since_slice.saturating_add(secs);
        if self.fee_suspended_until().is_none() {
            self.debt_units = self.debt_units.saturating_add(secs.saturating_mul(DEBT_NUM)).min(CAP_UNITS);
        }
    }

    /// `secs` seconds hashed for the dev session. Counted only inside a slice, which only starts
    /// after the dev login succeeded; pays the debt down by the time really hashed.
    pub fn on_dev_hashing(&mut self, secs: u64) {
        if secs == 0 || !self.enabled {
            return;
        }
        let Phase::Slice { paid, .. } = &mut self.phase else { return };
        *paid = paid.saturating_add(secs);
        self.dev_hash_secs = self.dev_hash_secs.saturating_add(secs);
        self.window.push(true, secs);
        self.debt_units = self.debt_units.saturating_sub(secs.saturating_mul(DEBT_DEN));
    }

    /// The dev session logged in.
    pub fn on_dev_authorized(&mut self, now: u64) {
        self.clock = now;
        if let Phase::PreWarm { authorized, .. } = &mut self.phase {
            *authorized = true;
        }
    }

    /// The dev login failed: the slice is aborted before any fee time, the debt is kept.
    pub fn on_dev_authorize_failed(&mut self, now: u64) {
        self.clock = now;
        if !matches!(self.phase, Phase::Idle) {
            let a = self.abort(now, AbortReason::AuthorizeFailed);
            self.pending.push_back(a);
        }
    }

    /// The dev session dropped: the slice ends; only the time already hashed was paid.
    pub fn on_dev_session_lost(&mut self, now: u64) {
        self.clock = now;
        if !matches!(self.phase, Phase::Idle) {
            let a = self.abort(now, AbortReason::SessionLost);
            self.pending.push_back(a);
        }
    }

    /// Result of a dev submit. When more than SUSPEND_REJECT_RATIO of the last
    /// SUSPEND_WINDOW_SHARES are rejected, the fee is suspended for SUSPEND_SECS.
    pub fn on_dev_share(&mut self, accepted: bool, now: u64) {
        self.clock = now;
        if !self.enabled {
            return;
        }
        if accepted {
            self.accepted = self.accepted.saturating_add(1);
        } else {
            self.rejected = self.rejected.saturating_add(1);
        }
        self.submits.push_back(accepted);
        while self.submits.len() > SUSPEND_WINDOW_SHARES {
            self.submits.pop_front();
        }
        if self.submits.len() == SUSPEND_WINDOW_SHARES {
            let rejected = self.submits.iter().filter(|&&ok| !ok).count();
            if rejected as f32 / SUSPEND_WINDOW_SHARES as f32 > SUSPEND_REJECT_RATIO {
                self.suspended_until = Some(now.saturating_add(SUSPEND_SECS));
                self.submits.clear();
            }
        }
    }

    /// Next action for the daemon, or `None`. Call it until it returns `None`, at least once per
    /// second; report activity between calls.
    pub fn poll(&mut self, now: u64) -> Option<FeeAction> {
        self.clock = now;
        if let Some(a) = self.pending.pop_front() {
            return Some(a);
        }
        if !self.enabled {
            return None;
        }
        if self.suspended_until.is_some_and(|until| now >= until) {
            self.suspended_until = None;
        }
        let suspended = self.suspended_until.is_some();
        match self.phase {
            Phase::Idle => {
                let gate = self.next_slice_at?;
                if suspended || !self.armed_this_run || now.saturating_add(PREWARM_SECS) < gate {
                    return None;
                }
                self.slice_len_if_ready(PREWARM_SECS)?;
                self.phase = Phase::PreWarm { since: now, authorized: false };
                Some(FeeAction::PreWarm)
            }
            Phase::PreWarm { since, authorized } => {
                if suspended {
                    return Some(self.abort(now, AbortReason::Suspended));
                }
                if !authorized {
                    if now >= since.saturating_add(DEV_AUTH_TIMEOUT_SECS) {
                        return Some(self.abort(now, AbortReason::AuthorizeTimeout));
                    }
                    return None;
                }
                if now < since.saturating_add(PREWARM_SECS) || self.next_slice_at.is_some_and(|t| now < t) {
                    return None;
                }
                let planned = self.slice_len_if_ready(0)?;
                self.phase = Phase::Slice { planned, paid: 0 };
                self.user_since_slice = 0;
                Some(FeeAction::StartSlice)
            }
            Phase::Slice { planned, paid } => {
                if suspended {
                    return Some(self.abort(now, AbortReason::Suspended));
                }
                if paid < planned {
                    return None;
                }
                self.phase = Phase::Idle;
                self.slices_paid = self.slices_paid.saturating_add(1);
                self.user_since_slice = 0;
                self.next_slice_at = Some(now);
                Some(FeeAction::EndSlice)
            }
        }
    }

    /// Accounting snapshot.
    pub fn stats(&self) -> FeeStats {
        let active = self.user_hash_secs.saturating_add(self.dev_hash_secs);
        let pct = |dev: u64, total: u64| if total == 0 { 0.0 } else { dev as f64 * 100.0 / total as f64 };
        let suspended = self.fee_suspended_until();
        let phase = match (self.enabled, self.phase) {
            (false, _) => FeePhase::Disabled,
            (true, Phase::Slice { .. }) => FeePhase::Slice,
            (true, Phase::PreWarm { .. }) => FeePhase::PreWarm,
            (true, Phase::Idle) if suspended.is_some() => FeePhase::Suspended,
            (true, Phase::Idle) => FeePhase::Waiting,
        };
        FeeStats {
            enabled: self.enabled,
            phase,
            user_hash_secs: self.user_hash_secs,
            dev_hash_secs: self.dev_hash_secs,
            dev_shares_accepted: self.accepted,
            dev_shares_rejected: self.rejected,
            measured_fee_pct: pct(self.dev_hash_secs, active),
            window_fee_pct: pct(self.window.dev, self.window.total),
            next_slice_at: self.next_slice_eta(),
            debt_secs: self.debt_units as f64 / DEBT_DEN as f64,
            fee_suspended_until: suspended,
            slices_paid: self.slices_paid,
            slices_aborted: self.slices_aborted,
        }
    }

    /// Length of the slice that could start after `extra_user` more seconds of user hashing, or
    /// `None` if none could. Every input only grows with more user hashing, so the prediction
    /// made at PreWarm (`extra_user = PREWARM_SECS`) holds when those seconds have been hashed.
    fn slice_len_if_ready(&self, extra_user: u64) -> Option<u64> {
        let debt = self.debt_units.saturating_add(extra_user.saturating_mul(DEBT_NUM)).min(CAP_UNITS);
        if debt < SLICE_UNITS || self.user_since_slice.saturating_add(extra_user) < MIN_SLICE_GAP_SECS {
            return None;
        }
        if debt > CATCHUP_UNITS {
            // Backlog from a dev-pool outage: full slices, spaced by MIN_SLICE_GAP_SECS.
            return Some(SLICE_SECS);
        }
        let len = self.window.max_dev_run(extra_user, SLICE_SECS);
        (len >= MIN_SLICE_SECS).then_some(len)
    }

    fn abort(&mut self, now: u64, reason: AbortReason) -> FeeAction {
        if let Phase::Slice { paid, .. } = self.phase {
            if paid > 0 {
                self.user_since_slice = 0;
            }
        }
        self.phase = Phase::Idle;
        self.slices_aborted = self.slices_aborted.saturating_add(1);
        match reason {
            AbortReason::AuthorizeFailed | AbortReason::AuthorizeTimeout | AbortReason::SessionLost => {
                self.next_slice_at = Some(now.saturating_add(DEV_RETRY_SECS));
            }
            AbortReason::Suspended | AbortReason::MiningStopped => {}
        }
        FeeAction::Abort(reason)
    }

    fn next_slice_eta(&self) -> Option<u64> {
        if !self.enabled || !self.armed_this_run {
            return None;
        }
        match self.phase {
            Phase::Slice { .. } => None,
            Phase::PreWarm { since, .. } => {
                Some(since.saturating_add(PREWARM_SECS).max(self.next_slice_at.unwrap_or(0)))
            }
            Phase::Idle => {
                let gate = self.next_slice_at?;
                let base = self.clock.max(self.fee_suspended_until().unwrap_or(0));
                let debt_short = SLICE_UNITS.saturating_sub(self.debt_units).div_ceil(DEBT_NUM);
                let gap_short = MIN_SLICE_GAP_SECS.saturating_sub(self.user_since_slice);
                Some(gate.max(base.saturating_add(debt_short.max(gap_short))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_of(segs: &[(bool, u64)]) -> Window {
        let mut w = Window::default();
        for &(dev, secs) in segs {
            w.push(dev, secs);
        }
        w
    }

    #[test]
    fn window_trims_to_its_length_and_merges_runs() {
        let w = window_of(&[(false, 50_000), (false, 30_000), (true, 120), (false, 10_000)]);
        assert_eq!(w.total, FEE_WINDOW_SECS);
        assert_eq!(w.dev, 120);
        assert_eq!(w.segs.len(), 3);
        assert_eq!(w.segs[0], WindowSegment { dev: false, secs: FEE_WINDOW_SECS - 10_120 });
    }

    #[test]
    fn first_slice_is_exactly_at_the_ceiling() {
        // 5880 s of user time owes exactly 120 s, and 120/6000 is exactly 2.00 %.
        let w = window_of(&[(false, 5_880)]);
        assert_eq!(w.max_dev_run(0, SLICE_SECS), 120);
        let w = window_of(&[(false, 5_870)]);
        assert_eq!(w.max_dev_run(10, SLICE_SECS), 120);
        assert_eq!(w.max_dev_run(0, SLICE_SECS), 119);
    }

    #[test]
    fn full_window_ceiling_accounts_for_dev_time_sliding_out() {
        // Continuous hashing: after 14 slices of 120 s every 6000 s and 5880 more user seconds,
        // the full window holds 1680 s of dev time and leaves 48 s of room...
        let mut segs = Vec::new();
        for _ in 0..14 {
            segs.push((false, 5_880));
            segs.push((true, 120));
        }
        segs.push((false, 5_880));
        let w = window_of(&segs);
        assert_eq!((w.total, w.dev), (FEE_WINDOW_SECS, 14 * 120));
        assert_eq!(w.max_dev_run(0, SLICE_SECS), 48);
        // ...but once the oldest slice is about to slide out, a full slice fits again.
        let mut w = window_of(&segs);
        w.push(true, 48);
        w.push(false, 2_352);
        assert_eq!(w.dev, 14 * 120 + 48);
        assert_eq!(w.max_dev_run(0, SLICE_SECS), 120);
    }

    #[test]
    fn debt_is_exact_in_one_second_ticks() {
        let mut s = FeeScheduler::new(1, "prl1user");
        for _ in 0..98 {
            s.on_user_hashing(1);
        }
        assert_eq!(s.debt_units, 2 * DEBT_DEN);
        assert_eq!(s.persisted().debt_secs, 2);
        assert_eq!(s.persisted().debt_frac, 0);
    }

    #[test]
    fn debt_is_capped() {
        let mut s = FeeScheduler::new(1, "prl1user");
        s.on_user_hashing(10 * 24 * 3600);
        assert_eq!(s.debt_units, CAP_UNITS);
        let st = s.persisted();
        assert_eq!((st.debt_secs, st.debt_frac), (DEBT_CAP_SECS, 0));
    }

    #[test]
    fn restore_clamps_bad_input() {
        let st = PersistedFeeState {
            debt_secs: u64::MAX,
            debt_frac: u64::MAX,
            recent_dev_submits: vec![false; 100],
            window: vec![WindowSegment { dev: false, secs: u64::MAX }],
            ..Default::default()
        };
        let s = FeeScheduler::restore(st, "prl1user");
        assert_eq!(s.debt_units, CAP_UNITS);
        assert_eq!(s.submits.len(), SUSPEND_WINDOW_SHARES);
        assert_eq!(s.window.total, FEE_WINDOW_SECS);
    }
}
