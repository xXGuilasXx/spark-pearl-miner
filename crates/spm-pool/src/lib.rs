//! spm-pool — the pool failover manager of spark-pearl-miner.
//!
//! The whole policy is one **pure reducer**:
//!
//! ```text
//! step(&mut State, Event, now) -> Vec<Action>
//! ```
//!
//! It owns no sockets, no threads and never reads a clock: the daemon feeds it transport events
//! and user requests stamped with a monotonic [`Instant`], executes the returned [`Action`]s and
//! arms a single timer from the latest [`Action::ScheduleTick`]. Because every decision is a
//! function of `(state, event, now)`, the policy is tested deterministically by warping `now`
//! (see `tests/`), including a proptest over random event sequences.
//!
//! Policy summary (the P0 item of `TODO.md`, detailed in `docs/en/ARCHITECTURE.md`, "Failover"):
//! up to three user slots in priority order; mine on the highest-priority usable slot; on any
//! failure of the active slot move to the next usable slot with wrap-around; when every slot is
//! unusable go `AllDown` and retry round-robin honouring each slot's backoff; while a
//! lower-priority slot is active, probe the best higher-priority slot every 300 s and fail back
//! after 60 s of stable health, draining the old session for up to 5 s.
//!
//! Daemon contract (also in `README.md`):
//! * After [`Action::Close`] for a slot the daemon delivers no further events from that
//!   connection; the next session of the slot starts with [`Action::Resolve`].
//! * [`Action::Connect`] with `tls: true` means "TCP connect, report [`Event::Connected`], then run
//!   the TLS handshake and report [`Event::TlsOk`] / [`Event::TlsProtocolError`] /
//!   [`Event::TlsOtherError`]"; pinned certificates (LuckyPool) are checked by the daemon and a
//!   pin mismatch is a `TlsOtherError`.
//! * A session opened after [`Action::Probe`] is a probe: never submit on it and never feed its
//!   jobs to the GPU until [`Action::SetActive`] names it. Its jobs are still reported with
//!   [`Event::JobReceived`].
//! * [`Action::SetActive`] is applied by the work arbiter at the next attempt boundary; hits from
//!   the attempt in flight still carry the old slot and are submitted while it drains.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Add;
use std::time::Duration;

/// Maximum number of user pool slots ("até 3 endereços de pool").
pub const MAX_SLOTS: usize = 3;
/// The only certificate version this build produces proofs for (V3, salted seeds).
pub const SUPPORTED_CERT_VERSION: u32 = 3;
/// Alert text when a pool announces a certificate version this build cannot mine.
pub const UPDATE_REQUIRED_ALERT: &str = "network upgrade – update required";
/// Alert text when shares are rejected as invalid on the pool we failed over to as well.
pub const REJECT_EVERYWHERE_ALERT: &str = "shares are rejected on every pool – update the miner";
/// Alert text when every configured slot is unusable.
pub const ALL_DOWN_ALERT: &str = "all pools are down – GPU idle, retrying with backoff";

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// A point on the daemon's monotonic clock, in milliseconds since an arbitrary origin.
/// The reducer never reads a clock; every [`step`] receives `now` from the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Instant(u64);

impl Instant {
    /// The origin of the clock.
    pub const ZERO: Instant = Instant(0);

    /// An instant `ms` milliseconds after the origin.
    pub const fn from_millis(ms: u64) -> Self {
        Instant(ms)
    }

    /// An instant `secs` seconds after the origin.
    pub const fn from_secs(secs: u64) -> Self {
        Instant(secs.saturating_mul(1000))
    }

    /// Milliseconds since the origin.
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Time elapsed since `earlier` (zero if `earlier` is later).
    pub fn saturating_since(self, earlier: Instant) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    /// `self + secs` seconds (saturating).
    pub const fn plus_secs(self, secs: u64) -> Instant {
        Instant(self.0.saturating_add(secs.saturating_mul(1000)))
    }

    /// `self + ms` milliseconds (saturating).
    pub const fn plus_millis(self, ms: u64) -> Instant {
        Instant(self.0.saturating_add(ms))
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        self.plus_millis(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03}s", self.0 / 1000, self.0 % 1000)
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// A user pool slot, 0-based (`SlotId(0)` is "pool 1", the highest priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u8);

impl SlotId {
    /// Index into the slot table.
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Every user slot, in priority order.
    pub fn all() -> impl Iterator<Item = SlotId> {
        (0..MAX_SLOTS).filter_map(|i| u8::try_from(i).ok()).map(SlotId)
    }

    fn from_index(i: usize) -> SlotId {
        SlotId(u8::try_from(i).unwrap_or(u8::MAX))
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pool {}", u16::from(self.0) + 1)
    }
}

/// Transport security of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TlsMode {
    /// TLS first; plain TCP only after a TLS *protocol* error (the server does not speak TLS).
    /// The result is cached per `host:port`.
    #[default]
    Auto,
    /// TLS only (also used for pinned self-signed pools; the daemon checks the pin).
    On,
    /// Plain TCP only.
    Off,
}

/// One user pool as configured in the GUI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolConfig {
    /// Host name or IP literal.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Transport security.
    pub tls: TlsMode,
    /// Disabled slots are never contacted.
    pub enabled: bool,
    /// Opaque login identity (e.g. `wallet.worker`). The reducer only compares it: any change
    /// forces a new session.
    pub login: String,
}

impl PoolConfig {
    /// An enabled pool with an empty login.
    pub fn new(host: impl Into<String>, port: u16, tls: TlsMode) -> Self {
        PoolConfig { host: host.into(), port, tls, enabled: true, login: String::new() }
    }
}

/// Failover thresholds. [`FailoverConfig::default`] holds the values of `docs/en/ARCHITECTURE.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct FailoverConfig {
    /// DNS resolution and TCP connect, each.
    pub connect_timeout_s: u64,
    /// TLS handshake, and the authorize reply.
    pub handshake_timeout_s: u64,
    /// First job after the authorize ack.
    pub first_job_timeout_s: u64,
    /// No job for this long on the active slot: reconnect once, then fail over.
    pub stall_soft_reconnect_s: u64,
    /// Consecutive invalid rejects that count as a reject storm.
    pub max_consecutive_invalid: u32,
    /// Share of rejects (stale and low-diff excluded) over `reject_window` that is a storm (`>`).
    pub reject_ratio_max: f64,
    /// Size of the reject-ratio window.
    pub reject_window: usize,
    /// Share of stale rejects over `stale_window` that triggers a failover (`>`).
    pub stale_ratio_max: f64,
    /// Size of the stale window.
    pub stale_window: usize,
    /// A submit without an ack for this long is an ack timeout.
    pub submit_ack_timeout_s: u64,
    /// Consecutive ack timeouts that fail the active slot.
    pub max_ack_timeouts: u32,
    /// Backoff schedule after consecutive failures (the last entry repeats).
    pub backoff_s: Vec<u64>,
    /// Symmetric jitter applied to every backoff, in percent.
    pub backoff_jitter_pct: u64,
    /// While a lower-priority slot is active, probe the best higher-priority slot this often.
    pub failback_probe_every_s: u64,
    /// A probed slot must stay healthy this long before the switch.
    pub failback_stable_s: u64,
    /// Retry period after an authorization error (config error).
    pub auth_retry_s: u64,
    /// Quarantine after ban text from a pool.
    pub quarantine_s: u64,
    /// Maximum drain of the old session after a planned switch.
    pub drain_s: u64,
    /// EOF/reset on a slot active for longer than this reconnects to the same pool once.
    pub reconnect_same_after_s: u64,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        FailoverConfig {
            connect_timeout_s: 10,
            handshake_timeout_s: 15,
            first_job_timeout_s: 30,
            stall_soft_reconnect_s: 900,
            max_consecutive_invalid: 5,
            reject_ratio_max: 0.5,
            reject_window: 20,
            stale_ratio_max: 0.02,
            stale_window: 100,
            submit_ack_timeout_s: 30,
            max_ack_timeouts: 3,
            backoff_s: vec![5, 10, 20, 40, 80, 120],
            backoff_jitter_pct: 20,
            failback_probe_every_s: 300,
            failback_stable_s: 60,
            auth_retry_s: 600,
            quarantine_s: 600,
            drain_s: 5,
            reconnect_same_after_s: 60,
        }
    }
}

impl FailoverConfig {
    /// Worst-case time for one connection attempt to either reach its first job or fail:
    /// resolve + connect + TLS handshake + plain fallback connect + authorize + first job.
    pub fn attempt_bound(&self) -> Duration {
        Duration::from_secs(
            3 * self.connect_timeout_s + 2 * self.handshake_timeout_s + self.first_job_timeout_s,
        )
    }

    /// Worst-case time from a failure of the active slot (or from start) until a new slot is
    /// active or the manager is `AllDown`, provided the daemon honours `ScheduleTick`.
    pub fn failover_bound(&self) -> Duration {
        self.attempt_bound() * MAX_SLOTS as u32
    }
}

// ---------------------------------------------------------------------------------------------
// States, events, actions
// ---------------------------------------------------------------------------------------------

/// Lifecycle of one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// Not configured or disabled by the user.
    Disabled,
    /// Configured, no session, eligible.
    Idle,
    /// DNS lookup in flight.
    Resolving,
    /// TCP connect in flight.
    Connecting,
    /// TLS handshake in flight.
    TlsHandshake,
    /// Authorize sent, waiting for the ack.
    Authorizing,
    /// Authorized, waiting for the first job.
    AwaitingJob,
    /// The slot the GPU mines for. At most one slot is `Active`.
    Active,
    /// A healthy probe session (connected, authorized, has jobs) that is not mined.
    Standby,
    /// Old session after a planned switch: in-flight hits are still submitted until `until`.
    Draining {
        /// When the session is closed.
        until: Instant,
    },
    /// Failed; not contacted before `until`.
    Backoff {
        /// End of the backoff.
        until: Instant,
        /// Consecutive failures so far (1 after the first failure).
        n: u32,
    },
    /// The pool rejected the credentials; retried at `retry_at`.
    ConfigError {
        /// The pool's message.
        msg: String,
        /// When the slot becomes eligible again.
        retry_at: Instant,
    },
    /// The pool sent ban text; not contacted before `until`.
    Quarantined {
        /// End of the quarantine.
        until: Instant,
    },
}

impl SlotState {
    /// A session (or a DNS lookup) exists for the slot.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            SlotState::Resolving
                | SlotState::Connecting
                | SlotState::TlsHandshake
                | SlotState::Authorizing
                | SlotState::AwaitingJob
                | SlotState::Active
                | SlotState::Standby
                | SlotState::Draining { .. }
        )
    }

    /// The slot is on its way up (resolve → first job).
    pub fn is_coming_up(&self) -> bool {
        matches!(
            self,
            SlotState::Resolving
                | SlotState::Connecting
                | SlotState::TlsHandshake
                | SlotState::Authorizing
                | SlotState::AwaitingJob
        )
    }
}

/// Why mining is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PauseReason {
    /// The user pressed Stop: every session is closed.
    UserStop,
    /// Another GPU user asked for the GPU (spark-modo / yield): sessions stay open.
    Yield,
    /// A pool announced a certificate version this build cannot mine: sessions closed.
    UnsupportedScheme,
    /// Invalid-share storms on two pools in a row: sessions closed, "update the miner".
    RejectEverywhere,
    /// The power/health governor asked for a pause: sessions stay open.
    Health,
}

impl PauseReason {
    /// Frozen pauses close every session and stop the failover policy until resumed.
    pub fn is_frozen(self) -> bool {
        matches!(
            self,
            PauseReason::UserStop | PauseReason::UnsupportedScheme | PauseReason::RejectEverywhere
        )
    }

    fn rank(self) -> u8 {
        match self {
            PauseReason::Yield => 0,
            PauseReason::Health => 1,
            PauseReason::RejectEverywhere => 2,
            PauseReason::UnsupportedScheme => 3,
            PauseReason::UserStop => 4,
        }
    }
}

/// Group-level state shown in the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerState {
    /// Bringing up the first slot.
    Starting,
    /// Mining on `active`.
    Mining {
        /// The active slot.
        active: SlotId,
    },
    /// `from` failed; `to` is being brought up (`from == to` for a same-pool reconnect).
    FailingOver {
        /// The slot that failed.
        from: SlotId,
        /// The slot being brought up.
        to: SlotId,
    },
    /// Every slot is unusable; the GPU is idle; round-robin retries honour backoff.
    AllDown {
        /// When the manager went all-down.
        since: Instant,
    },
    /// Mining is paused.
    Paused {
        /// Why.
        reason: PauseReason,
    },
}

/// How a pool rejected a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectKind {
    /// The proof did not verify.
    Invalid,
    /// The job was superseded (excluded from the reject-storm rules).
    Stale,
    /// The share did not meet the target (excluded from the reject-storm rules).
    LowDiff,
    /// Anything else (duplicate, unknown error); counts as a reject.
    Other,
}

/// Input of the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Timer fired (or a periodic heartbeat); deadlines are evaluated on every event anyway.
    Tick,
    /// DNS answer for the slot.
    Resolved { slot: SlotId },
    /// DNS failure.
    ResolveFailed { slot: SlotId },
    /// TCP connected (the daemon starts the TLS handshake itself when `tls` was requested).
    Connected { slot: SlotId },
    /// TCP refused, unreachable or reset during connect.
    ConnectFailed { slot: SlotId },
    /// TLS handshake completed.
    TlsOk { slot: SlotId },
    /// The server does not speak TLS (garbage record, plain-text reply, …).
    TlsProtocolError { slot: SlotId },
    /// Certificate, pin or other TLS failure — never falls back to plain.
    TlsOtherError { slot: SlotId },
    /// Authorize acknowledged.
    Authorized { slot: SlotId },
    /// Authorize rejected (bad wallet/worker): the slot becomes a config error.
    AuthRejected { slot: SlotId, msg: String },
    /// A `mining.notify`. `cert_version: None` means the pool omits it (treated
    /// as unsupported — an "update required" alert is raised, like for `>= 4`).
    JobReceived { slot: SlotId, job_id: String, cert_version: Option<u32> },
    /// A submit was accepted.
    ShareAccepted { slot: SlotId },
    /// A submit was rejected.
    ShareRejected { slot: SlotId, kind: RejectKind },
    /// The daemon detected a submit without ack. Optional: the reducer also times out pending
    /// submits itself; an event with no submit pending is ignored.
    SubmitAckTimeout { slot: SlotId },
    /// EOF or reset on an open session; `was_active_for_s` is how long it had been mining.
    Disconnected { slot: SlotId, was_active_for_s: u64 },
    /// The pool sent ban text.
    BanText { slot: SlotId },
    /// A daemon that runs probes as one step reports the probe reached its first job.
    ProbeOk { slot: SlotId },
    /// A daemon that runs probes as one step reports the probe failed.
    ProbeFailed { slot: SlotId },
    /// Manual switch from the GUI; also pins the slot until `UserPin { slot: None }`.
    UserSwitch { slot: SlotId },
    /// Pin a slot (manual override of automatic failback) or unpin with `None`.
    UserPin { slot: Option<SlotId> },
    /// New pool list (index = priority; at most [`MAX_SLOTS`]).
    ConfigChanged { pools: Vec<PoolConfig> },
    /// Pause mining.
    PauseRequest { reason: PauseReason },
    /// Resume from any pause.
    ResumeRequest,
    /// The GPU found a share for `job_id` of the session `slot` that produced the job.
    HitFound { slot: SlotId, job_id: String },
}

impl Event {
    /// `JobReceived` from a parsed `mining.notify`.
    pub fn job_received(slot: SlotId, job: &spm_proto::Job) -> Event {
        Event::JobReceived { slot, job_id: job.job_id.clone(), cert_version: job.cert_version }
    }

    /// The event for an id-matched submit reply (`None` for an unrelated reply).
    /// Rejections whose text looks like a ban become [`Event::BanText`].
    pub fn from_submit_reply(slot: SlotId, reply: &spm_proto::Reply) -> Option<Event> {
        match reply {
            spm_proto::Reply::Accepted => Some(Event::ShareAccepted { slot }),
            spm_proto::Reply::Rejected(msg) if is_ban_text(msg) => Some(Event::BanText { slot }),
            spm_proto::Reply::Rejected(msg) => {
                Some(Event::ShareRejected { slot, kind: classify_reject(msg) })
            }
            spm_proto::Reply::Unrelated => None,
        }
    }
}

/// Output of the reducer, executed in order by the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a DNS lookup for the slot's host (a new session begins here).
    Resolve { slot: SlotId },
    /// Connect to the resolved address; `tls` selects TLS or plain TCP.
    Connect { slot: SlotId, tls: bool },
    /// Close the slot's session (or cancel its lookup); no further events from it.
    Close { slot: SlotId },
    /// Send the dialect's authorize (and subscribe) messages.
    Authorize { slot: SlotId },
    /// Mine the jobs of this slot from the next attempt boundary on.
    SetActive { slot: SlotId },
    /// The GPU may hash (the active slot has a job and nothing is paused).
    StartGpu,
    /// Stop hashing (the worker keeps or releases its context per the arbiter's policy).
    StopGpu,
    /// Submit the hit on the slot's session (its current job is `job_id`).
    Submit { slot: SlotId, job_id: String },
    /// The hit's job is not the current job of an open session: drop it.
    DiscardStale { slot: SlotId, job_id: String },
    /// The next session of this slot is a probe: no submits, not fed to the GPU.
    Probe { slot: SlotId },
    /// User-visible alert (GUI notification + log).
    Alert { msg: String },
    /// Arm the single timer: deliver `Event::Tick` at `at` (replaces any previous schedule).
    ScheduleTick { at: Instant },
    /// Diagnostic log line.
    Log { msg: String },
}

// ---------------------------------------------------------------------------------------------
// Pool text classification helpers (for the daemon)
// ---------------------------------------------------------------------------------------------

/// Classify a pool's reject message. Unknown texts are [`RejectKind::Other`].
pub fn classify_reject(msg: &str) -> RejectKind {
    let m = msg.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    if any(&["low difficulty", "low diff", "lowdiff", "above target", "difficulty too low", "high-hash", "high hash", "does not meet"]) {
        RejectKind::LowDiff
    } else if any(&["stale", "job not found", "unknown job", "invalid job", "old job", "expired", "outdated", "obsolete"]) {
        RejectKind::Stale
    } else if any(&["invalid", "bad proof", "verif", "malformed", "mismatch", "bad share"]) {
        RejectKind::Invalid
    } else {
        RejectKind::Other
    }
}

/// Does a pool message announce a ban / block of this miner?
pub fn is_ban_text(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| matches!(w, "ban" | "banned" | "blacklist" | "blacklisted" | "blocked"))
}

// ---------------------------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct ShareStats {
    consecutive_bad: u32,
    /// Last `reject_window` outcomes excluding stale and low-diff; `true` = rejected.
    verdicts: VecDeque<bool>,
    /// Last `stale_window` outcomes; `true` = stale.
    staleness: VecDeque<bool>,
    ack_timeouts_in_row: u32,
    accepted: u32,
    /// Ack deadlines of in-flight submits, oldest first (acks arrive in order per session).
    pending: VecDeque<Instant>,
}

fn push_window(w: &mut VecDeque<bool>, cap: usize, v: bool) {
    if cap == 0 {
        return;
    }
    w.push_back(v);
    while w.len() > cap {
        w.pop_front();
    }
}

fn ratio_exceeded(w: &VecDeque<bool>, cap: usize, max: f64) -> bool {
    if cap == 0 || w.len() < cap {
        return false;
    }
    let hits = w.iter().filter(|&&b| b).count();
    hits as f64 > max * w.len() as f64
}

impl ShareStats {
    fn reject_storm(&self, cfg: &FailoverConfig) -> bool {
        self.consecutive_bad >= cfg.max_consecutive_invalid.max(1)
            || ratio_exceeded(&self.verdicts, cfg.reject_window, cfg.reject_ratio_max)
    }

    fn stale_excess(&self, cfg: &FailoverConfig) -> bool {
        ratio_exceeded(&self.staleness, cfg.stale_window, cfg.stale_ratio_max)
    }
}

#[derive(Debug, Clone)]
struct Slot {
    cfg: Option<PoolConfig>,
    state: SlotState,
    entered: Instant,
    job_id: Option<String>,
    last_job_at: Instant,
    jobs_since_active: u32,
    tls_now: bool,
    backoff_n: u32,
    reconnect_used: bool,
    stall_reconnect_used: bool,
    unsupported: bool,
    stats: ShareStats,
}

impl Slot {
    fn new(cfg: Option<PoolConfig>) -> Slot {
        let enabled = cfg.as_ref().is_some_and(|c| c.enabled);
        Slot {
            cfg,
            state: if enabled { SlotState::Idle } else { SlotState::Disabled },
            entered: Instant::ZERO,
            job_id: None,
            last_job_at: Instant::ZERO,
            jobs_since_active: 0,
            tls_now: true,
            backoff_n: 0,
            reconnect_used: false,
            stall_reconnect_used: false,
            unsupported: false,
            stats: ShareStats::default(),
        }
    }

    fn enabled(&self) -> bool {
        self.cfg.as_ref().is_some_and(|c| c.enabled)
    }

    fn set(&mut self, state: SlotState, now: Instant) {
        self.state = state;
        self.entered = now;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Normal,
    FailingOver { from: SlotId },
    AllDown { since: Instant },
}

/// Why a probe session exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    /// Automatic failback to a better slot (switch after `failback_stable_s`).
    Failback,
    /// Manual switch / pin (switch as soon as the slot has a job).
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeRun {
    slot: SlotId,
    kind: ProbeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    Resolve,
    Connect,
    Tls(&'static str),
    Timeout(&'static str),
    Auth(String),
    Eof,
    Stall,
    RejectStorm,
    Stale,
    AckTimeouts,
    Ban,
    Probe,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Resolve => f.write_str("DNS resolution failed"),
            Failure::Connect => f.write_str("connection refused or unreachable"),
            Failure::Tls(what) => write!(f, "TLS {what}"),
            Failure::Timeout(stage) => write!(f, "timeout while {stage}"),
            Failure::Auth(msg) => write!(f, "authorization rejected: {msg}"),
            Failure::Eof => f.write_str("connection lost"),
            Failure::Stall => f.write_str("no job received (stall) after a reconnect"),
            Failure::RejectStorm => f.write_str("invalid-share storm"),
            Failure::Stale => f.write_str("stale shares above the limit"),
            Failure::AckTimeouts => f.write_str("submits not acknowledged"),
            Failure::Ban => f.write_str("banned by the pool"),
            Failure::Probe => f.write_str("probe failed"),
        }
    }
}

/// The failover manager's state. Create it with [`State::new`] and drive it with [`step`].
#[derive(Debug, Clone)]
pub struct State {
    cfg: FailoverConfig,
    slots: Vec<Slot>,
    manager: ManagerState,
    phase: Phase,
    target: Option<SlotId>,
    probe: Option<ProbeRun>,
    tried: [bool; MAX_SLOTS],
    rr_cursor: usize,
    pin: Option<SlotId>,
    pause: Option<PauseReason>,
    gpu_on: bool,
    next_probe_at: Option<Instant>,
    reject_chain: Option<SlotId>,
    tls_cache: BTreeMap<(String, u16), bool>,
    rng: u64,
    last_now: Instant,
    scheduled: Option<Instant>,
    episode_start: Option<Instant>,
}

/// Advance the failover manager by one event at time `now` and return the actions to execute.
///
/// `now` must come from a monotonic clock; a value earlier than a previous call is treated as
/// the previous value.
pub fn step(state: &mut State, event: Event, now: Instant) -> Vec<Action> {
    let mut out = Vec::new();
    state.step_inner(event, now, &mut out);
    out
}

impl State {
    /// A manager in `Starting` for `pools` (index = priority; entries beyond [`MAX_SLOTS`] are
    /// ignored). `seed` makes the backoff jitter deterministic. Nothing happens until the first
    /// [`step`] (typically `Event::Tick`).
    pub fn new(cfg: FailoverConfig, pools: Vec<PoolConfig>, seed: u64) -> State {
        let slots = (0..MAX_SLOTS).map(|i| Slot::new(pools.get(i).cloned())).collect();
        State {
            cfg,
            slots,
            manager: ManagerState::Starting,
            phase: Phase::Starting,
            target: None,
            probe: None,
            tried: [false; MAX_SLOTS],
            rr_cursor: 0,
            pin: None,
            pause: None,
            gpu_on: false,
            next_probe_at: None,
            reject_chain: None,
            tls_cache: BTreeMap::new(),
            rng: seed,
            last_now: Instant::ZERO,
            scheduled: None,
            episode_start: None,
        }
    }

    // ----- read-only views -----

    /// The thresholds in use.
    pub fn config(&self) -> &FailoverConfig {
        &self.cfg
    }

    /// Group-level state.
    pub fn manager(&self) -> ManagerState {
        self.manager
    }

    /// State of one slot (`None` for an out-of-range id).
    pub fn slot_state(&self, slot: SlotId) -> Option<&SlotState> {
        self.slots.get(slot.index()).map(|s| &s.state)
    }

    /// Configuration of one slot.
    pub fn slot_config(&self, slot: SlotId) -> Option<&PoolConfig> {
        self.slots.get(slot.index()).and_then(|s| s.cfg.as_ref())
    }

    /// Current job of the slot's session, if any.
    pub fn current_job(&self, slot: SlotId) -> Option<&str> {
        self.slots.get(slot.index()).and_then(|s| s.job_id.as_deref())
    }

    /// The slot in [`SlotState::Active`], if any.
    pub fn active(&self) -> Option<SlotId> {
        self.slots.iter().position(|s| s.state == SlotState::Active).map(SlotId::from_index)
    }

    /// Whether the reducer last told the GPU to hash.
    pub fn gpu_running(&self) -> bool {
        self.gpu_on
    }

    /// Current pause, if any.
    pub fn pause_reason(&self) -> Option<PauseReason> {
        self.pause
    }

    /// Pinned slot (manual override), if any.
    pub fn pin(&self) -> Option<SlotId> {
        self.pin
    }

    /// Slot currently being probed, and why.
    pub fn probing(&self) -> Option<(SlotId, ProbeKind)> {
        self.probe.map(|p| (p.slot, p.kind))
    }

    /// Start of the current failover (or start-up) episode; `None` while mining or all-down.
    pub fn failover_started(&self) -> Option<Instant> {
        self.episode_start
    }

    /// Consecutive failures of the slot (drives the backoff schedule).
    pub fn failures(&self, slot: SlotId) -> u32 {
        self.slots.get(slot.index()).map_or(0, |s| s.backoff_n)
    }

    /// Submits of the slot still waiting for an ack.
    pub fn pending_acks(&self, slot: SlotId) -> usize {
        self.slots.get(slot.index()).map_or(0, |s| s.stats.pending.len())
    }

    /// The slot announced an unsupported certificate version.
    pub fn is_unsupported(&self, slot: SlotId) -> bool {
        self.slots.get(slot.index()).is_some_and(|s| s.unsupported)
    }

    /// Learned transport for an `Auto` endpoint (`Some(true)` = TLS, `Some(false)` = plain).
    pub fn tls_cached(&self, host: &str, port: u16) -> Option<bool> {
        self.tls_cache.get(&(host.to_string(), port)).copied()
    }

    /// Earliest instant at which the reducer needs a `Tick`.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.scheduled
    }

    // ----- driver -----

    fn step_inner(&mut self, event: Event, now: Instant, out: &mut Vec<Action>) {
        let now = now.max(self.last_now);
        self.last_now = now;
        if self.scheduled.is_some_and(|t| t <= now) {
            self.scheduled = None;
        }
        self.expire(now, out);
        self.handle(event, now, out);
        self.reconcile(now, out);
    }

    fn idx(&self, slot: SlotId) -> Option<usize> {
        (slot.index() < self.slots.len()).then_some(slot.index())
    }

    fn slot_in(&self, slot: SlotId, pred: impl Fn(&SlotState) -> bool) -> Option<usize> {
        self.idx(slot).filter(|&i| pred(&self.slots[i].state))
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn next_rand(&mut self) -> u64 {
        // SplitMix64: tiny, deterministic, good enough for jitter.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Backoff after `failures_before` consecutive failures, with ±jitter.
    fn backoff_delay(&mut self, failures_before: u32) -> Duration {
        let sched = &self.cfg.backoff_s;
        let base_s = sched
            .get(failures_before as usize)
            .or_else(|| sched.last())
            .copied()
            .unwrap_or(5);
        let base_ms = base_s.saturating_mul(1000);
        let span = base_ms.saturating_mul(self.cfg.backoff_jitter_pct.min(100)) / 100;
        let r = self.next_rand();
        let offset = if span == 0 { 0 } else { r % (2 * span + 1) };
        Duration::from_millis(base_ms - span + offset)
    }

    fn usable(&self, i: usize) -> bool {
        let s = &self.slots[i];
        if !s.enabled() || s.unsupported {
            return false;
        }
        match s.state {
            SlotState::Idle | SlotState::Standby => true,
            SlotState::Draining { .. } => s.job_id.is_some(),
            ref st => st.is_coming_up(),
        }
    }

    fn start_candidate(&self) -> Option<SlotId> {
        if let Some(p) = self.pin {
            if self.idx(p).is_some_and(|i| self.usable(i)) {
                return Some(p);
            }
        }
        (0..MAX_SLOTS).find(|&i| self.usable(i)).map(SlotId::from_index)
    }

    fn next_after(&self, from: SlotId) -> Option<SlotId> {
        (1..=MAX_SLOTS)
            .map(|k| (from.index() + k) % MAX_SLOTS)
            .find(|&j| self.usable(j) && !self.tried[j])
            .map(SlotId::from_index)
    }

    fn round_robin(&self) -> Option<SlotId> {
        (0..MAX_SLOTS)
            .map(|k| (self.rr_cursor + k) % MAX_SLOTS)
            .find(|&j| self.usable(j))
            .map(SlotId::from_index)
    }

    /// Could a better slot than `active` exist (so failback checks are worth scheduling)?
    fn wants_failback(&self, active: SlotId) -> bool {
        match self.pin {
            Some(p) => p != active,
            None => active.index() > 0,
        }
    }

    fn failback_candidate(&self, active: SlotId) -> Option<SlotId> {
        match self.pin {
            Some(p) => (p != active && self.idx(p).is_some_and(|i| self.usable(i))).then_some(p),
            None => (0..active.index()).find(|&j| self.usable(j)).map(SlotId::from_index),
        }
    }

    fn decide_tls(&self, i: usize) -> bool {
        match &self.slots[i].cfg {
            Some(c) => match c.tls {
                TlsMode::On => true,
                TlsMode::Off => false,
                TlsMode::Auto => {
                    self.tls_cache.get(&(c.host.clone(), c.port)).copied().unwrap_or(true)
                }
            },
            None => true,
        }
    }

    fn remember_tls(&mut self, i: usize) {
        let s = &self.slots[i];
        if let Some(c) = &s.cfg {
            if c.tls == TlsMode::Auto {
                self.tls_cache.insert((c.host.clone(), c.port), s.tls_now);
            }
        }
    }

    // ----- deadlines -----

    fn expire(&mut self, now: Instant, out: &mut Vec<Action>) {
        for i in 0..MAX_SLOTS {
            let s = &self.slots[i];
            let since_entered = now.saturating_since(s.entered);
            let connect = Self::secs(self.cfg.connect_timeout_s);
            let handshake = Self::secs(self.cfg.handshake_timeout_s);
            match s.state.clone() {
                SlotState::Resolving if since_entered >= connect => {
                    self.fail(i, Failure::Timeout("resolving"), now, out)
                }
                SlotState::Connecting if since_entered >= connect => {
                    self.fail(i, Failure::Timeout("connecting"), now, out)
                }
                SlotState::TlsHandshake if since_entered >= handshake => {
                    self.fail(i, Failure::Timeout("in the TLS handshake"), now, out)
                }
                SlotState::Authorizing if since_entered >= handshake => {
                    self.fail(i, Failure::Timeout("authorizing"), now, out)
                }
                SlotState::AwaitingJob
                    if since_entered >= Self::secs(self.cfg.first_job_timeout_s) =>
                {
                    self.fail(i, Failure::Timeout("waiting for the first job"), now, out)
                }
                SlotState::Active => self.expire_active(i, now, out),
                SlotState::Draining { until } if now >= until => {
                    self.close_session(i, out);
                    self.slots[i].set(SlotState::Idle, now);
                    out.push(Action::Log { msg: format!("{}: drained and closed", SlotId::from_index(i)) });
                }
                SlotState::Backoff { until, .. } | SlotState::Quarantined { until }
                    if now >= until =>
                {
                    self.slots[i].set(SlotState::Idle, now);
                }
                SlotState::ConfigError { retry_at, .. } if now >= retry_at => {
                    self.slots[i].set(SlotState::Idle, now);
                }
                _ => {}
            }
        }
    }

    fn expire_active(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        let stable = Self::secs(self.cfg.failback_stable_s.max(self.cfg.reconnect_same_after_s));
        {
            let s = &mut self.slots[i];
            if now.saturating_since(s.entered) >= stable {
                s.backoff_n = 0;
                s.reconnect_used = false;
            }
        }
        while let Some(&deadline) = self.slots[i].stats.pending.front() {
            if deadline > now {
                break;
            }
            self.slots[i].stats.pending.pop_front();
            if self.on_ack_timeout(i, now, out) {
                return;
            }
        }
        let stall = Self::secs(self.cfg.stall_soft_reconnect_s);
        if now.saturating_since(self.slots[i].last_job_at) >= stall {
            if self.slots[i].stall_reconnect_used {
                self.fail(i, Failure::Stall, now, out);
            } else {
                self.slots[i].stall_reconnect_used = true;
                self.soft_reconnect(i, "no job for the stall period", now, out);
            }
        }
    }

    // ----- event handling -----

    fn handle(&mut self, event: Event, now: Instant, out: &mut Vec<Action>) {
        match event {
            Event::Tick => {}
            Event::Resolved { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::Resolving) {
                    let tls = self.decide_tls(i);
                    let s = &mut self.slots[i];
                    s.tls_now = tls;
                    s.set(SlotState::Connecting, now);
                    out.push(Action::Connect { slot, tls });
                }
            }
            Event::ResolveFailed { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::Resolving) {
                    self.fail(i, Failure::Resolve, now, out);
                }
            }
            Event::Connected { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::Connecting) {
                    let s = &mut self.slots[i];
                    if s.tls_now {
                        s.set(SlotState::TlsHandshake, now);
                    } else {
                        s.set(SlotState::Authorizing, now);
                        out.push(Action::Authorize { slot });
                    }
                }
            }
            Event::ConnectFailed { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::Connecting) {
                    self.fail(i, Failure::Connect, now, out);
                }
            }
            Event::TlsOk { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::TlsHandshake) {
                    self.slots[i].set(SlotState::Authorizing, now);
                    out.push(Action::Authorize { slot });
                }
            }
            Event::TlsProtocolError { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::TlsHandshake) {
                    let auto = self.slots[i].cfg.as_ref().is_some_and(|c| c.tls == TlsMode::Auto);
                    if auto && self.slots[i].tls_now {
                        let s = &mut self.slots[i];
                        s.tls_now = false;
                        s.set(SlotState::Connecting, now);
                        out.push(Action::Close { slot });
                        out.push(Action::Connect { slot, tls: false });
                        out.push(Action::Log {
                            msg: format!("{slot}: server does not speak TLS, retrying plain TCP"),
                        });
                    } else {
                        self.fail(i, Failure::Tls("protocol error"), now, out);
                    }
                }
            }
            Event::TlsOtherError { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::TlsHandshake) {
                    self.fail(i, Failure::Tls("certificate/handshake error"), now, out);
                }
            }
            Event::Authorized { slot } => {
                if let Some(i) = self.slot_in(slot, |s| *s == SlotState::Authorizing) {
                    self.remember_tls(i);
                    self.slots[i].set(SlotState::AwaitingJob, now);
                }
            }
            Event::AuthRejected { slot, msg } => {
                if let Some(i) = self.slot_in(slot, |s| {
                    matches!(s, SlotState::Authorizing | SlotState::AwaitingJob)
                }) {
                    self.fail(i, Failure::Auth(msg), now, out);
                }
            }
            Event::JobReceived { slot, job_id, cert_version } => {
                if let Some(i) = self.idx(slot) {
                    self.on_job(i, job_id, cert_version, now, out);
                }
            }
            Event::ShareAccepted { slot } => {
                if let Some(i) = self.idx(slot) {
                    self.on_share(i, None, now, out);
                }
            }
            Event::ShareRejected { slot, kind } => {
                if let Some(i) = self.idx(slot) {
                    self.on_share(i, Some(kind), now, out);
                }
            }
            Event::SubmitAckTimeout { slot } => {
                if let Some(i) = self.slot_in(slot, |s| {
                    matches!(s, SlotState::Active | SlotState::Draining { .. })
                }) {
                    if self.slots[i].stats.pending.pop_front().is_some()
                        && self.slots[i].state == SlotState::Active
                    {
                        self.on_ack_timeout(i, now, out);
                    }
                }
            }
            Event::Disconnected { slot, was_active_for_s } => {
                if let Some(i) = self.idx(slot) {
                    self.on_disconnected(i, was_active_for_s, now, out);
                }
            }
            Event::BanText { slot } => {
                if let Some(i) = self.slot_in(slot, SlotState::is_open) {
                    self.fail(i, Failure::Ban, now, out);
                }
            }
            Event::ProbeOk { slot } => {
                let is_probe = self.probe.is_some_and(|p| p.slot == slot);
                if let Some(i) = self.slot_in(slot, SlotState::is_coming_up) {
                    if is_probe {
                        self.slots[i].set(SlotState::Standby, now);
                    }
                }
            }
            Event::ProbeFailed { slot } => {
                let related =
                    self.probe.is_some_and(|p| p.slot == slot) || self.target == Some(slot);
                if let Some(i) = self.slot_in(slot, |s| {
                    s.is_coming_up() || *s == SlotState::Standby
                }) {
                    if related {
                        self.fail(i, Failure::Probe, now, out);
                    }
                }
            }
            Event::UserSwitch { slot } | Event::UserPin { slot: Some(slot) } => {
                self.on_user_pin(slot, now, out)
            }
            Event::UserPin { slot: None } => {
                if self.pin.take().is_some() {
                    self.next_probe_at = Some(now.plus_secs(self.cfg.failback_probe_every_s));
                    out.push(Action::Log { msg: "unpinned: automatic failback re-enabled".into() });
                }
            }
            Event::ConfigChanged { pools } => self.on_config(pools, now, out),
            Event::PauseRequest { reason } => self.enter_pause(reason, now, out),
            Event::ResumeRequest => self.on_resume(now, out),
            Event::HitFound { slot, job_id } => self.on_hit(slot, job_id, now, out),
        }
    }

    fn on_job(
        &mut self,
        i: usize,
        job_id: String,
        cert_version: Option<u32>,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        let st = self.slots[i].state.clone();
        let accepts_jobs = matches!(
            st,
            SlotState::Authorizing
                | SlotState::AwaitingJob
                | SlotState::Active
                | SlotState::Standby
                | SlotState::Draining { .. }
        );
        if !accepts_jobs {
            return;
        }
        if cert_version != Some(SUPPORTED_CERT_VERSION) {
            self.on_unsupported(i, cert_version, now, out);
            return;
        }
        if st == SlotState::Authorizing {
            // Some pools push the first job before the authorize ack: implicit authorization.
            self.remember_tls(i);
        }
        {
            let s = &mut self.slots[i];
            s.job_id = Some(job_id);
            s.last_job_at = now;
            s.jobs_since_active = s.jobs_since_active.saturating_add(1);
            if s.jobs_since_active >= 2 {
                s.stall_reconnect_used = false;
            }
        }
        if matches!(st, SlotState::Authorizing | SlotState::AwaitingJob) {
            let id = SlotId::from_index(i);
            if self.target == Some(id) {
                self.promote(i, now, out);
            } else if self.probe.is_some_and(|p| p.slot == id) {
                self.slots[i].set(SlotState::Standby, now);
                out.push(Action::Log { msg: format!("{id}: probe healthy (standby)") });
            } else {
                // A session nobody wants any more (defensive; roles are cleared with Close).
                self.close_session(i, out);
                self.slots[i].set(SlotState::Idle, now);
            }
        }
    }

    fn on_unsupported(
        &mut self,
        i: usize,
        cert_version: Option<u32>,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        let id = SlotId::from_index(i);
        let critical = self.slots[i].state == SlotState::Active || self.target == Some(id);
        let v = cert_version.map_or_else(|| "unknown".to_string(), |v| v.to_string());
        out.push(Action::Alert { msg: format!("{id}: {UPDATE_REQUIRED_ALERT} (cert_version {v})") });
        self.close_session(i, out);
        self.slots[i].set(SlotState::Idle, now);
        self.slots[i].unsupported = true;
        if self.probe.is_some_and(|p| p.slot == id) {
            self.probe = None;
        }
        if self.target == Some(id) {
            self.target = None;
        }
        if critical {
            self.enter_pause(PauseReason::UnsupportedScheme, now, out);
        }
    }

    fn on_share(&mut self, i: usize, kind: Option<RejectKind>, now: Instant, out: &mut Vec<Action>) {
        let st = self.slots[i].state.clone();
        if !matches!(st, SlotState::Active | SlotState::Draining { .. }) {
            return;
        }
        let (rw, sw) = (self.cfg.reject_window, self.cfg.stale_window);
        let id = SlotId::from_index(i);
        {
            let stats = &mut self.slots[i].stats;
            stats.pending.pop_front();
            stats.ack_timeouts_in_row = 0;
            match kind {
                None => {
                    stats.consecutive_bad = 0;
                    stats.accepted = stats.accepted.saturating_add(1);
                    push_window(&mut stats.verdicts, rw, false);
                    push_window(&mut stats.staleness, sw, false);
                }
                Some(RejectKind::Invalid | RejectKind::Other) => {
                    stats.consecutive_bad = stats.consecutive_bad.saturating_add(1);
                    push_window(&mut stats.verdicts, rw, true);
                    push_window(&mut stats.staleness, sw, false);
                }
                Some(RejectKind::Stale) => push_window(&mut stats.staleness, sw, true),
                Some(RejectKind::LowDiff) => push_window(&mut stats.staleness, sw, false),
            }
        }
        if kind == Some(RejectKind::LowDiff) {
            out.push(Action::Log {
                msg: format!("{id}: low-difficulty reject (excluded from the failover rules)"),
            });
        }
        if st != SlotState::Active {
            return;
        }
        if kind.is_none()
            && self.reject_chain.is_some()
            && self.slots[i].stats.accepted as usize >= self.cfg.reject_window.max(1)
        {
            self.reject_chain = None;
        }
        if self.slots[i].stats.reject_storm(&self.cfg) {
            if self.reject_chain.is_some() {
                self.reject_everywhere(i, now, out);
            } else {
                self.reject_chain = Some(id);
                self.fail(i, Failure::RejectStorm, now, out);
            }
        } else if self.slots[i].stats.stale_excess(&self.cfg) {
            self.fail(i, Failure::Stale, now, out);
        }
    }

    /// Returns `true` when the slot failed.
    fn on_ack_timeout(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) -> bool {
        let stats = &mut self.slots[i].stats;
        stats.ack_timeouts_in_row = stats.ack_timeouts_in_row.saturating_add(1);
        if stats.ack_timeouts_in_row >= self.cfg.max_ack_timeouts.max(1) {
            self.fail(i, Failure::AckTimeouts, now, out);
            true
        } else {
            out.push(Action::Log {
                msg: format!("{}: submit ack timeout", SlotId::from_index(i)),
            });
            false
        }
    }

    fn on_disconnected(&mut self, i: usize, was_active_for_s: u64, now: Instant, out: &mut Vec<Action>) {
        match self.slots[i].state {
            SlotState::Active => {
                if was_active_for_s > self.cfg.reconnect_same_after_s && !self.slots[i].reconnect_used {
                    self.slots[i].reconnect_used = true;
                    self.soft_reconnect(i, "connection lost after a stable session", now, out);
                } else {
                    self.fail(i, Failure::Eof, now, out);
                }
            }
            SlotState::Draining { .. } => {
                self.close_session(i, out);
                self.slots[i].set(SlotState::Idle, now);
            }
            ref s if s.is_coming_up() || *s == SlotState::Standby => {
                self.fail(i, Failure::Eof, now, out)
            }
            _ => {}
        }
    }

    fn on_hit(&mut self, slot: SlotId, job_id: String, now: Instant, out: &mut Vec<Action>) {
        let current = self.idx(slot).filter(|&i| {
            let s = &self.slots[i];
            matches!(s.state, SlotState::Active | SlotState::Draining { .. })
                && s.job_id.as_deref() == Some(job_id.as_str())
        });
        match current {
            Some(i) => {
                let deadline = now.plus_secs(self.cfg.submit_ack_timeout_s);
                self.slots[i].stats.pending.push_back(deadline);
                out.push(Action::Submit { slot, job_id });
            }
            None => {
                out.push(Action::DiscardStale { slot, job_id });
            }
        }
    }

    fn on_user_pin(&mut self, slot: SlotId, now: Instant, out: &mut Vec<Action>) {
        let Some(i) = self.idx(slot).filter(|&i| self.slots[i].enabled()) else {
            out.push(Action::Log { msg: format!("{slot}: cannot switch to a disabled slot") });
            return;
        };
        self.pin = Some(slot);
        out.push(Action::Log { msg: format!("{slot}: pinned (manual override until unpinned)") });
        if self.pause.is_some_and(PauseReason::is_frozen) {
            return; // applied when mining resumes
        }
        if matches!(self.slots[i].state, SlotState::Backoff { .. } | SlotState::ConfigError { .. }) {
            // The user asked for this pool explicitly: skip the remaining backoff.
            self.slots[i].set(SlotState::Idle, now);
        }
        if !self.usable(i) && self.slots[i].state != SlotState::Active {
            out.push(Action::Log {
                msg: format!("{slot}: not usable right now; switching when it becomes available"),
            });
            self.next_probe_at = Some(now);
            return;
        }
        match self.active() {
            Some(a) if a == slot => {}
            Some(_) => {
                if let Some(p) = self.probe {
                    if p.slot != slot {
                        self.cancel(p.slot.index(), now, out);
                    }
                }
                let st = self.slots[i].state.clone();
                let has_job = self.slots[i].job_id.is_some();
                match st {
                    SlotState::Standby | SlotState::Draining { .. } if has_job => {
                        self.promote(i, now, out)
                    }
                    SlotState::Idle => self.start_probe(i, ProbeKind::Manual, now, out),
                    _ => self.probe = Some(ProbeRun { slot, kind: ProbeKind::Manual }),
                }
            }
            None => {
                if let Some(t) = self.target {
                    if t != slot {
                        self.cancel(t.index(), now, out);
                    }
                }
                if let Some(p) = self.probe {
                    if p.slot != slot {
                        self.cancel(p.slot.index(), now, out);
                    }
                }
                self.phase = Phase::Starting;
                self.tried = [false; MAX_SLOTS];
                self.episode_start = Some(now);
            }
        }
    }

    fn on_config(&mut self, pools: Vec<PoolConfig>, now: Instant, out: &mut Vec<Action>) {
        if pools.len() > MAX_SLOTS {
            out.push(Action::Log {
                msg: format!("{} pools configured; only the first {MAX_SLOTS} are used", pools.len()),
            });
        }
        let mut active_changed = false;
        let mut changed = false;
        for i in 0..MAX_SLOTS {
            let new = pools.get(i).cloned();
            if new == self.slots[i].cfg {
                continue;
            }
            changed = true;
            let id = SlotId::from_index(i);
            active_changed |= self.slots[i].state == SlotState::Active;
            self.close_session(i, out);
            if self.target == Some(id) {
                self.target = None;
            }
            if self.probe.is_some_and(|p| p.slot == id) {
                self.probe = None;
            }
            let fresh = Slot::new(new);
            if self.pin == Some(id) && !fresh.enabled() {
                self.pin = None;
            }
            let state = fresh.state.clone();
            self.slots[i] = fresh;
            self.slots[i].set(state, now);
        }
        if !changed {
            return;
        }
        out.push(Action::Log { msg: "pool configuration applied".into() });
        if self.pause.is_some_and(PauseReason::is_frozen) {
            return;
        }
        if active_changed || self.active().is_none() {
            // Restart the selection from the top: the user's edit takes effect now.
            let best = self.start_candidate();
            if let Some(t) = self.target {
                if Some(t) != best {
                    self.cancel(t.index(), now, out);
                }
            }
            self.phase = Phase::Starting;
            self.tried = [false; MAX_SLOTS];
            self.episode_start = Some(now);
        } else {
            self.next_probe_at = Some(now);
        }
    }

    fn enter_pause(&mut self, reason: PauseReason, now: Instant, out: &mut Vec<Action>) {
        if let Some(cur) = self.pause {
            if reason.rank() < cur.rank() {
                out.push(Action::Log { msg: format!("pause {reason:?} ignored while {cur:?}") });
                return;
            }
        }
        self.pause = Some(reason);
        out.push(Action::Log { msg: format!("paused: {reason:?}") });
        if reason.is_frozen() {
            for i in 0..MAX_SLOTS {
                if self.slots[i].state.is_open() {
                    self.close_session(i, out);
                    self.slots[i].set(SlotState::Idle, now);
                }
            }
            self.target = None;
            self.probe = None;
            self.phase = Phase::Starting;
            self.tried = [false; MAX_SLOTS];
            self.episode_start = None;
            self.next_probe_at = None;
        }
    }

    fn on_resume(&mut self, _now: Instant, out: &mut Vec<Action>) {
        let Some(reason) = self.pause.take() else { return };
        if reason.is_frozen() {
            for s in &mut self.slots {
                s.unsupported = false;
            }
            self.reject_chain = None;
            self.phase = Phase::Starting;
            self.episode_start = None;
        }
        out.push(Action::Log { msg: format!("resumed from {reason:?}") });
    }

    // ----- transitions -----

    fn close_session(&mut self, i: usize, out: &mut Vec<Action>) {
        let s = &mut self.slots[i];
        if s.state.is_open() {
            out.push(Action::Close { slot: SlotId::from_index(i) });
        }
        s.job_id = None;
        s.stats.pending.clear();
        s.jobs_since_active = 0;
    }

    /// Close a session nobody needs any more, without penalty.
    fn cancel(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        let id = SlotId::from_index(i);
        if self.slots[i].state.is_open() && self.slots[i].state != SlotState::Active {
            self.close_session(i, out);
            self.slots[i].set(SlotState::Idle, now);
        }
        if self.target == Some(id) {
            self.target = None;
        }
        if self.probe.is_some_and(|p| p.slot == id) {
            self.probe = None;
        }
    }

    fn penalize(&mut self, i: usize, failure: &Failure, now: Instant, out: &mut Vec<Action>) {
        self.close_session(i, out);
        let state = match failure {
            Failure::Auth(msg) => SlotState::ConfigError {
                msg: msg.clone(),
                retry_at: now.plus_secs(self.cfg.auth_retry_s),
            },
            Failure::Ban => SlotState::Quarantined { until: now.plus_secs(self.cfg.quarantine_s) },
            _ => {
                let n = self.slots[i].backoff_n;
                let delay = self.backoff_delay(n);
                let n = n.saturating_add(1);
                self.slots[i].backoff_n = n;
                SlotState::Backoff { until: now + delay, n }
            }
        };
        self.slots[i].set(state, now);
    }

    fn fail(&mut self, i: usize, failure: Failure, now: Instant, out: &mut Vec<Action>) {
        let id = SlotId::from_index(i);
        let was_active = self.slots[i].state == SlotState::Active;
        let was_target = self.target == Some(id);
        self.penalize(i, &failure, now, out);
        let msg = match &self.slots[i].state {
            SlotState::Backoff { until, n } => format!("{id}: {failure}; backoff #{n} until {until}"),
            SlotState::ConfigError { retry_at, .. } => {
                format!("{id}: {failure} – check wallet/worker; retrying at {retry_at}")
            }
            SlotState::Quarantined { until } => format!("{id}: {failure}; quarantined until {until}"),
            _ => format!("{id}: {failure}"),
        };
        if was_active || matches!(failure, Failure::Auth(_) | Failure::Ban) {
            out.push(Action::Alert { msg });
        } else {
            out.push(Action::Log { msg });
        }
        if self.probe.is_some_and(|p| p.slot == id) {
            self.probe = None;
        }
        if was_active {
            self.tried = [false; MAX_SLOTS];
            self.tried[i] = true;
            self.episode_start = Some(now);
            self.target = None;
            self.phase = Phase::FailingOver { from: id };
        } else if was_target {
            self.tried[i] = true;
            self.target = None;
            match self.phase {
                Phase::AllDown { .. } => self.rr_cursor = (i + 1) % MAX_SLOTS,
                _ => self.phase = Phase::FailingOver { from: id },
            }
        }
    }

    fn soft_reconnect(&mut self, i: usize, why: &str, now: Instant, out: &mut Vec<Action>) {
        let id = SlotId::from_index(i);
        self.close_session(i, out);
        self.slots[i].set(SlotState::Idle, now);
        self.target = Some(id);
        self.phase = Phase::FailingOver { from: id };
        self.tried = [false; MAX_SLOTS];
        self.episode_start = Some(now);
        out.push(Action::Log { msg: format!("{id}: {why}; reconnecting to the same pool") });
    }

    fn reject_everywhere(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        self.penalize(i, &Failure::RejectStorm, now, out);
        out.push(Action::Alert { msg: REJECT_EVERYWHERE_ALERT.into() });
        self.enter_pause(PauseReason::RejectEverywhere, now, out);
    }

    fn start_probe(&mut self, i: usize, kind: ProbeKind, now: Instant, out: &mut Vec<Action>) {
        let slot = SlotId::from_index(i);
        out.push(Action::Probe { slot });
        out.push(Action::Resolve { slot });
        self.slots[i].set(SlotState::Resolving, now);
        self.probe = Some(ProbeRun { slot, kind });
    }

    fn promote(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        let id = SlotId::from_index(i);
        if let Some(a) = self.active() {
            if a != id {
                let until = now.plus_secs(self.cfg.drain_s);
                self.slots[a.index()].set(SlotState::Draining { until }, now);
                out.push(Action::Log { msg: format!("{a}: draining until {until}") });
            }
        }
        {
            let s = &mut self.slots[i];
            s.set(SlotState::Active, now);
            let pending = std::mem::take(&mut s.stats.pending);
            s.stats = ShareStats { pending, ..ShareStats::default() };
            s.jobs_since_active = u32::from(s.job_id.is_some());
        }
        out.push(Action::SetActive { slot: id });
        out.push(Action::Log { msg: format!("{id}: mining") });
        self.target = None;
        if self.probe.is_some_and(|p| p.slot == id) {
            self.probe = None;
        }
        self.phase = Phase::Normal;
        self.tried = [false; MAX_SLOTS];
        self.episode_start = None;
        self.next_probe_at = Some(now.plus_secs(self.cfg.failback_probe_every_s));
    }

    fn bring_up(&mut self, i: usize, now: Instant, out: &mut Vec<Action>) {
        let slot = SlotId::from_index(i);
        match self.slots[i].state {
            SlotState::Idle => {
                out.push(Action::Resolve { slot });
                self.slots[i].set(SlotState::Resolving, now);
            }
            SlotState::Standby | SlotState::Draining { .. } => {
                if self.slots[i].job_id.is_some() {
                    self.promote(i, now, out);
                } else {
                    self.slots[i].set(SlotState::AwaitingJob, now);
                }
            }
            _ => {}
        }
    }

    // ----- group policy -----

    fn reconcile(&mut self, now: Instant, out: &mut Vec<Action>) {
        if !self.pause.is_some_and(PauseReason::is_frozen) {
            match self.active() {
                None => self.reconcile_no_active(now, out),
                Some(a) => self.reconcile_active(a, now, out),
            }
        }
        self.finish(now, out);
    }

    fn reconcile_no_active(&mut self, now: Instant, out: &mut Vec<Action>) {
        if self.phase == Phase::Normal {
            self.phase = Phase::Starting;
        }
        if matches!(self.phase, Phase::Starting | Phase::FailingOver { .. }) && self.episode_start.is_none() {
            self.episode_start = Some(now);
        }
        if let Some(t) = self.target {
            if !self.usable(t.index()) {
                self.target = None;
            }
        }
        if self.target.is_none() {
            let mut pick = match self.phase {
                Phase::Starting | Phase::Normal => self.start_candidate(),
                Phase::FailingOver { from } => self.next_after(from),
                Phase::AllDown { .. } => self.round_robin(),
            };
            if pick.is_none() && !matches!(self.phase, Phase::AllDown { .. }) {
                self.phase = Phase::AllDown { since: now };
                self.episode_start = None;
                let msg = if self.slots.iter().any(Slot::enabled) {
                    ALL_DOWN_ALERT.to_string()
                } else {
                    "no pool is configured and enabled".to_string()
                };
                out.push(Action::Alert { msg });
                pick = self.round_robin();
            }
            if let Some(p) = pick {
                match self.phase {
                    Phase::AllDown { .. } => self.rr_cursor = p.index(),
                    Phase::FailingOver { from } if from != p => {
                        out.push(Action::Log { msg: format!("failing over from {from} to {p}") })
                    }
                    _ => {}
                }
                self.target = Some(p);
            }
        }
        if let Some(pr) = self.probe {
            if Some(pr.slot) == self.target {
                self.probe = None; // the probe session becomes the failover target
            } else {
                self.cancel(pr.slot.index(), now, out);
            }
        }
        if let Some(t) = self.target {
            self.bring_up(t.index(), now, out);
        }
    }

    fn reconcile_active(&mut self, active: SlotId, now: Instant, out: &mut Vec<Action>) {
        self.phase = Phase::Normal;
        self.tried = [false; MAX_SLOTS];
        self.episode_start = None;
        if let Some(t) = self.target {
            if t != active {
                self.cancel(t.index(), now, out);
            }
            self.target = None;
        }
        let every = self.cfg.failback_probe_every_s;
        match self.probe {
            Some(pr) => {
                let wanted = pr.kind == ProbeKind::Manual
                    || self.failback_candidate(active) == Some(pr.slot);
                let i = pr.slot.index();
                if !wanted {
                    self.cancel(i, now, out);
                } else if self.slots[i].state == SlotState::Standby && self.slots[i].job_id.is_some() {
                    let ready = pr.kind == ProbeKind::Manual
                        || now.saturating_since(self.slots[i].entered)
                            >= Self::secs(self.cfg.failback_stable_s);
                    if ready {
                        out.push(Action::Log {
                            msg: format!("switching from {active} to {}", pr.slot),
                        });
                        self.promote(i, now, out);
                    }
                }
            }
            None => {
                let due = self.next_probe_at.is_none_or(|t| now >= t);
                if due {
                    if let Some(c) = self.failback_candidate(active) {
                        out.push(Action::Log { msg: format!("probing {c} for failback") });
                        self.start_probe(c.index(), ProbeKind::Failback, now, out);
                    }
                    self.next_probe_at = Some(now.plus_secs(every));
                }
            }
        }
    }

    fn finish(&mut self, now: Instant, out: &mut Vec<Action>) {
        let want_gpu = self.pause.is_none()
            && self.active().is_some_and(|a| self.slots[a.index()].job_id.is_some());
        if want_gpu != self.gpu_on {
            if want_gpu {
                out.push(Action::StartGpu);
            } else {
                out.insert(0, Action::StopGpu);
            }
            self.gpu_on = want_gpu;
        }
        let active = self.active();
        self.manager = match (self.pause, active) {
            (Some(reason), _) => ManagerState::Paused { reason },
            (None, Some(active)) => ManagerState::Mining { active },
            (None, None) => match (self.phase, self.target) {
                (Phase::AllDown { since }, _) => ManagerState::AllDown { since },
                (Phase::FailingOver { from }, Some(to)) => ManagerState::FailingOver { from, to },
                _ => ManagerState::Starting,
            },
        };
        let next = self.compute_deadline(now);
        if next != self.scheduled {
            if let Some(at) = next {
                out.push(Action::ScheduleTick { at });
            }
            self.scheduled = next;
        }
    }

    fn compute_deadline(&self, now: Instant) -> Option<Instant> {
        let c = &self.cfg;
        let mut best: Option<Instant> = None;
        let mut add = |t: Instant| best = Some(best.map_or(t, |b| b.min(t)));
        for s in &self.slots {
            match &s.state {
                SlotState::Resolving | SlotState::Connecting => add(s.entered.plus_secs(c.connect_timeout_s)),
                SlotState::TlsHandshake | SlotState::Authorizing => {
                    add(s.entered.plus_secs(c.handshake_timeout_s))
                }
                SlotState::AwaitingJob => add(s.entered.plus_secs(c.first_job_timeout_s)),
                SlotState::Active => {
                    add(s.last_job_at.plus_secs(c.stall_soft_reconnect_s));
                    if let Some(&d) = s.stats.pending.front() {
                        add(d);
                    }
                }
                SlotState::Standby => {
                    let failback = self.probe.is_some_and(|p| p.kind == ProbeKind::Failback);
                    if failback && s.job_id.is_some() {
                        add(s.entered.plus_secs(c.failback_stable_s));
                    }
                }
                SlotState::Draining { until } => add(*until),
                SlotState::Backoff { until, .. } | SlotState::Quarantined { until } => add(*until),
                SlotState::ConfigError { retry_at, .. } => add(*retry_at),
                SlotState::Disabled | SlotState::Idle => {}
            }
        }
        if self.active().is_some_and(|a| self.wants_failback(a))
            && self.probe.is_none()
            && !self.pause.is_some_and(PauseReason::is_frozen)
        {
            if let Some(t) = self.next_probe_at {
                add(t);
            }
        }
        best.map(|t| t.max(now.plus_millis(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reject_texts() {
        assert_eq!(classify_reject("Low difficulty share"), RejectKind::LowDiff);
        assert_eq!(classify_reject("Stale share"), RejectKind::Stale);
        assert_eq!(classify_reject("Job not found"), RejectKind::Stale);
        assert_eq!(classify_reject("Invalid proof"), RejectKind::Invalid);
        assert_eq!(classify_reject("proof verification failed"), RejectKind::Invalid);
        assert_eq!(classify_reject("Duplicate share"), RejectKind::Other);
    }

    #[test]
    fn ban_text_detection() {
        assert!(is_ban_text("IP banned for 3600 s"));
        assert!(is_ban_text("You are blacklisted"));
        assert!(!is_ban_text("Invalid proof"));
        assert!(!is_ban_text("bandwidth exceeded"));
    }

    #[test]
    fn submit_reply_mapping() {
        use spm_proto::Reply;
        let s = SlotId(1);
        assert_eq!(Event::from_submit_reply(s, &Reply::Accepted), Some(Event::ShareAccepted { slot: s }));
        assert_eq!(
            Event::from_submit_reply(s, &Reply::Rejected("\"stale\"".into())),
            Some(Event::ShareRejected { slot: s, kind: RejectKind::Stale })
        );
        assert_eq!(Event::from_submit_reply(s, &Reply::Rejected("banned".into())), Some(Event::BanText { slot: s }));
        assert_eq!(Event::from_submit_reply(s, &Reply::Unrelated), None);
    }

    #[test]
    fn backoff_delay_within_jitter_and_capped() {
        let mut st = State::new(FailoverConfig::default(), vec![], 42);
        for n in 0..10u32 {
            let base = [5u64, 10, 20, 40, 80, 120][(n as usize).min(5)] * 1000;
            let d = st.backoff_delay(n).as_millis() as u64;
            assert!(d >= base * 8 / 10 && d <= base * 12 / 10, "n={n} d={d}");
        }
    }

    #[test]
    fn instant_arithmetic() {
        let t = Instant::from_secs(2);
        assert_eq!(t.plus_millis(500).as_millis(), 2500);
        assert_eq!((t + Duration::from_secs(1)).as_millis(), 3000);
        assert_eq!(Instant::from_secs(1).saturating_since(t), Duration::ZERO);
        assert_eq!(t.to_string(), "2.000s");
        assert_eq!(SlotId(0).to_string(), "pool 1");
    }
}
