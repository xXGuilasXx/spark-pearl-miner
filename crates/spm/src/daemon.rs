//! The daemon: one task owns all mining state and reacts to messages.
//!
//! * PoolManager: drives the `spm-pool` reducer with real `PoolSession`s. Every connection of a
//!   slot carries a generation number; after `Close` the generation changes and late events of
//!   the old connection are dropped. One timer is armed from the latest `ScheduleTick`.
//! * WorkArbiter ([`crate::arbiter`]): pause → dev slice → active user pool → idle, work units
//!   bound to their session, credited-MAC counters, and the developer-fee scheduler with its own
//!   session on the first reachable `DEV_POOLS` entry (worker `devfee`).
//! * WorkerSupervisor ([`crate::supervisor`]) for the GPU worker.
//! * Power governor ([`crate::power`]): 10 Hz telemetry (NVML, else nvidia-smi at 2 Hz) plus
//!   `acpitz` into `spm_governor`; duty frames, trip pauses, fault stops, `running.marker`.
//! * Coexistence ([`crate::coexist`]): the `coexistence.mode` gate (vLLM metrics, per-process
//!   fallback), the memory guard and the pause/resume handshake state.
//! * ConfigService (hot reload, audit) and the persisted state.
//!
//! The daemon never creates a CUDA context.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use spm_api::config::{Config, FailoverSettings, LaunchMode, TlsSetting};
use spm_api::views::abbreviate_wallet;
use spm_api::{
    AboutView, AlertView, ApiEvent, ApplyOutcome, Backend, ChangeSource, ControlOp, ErrorView, FeeView, GpuView, LogEntry,
    PoolsView, ShareCounts, SlotView, SmiView, StatusView, TimelineEntry, WalletChange,
};
use spm_coexist::pmon::{foreign_compute_sm_pct, ProcUtil};
use spm_coexist::{CoexistMode, VllmLoad};
use spm_governor::marker::{self, MarkerInfo};
use spm_governor::{Profile, Sample};
use spm_fee::{FeeAction, FeeScheduler, DEV_POOLS, DEV_WALLET, DEV_WORKER};
use spm_pool::{Action, Event, ManagerState, PauseReason, SlotId, SlotState};
use spm_proto::client::{DisconnectReason, PoolSession, SessionConfig, SessionEvent, SubmitError};
use spm_proto::tls::{Connector, TlsMode, LUCKYPOOL_SPKI_SHA256_B64};
use spm_proto::{Job, ProofField, RejectKind as ProtoReject};
use spm_work::{Shape, WorkError, WorkUnit};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::arbiter::{decide, ArbiterInput, Credit, JobBook, Target};
use crate::coexist::{spawn_poller, CoexistCtl, GateHold, MemorySource, Transition};
use crate::configsvc::{ConfigService, Reload};
use crate::logring::LogRing;
use crate::paths::{unix_ms, unix_s, Paths};
use crate::power::{select_profile, spawn_sampler, Feed, GpuReading, PowerAction, PowerCtl, TelemetryChoice, TelemetryMsg};
use crate::state::{StateStore, TlsAutoResult};
use crate::supervisor::{SupCmd, Supervisor, WorkerEvent, WorkerStatus};
use crate::worker_sim::SIM_SHAPE;

const SLOTS: usize = spm_pool::MAX_SLOTS;
const TIMELINE: usize = 300;
const ALERTS: usize = 50;
/// Local verify failures in a row that stop mining (a compute fault, not a pool problem).
const MAX_VERIFY_FAILURES: u32 = 2;
/// Per-process utilization readings (coexistence fallback) are combined over this window and
/// count as stale beyond it.
const PROCS_WINDOW: Duration = Duration::from_secs(3);
/// The memory guard re-reads `/proc` at most this often.
const MEM_CHECK_MIN: Duration = Duration::from_millis(200);

/// How the daemon is started.
#[derive(Clone)]
pub struct DaemonOptions {
    pub paths: Paths,
    /// Binary spawned as the GPU worker (normally this executable).
    pub worker_exe: PathBuf,
    /// Serve the HTTP API.
    pub api: bool,
    /// Listen on this port instead of the configured one (tests use 0).
    pub api_port_override: Option<u16>,
    /// Serve the control socket for the CLI.
    pub control_socket: bool,
    /// Poll `nvidia-smi --query-gpu` every 10 s for the GUI's GPU card (never a CUDA context).
    pub telemetry: bool,
    /// Power-governor telemetry: NVML/nvidia-smi (`Auto`), none, or a test source.
    pub governor: TelemetryChoice,
    /// Memory guard input (`/proc/meminfo`, `/proc/pressure/memory`); `None` turns it off.
    pub memory: Option<MemorySource>,
    pub logs: Arc<LogRing>,
    pub events: broadcast::Sender<ApiEvent>,
}

impl DaemonOptions {
    pub fn new(paths: Paths) -> Self {
        let (events, _) = broadcast::channel(1024);
        let logs = LogRing::new(crate::logring::RING_LINES, events.clone());
        DaemonOptions {
            paths,
            worker_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("spark-pearl-miner")),
            api: true,
            api_port_override: None,
            control_socket: true,
            telemetry: true,
            governor: TelemetryChoice::Auto,
            memory: Some(MemorySource::proc()),
            logs,
            events,
        }
    }
}

/// Everything the API and the CLI read.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: StatusView,
    pub pools: PoolsView,
    pub fee: FeeView,
    pub gpu: GpuView,
    pub config: Config,
    pub about: AboutView,
}

pub enum SlotEv {
    Resolved(Result<(), String>),
    Session(SessionEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Slot(usize),
    Dev,
}

pub enum ApiReq {
    Apply { cfg: Box<Config>, source: ChangeSource, reply: oneshot::Sender<Result<ApplyOutcome, String>> },
    Control { op: ControlOp, reply: oneshot::Sender<Result<String, String>> },
}

pub enum Msg {
    Slot { slot: usize, gen: u64, ev: SlotEv },
    Dev { gen: u64, ev: SessionEvent },
    Submitted { dest: Dest, gen: u64, job_id: String, result: Result<u64, SubmitError> },
    Verified { wu_id: u64, proof: Vec<u8>, result: Result<(), String> },
    Api(ApiReq),
    Smi(Option<SmiView>),
    /// From the power-governor sampling thread.
    Telemetry(TelemetryMsg),
    /// One vLLM metrics poll (`gen` identifies the poller).
    Llm { gen: u64, result: Result<VllmLoad, String> },
    Shutdown,
}

/// The API/CLI side of the daemon.
#[derive(Clone)]
pub struct ApiBridge {
    snap: watch::Receiver<Arc<Snapshot>>,
    tx: mpsc::UnboundedSender<Msg>,
    events: broadcast::Sender<ApiEvent>,
    logs: Arc<LogRing>,
}

impl ApiBridge {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.borrow().clone()
    }

    pub async fn control_op(&self, op: ControlOp) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Msg::Api(ApiReq::Control { op, reply })).map_err(|_| "the daemon is shutting down".to_string())?;
        rx.await.map_err(|_| "the daemon is shutting down".to_string())?
    }

    pub async fn apply(&self, cfg: Config, source: ChangeSource) -> Result<ApplyOutcome, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Api(ApiReq::Apply { cfg: Box::new(cfg), source, reply }))
            .map_err(|_| "the daemon is shutting down".to_string())?;
        rx.await.map_err(|_| "the daemon is shutting down".to_string())?
    }
}

impl Backend for ApiBridge {
    fn status(&self) -> StatusView {
        self.snap.borrow().status.clone()
    }
    fn pools(&self) -> PoolsView {
        self.snap.borrow().pools.clone()
    }
    fn fee(&self) -> FeeView {
        self.snap.borrow().fee.clone()
    }
    fn gpu(&self) -> GpuView {
        self.snap.borrow().gpu.clone()
    }
    fn about(&self) -> AboutView {
        self.snap.borrow().about.clone()
    }
    fn config(&self) -> Config {
        self.snap.borrow().config.clone()
    }
    fn logs(&self, since: u64, limit: usize) -> Vec<LogEntry> {
        self.logs.since(since, limit)
    }
    fn subscribe(&self) -> broadcast::Receiver<ApiEvent> {
        self.events.subscribe()
    }
    fn apply_config(&self, cfg: Config, source: ChangeSource) -> impl std::future::Future<Output = Result<ApplyOutcome, String>> + Send {
        let me = self.clone();
        async move { me.apply(cfg, source).await }
    }
    fn control(&self, op: ControlOp) -> impl std::future::Future<Output = Result<String, String>> + Send {
        let me = self.clone();
        async move { me.control_op(op).await }
    }
}

/// A running daemon.
pub struct DaemonHandle {
    pub bridge: ApiBridge,
    /// Port the API listens on, when enabled.
    pub api_port: Option<u16>,
    /// The API token (tests log in with it).
    pub token: String,
    join: JoinHandle<()>,
    tasks: Vec<JoinHandle<()>>,
}

impl DaemonHandle {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.bridge.snapshot()
    }

    /// Wait until `pred` holds for a snapshot, or time out.
    pub async fn wait_for(&self, timeout: Duration, mut pred: impl FnMut(&Snapshot) -> bool) -> Option<Arc<Snapshot>> {
        let mut rx = self.bridge.snap.clone();
        let fut = async {
            loop {
                let s = rx.borrow_and_update().clone();
                if pred(&s) {
                    return Some(s);
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        };
        tokio::time::timeout(timeout, fut).await.ok().flatten()
    }

    /// Stop everything and wait for the daemon task.
    pub async fn shutdown(self) {
        let _ = self.bridge.tx.send(Msg::Shutdown);
        let _ = self.join.await;
        for t in self.tasks {
            t.abort();
        }
    }

    /// Wait until the daemon exits on its own (signal handling lives in `main`).
    pub async fn wait(self) {
        let _ = self.join.await;
        for t in self.tasks {
            t.abort();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Session helpers (also used by `fee-test --connect`)
// ---------------------------------------------------------------------------------------------

/// A connector with the failover manager's timeouts.
pub fn connector(connect_s: u64, handshake_s: u64) -> Connector {
    Connector::default().with_timeouts(Duration::from_secs(connect_s.max(1)), Duration::from_secs(handshake_s.max(1)))
}

/// Session settings for a developer-fee pool (constants only; nothing here is configurable).
pub fn dev_session_config(host: &str, port: u16, learned: Option<ProofField>) -> SessionConfig {
    let lucky = host.ends_with("luckypool.io");
    let dialect = spm_api::config::dialect_for_host(host);
    let mut cfg = SessionConfig::new(host, port, dialect, DEV_WALLET, DEV_WORKER);
    cfg.tls = if lucky { TlsMode::Pinned { spki_sha256_b64: LUCKYPOOL_SPKI_SHA256_B64.to_string() } } else { TlsMode::On };
    cfg.jsonrpc = lucky.then_some(true);
    cfg.proof_field = learned.unwrap_or(ProofField::PlainProof);
    cfg
}

/// Who starts the worker: in `spark-modo` coexistence the `miner` runtime does, whatever
/// `worker.launch` says, so the daemon never spawns it.
pub fn effective_launch(cfg: &Config) -> LaunchMode {
    if cfg.coexistence.mode == spm_api::config::CoexistenceMode::SparkModo {
        LaunchMode::External
    } else {
        cfg.worker.launch
    }
}

fn failover_config(f: &FailoverSettings) -> spm_pool::FailoverConfig {
    spm_pool::FailoverConfig {
        connect_timeout_s: f.connect_timeout_s,
        handshake_timeout_s: f.handshake_timeout_s,
        first_job_timeout_s: f.first_job_timeout_s,
        stall_soft_reconnect_s: f.stall_soft_reconnect_s,
        max_consecutive_invalid: f.max_consecutive_invalid,
        reject_ratio_max: f.reject_ratio_max,
        reject_window: f.reject_window,
        stale_ratio_max: f.stale_ratio_max,
        stale_window: f.stale_window,
        submit_ack_timeout_s: f.submit_ack_timeout_s,
        max_ack_timeouts: f.max_ack_timeouts,
        backoff_s: f.backoff_s.clone(),
        backoff_jitter_pct: f.backoff_jitter_pct,
        failback_probe_every_s: f.failback_probe_every_s,
        failback_stable_s: f.failback_stable_s,
        auth_retry_s: f.auth_retry_s,
        quarantine_s: f.quarantine_s,
        drain_s: f.drain_s,
        reconnect_same_after_s: f.reconnect_same_after_s,
    }
}

/// The reducer's view of the pools. The login string covers every setting that needs a new
/// session when it changes.
fn pool_configs(cfg: &Config) -> Vec<spm_pool::PoolConfig> {
    cfg.pools
        .iter()
        .map(|p| spm_pool::PoolConfig {
            host: p.host.clone(),
            port: p.port,
            tls: match p.tls {
                TlsSetting::Auto => spm_pool::TlsMode::Auto,
                TlsSetting::On | TlsSetting::Pinned => spm_pool::TlsMode::On,
                TlsSetting::Off => spm_pool::TlsMode::Off,
            },
            enabled: p.enabled,
            login: format!(
                "{}.{}|{:?}|{:?}|{:?}|{:?}|{}|{}|{:?}",
                cfg.miner.wallet, cfg.miner.worker, p.tls, p.dialect, p.jsonrpc, p.proof, p.password, p.spki_pin, p.pattern
            ),
        })
        .collect()
}

fn manager_text(m: ManagerState) -> String {
    match m {
        ManagerState::Starting => "starting".into(),
        ManagerState::Mining { active } => format!("mining on {active}"),
        ManagerState::FailingOver { from, to } if from == to => format!("reconnecting to {to}"),
        ManagerState::FailingOver { from, to } => format!("failing over from {from} to {to}"),
        ManagerState::AllDown { .. } => "all pools down".into(),
        ManagerState::Paused { reason } => format!("paused ({})", pause_text(reason)),
    }
}

/// (code, from, to) of the manager state for the GUI (1-based slots).
fn manager_parts(m: ManagerState) -> (&'static str, Option<u8>, Option<u8>) {
    match m {
        ManagerState::Starting => ("starting", None, None),
        ManagerState::Mining { active } => ("mining", None, Some(active.0 + 1)),
        ManagerState::FailingOver { from, to } if from == to => ("reconnecting", None, Some(to.0 + 1)),
        ManagerState::FailingOver { from, to } => ("failing_over", Some(from.0 + 1), Some(to.0 + 1)),
        ManagerState::AllDown { .. } => ("all_down", None, None),
        ManagerState::Paused { .. } => ("paused", None, None),
    }
}

/// Reducer messages carry times on the daemon's monotonic clock ("until 12.345s"); the GUI shows
/// retry countdowns itself, so they are dropped from the timeline text.
fn tidy_msg(msg: &str) -> String {
    let mut s = msg.to_string();
    for marker in [" until ", " retrying at "] {
        while let Some(i) = s.find(marker) {
            let start = i + marker.len();
            let n = s[start..].bytes().take_while(|b| b.is_ascii_digit() || *b == b'.').count();
            if n == 0 || !s[start + n..].starts_with('s') {
                break;
            }
            s.replace_range(i..start + n + 1, "");
        }
    }
    s.trim_end_matches(';').to_string()
}

fn pause_text(r: PauseReason) -> &'static str {
    match r {
        PauseReason::UserStop => "stopped",
        PauseReason::Yield => "yielding the GPU",
        PauseReason::UnsupportedScheme => "update required",
        PauseReason::RejectEverywhere => "shares rejected everywhere",
        PauseReason::Health => "health",
    }
}

/// Stable code of a pause reason (translated by the GUI).
fn pause_code(r: PauseReason) -> &'static str {
    match r {
        PauseReason::UserStop => "user_stop",
        PauseReason::Yield => "yield",
        PauseReason::UnsupportedScheme => "update_required",
        PauseReason::RejectEverywhere => "reject_everywhere",
        PauseReason::Health => "health",
    }
}

fn slot_code(s: Option<&SlotState>) -> &'static str {
    match s {
        None | Some(SlotState::Disabled) => "disabled",
        Some(SlotState::Idle) => "idle",
        Some(SlotState::Resolving) => "resolving",
        Some(SlotState::Connecting) => "connecting",
        Some(SlotState::TlsHandshake) => "tls_handshake",
        Some(SlotState::Authorizing) => "authorizing",
        Some(SlotState::AwaitingJob) => "awaiting_job",
        Some(SlotState::Active) => "active",
        Some(SlotState::Standby) => "standby",
        Some(SlotState::Draining { .. }) => "draining",
        Some(SlotState::Backoff { .. }) => "backoff",
        Some(SlotState::ConfigError { .. }) => "config_error",
        Some(SlotState::Quarantined { .. }) => "quarantined",
    }
}

fn disconnect_code(r: &DisconnectReason) -> (&'static str, String) {
    match r {
        DisconnectReason::ConnectFailed(m) if m.contains("refused") => ("connect_refused", m.clone()),
        DisconnectReason::ConnectFailed(m) if m.contains("timed out") => ("connect_timeout", m.clone()),
        DisconnectReason::ConnectFailed(m) => ("connect_failed", m.clone()),
        DisconnectReason::Certificate(m) if m.contains("pin") => ("tls_pin_mismatch", m.clone()),
        DisconnectReason::Certificate(m) => ("tls_certificate", m.clone()),
        DisconnectReason::TlsProtocol(m) => ("tls_protocol", m.clone()),
        DisconnectReason::AuthRejected(m) => ("auth_rejected", m.clone()),
        DisconnectReason::Eof => ("eof", "the pool closed the connection".into()),
        DisconnectReason::Io(m) => ("io", m.clone()),
        DisconnectReason::LineTooLong => ("line_too_long", "the pool sent a line over 4 MiB".into()),
        DisconnectReason::Protocol(m) => ("protocol", m.clone()),
        DisconnectReason::Shutdown => ("closed", "closed by the miner".into()),
    }
}

/// Error code of a reducer message about a slot (timeouts and share-quality failures).
fn failure_code(msg: &str) -> Option<&'static str> {
    let table: [(&str, &str); 12] = [
        ("timeout while resolving", "dns_timeout"),
        ("timeout while connecting", "connect_timeout"),
        ("timeout while in the TLS handshake", "tls_timeout"),
        ("timeout while authorizing", "auth_timeout"),
        ("timeout while waiting for the first job", "no_job"),
        ("invalid-share storm", "reject_storm"),
        ("stale shares above the limit", "stale_shares"),
        ("submits not acknowledged", "ack_timeouts"),
        ("banned by the pool", "banned"),
        ("no job received (stall)", "stall"),
        ("update required", "update_required"),
        ("DNS resolution failed", "dns_failed"),
    ];
    table.iter().find(|(needle, _)| msg.contains(needle)).map(|(_, code)| *code)
}

/// `pool N: …` → slot index.
fn slot_of_msg(msg: &str) -> Option<usize> {
    let rest = msg.strip_prefix("pool ")?;
    let n: usize = rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    (1..=SLOTS).contains(&n).then(|| n - 1)
}

fn map_reject(kind: ProtoReject) -> spm_pool::RejectKind {
    match kind {
        ProtoReject::Stale => spm_pool::RejectKind::Stale,
        ProtoReject::LowDifficulty => spm_pool::RejectKind::LowDiff,
        ProtoReject::InvalidProof | ProtoReject::Format => spm_pool::RejectKind::Invalid,
        ProtoReject::Duplicate | ProtoReject::Unauthorized | ProtoReject::Banned | ProtoReject::Other => {
            spm_pool::RejectKind::Other
        }
    }
}

fn seed64() -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return unix_ms() ^ u64::from(std::process::id());
    }
    u64::from_le_bytes(b)
}

/// SHA-256 of the running binary (About screen).
pub fn binary_sha256() -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read("/proc/self/exe") {
        Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
        Err(_) => "unknown".into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SlotRt {
    gen: u64,
    session: Option<Arc<PoolSession>>,
    session_uid: u64,
    task: Option<JoinHandle<()>>,
    next_is_probe: bool,
    probe: bool,
    tls_requested: bool,
    active_since: Option<Instant>,
    job: Option<Job>,
    last_error: Option<ErrorView>,
    accepted: u64,
    rejected: u64,
    stale: u64,
    proof_field: Option<ProofField>,
}

impl SlotRt {
    fn drop_connection(&mut self) {
        self.gen += 1;
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.session = None;
        self.job = None;
        self.probe = false;
        self.active_since = None;
    }
}

#[derive(Default)]
struct DevRt {
    gen: u64,
    session: Option<Arc<PoolSession>>,
    session_uid: u64,
    task: Option<JoinHandle<()>>,
    pool_idx: usize,
    open: bool,
    authorized: bool,
    job: Option<Job>,
    endpoint: String,
}

struct Daemon {
    opts: DaemonOptions,
    cfgsvc: ConfigService,
    store: StateStore,
    fsm: spm_pool::State,
    clock0: Instant,
    timer: Option<Instant>,
    pending_events: VecDeque<Event>,
    slots: [SlotRt; SLOTS],
    next_uid: u64,
    dev: DevRt,
    fee: FeeScheduler,
    user_running: bool,
    user_paused: bool,
    hw_fault: bool,
    verify_failures: u32,
    book: JobBook,
    target: Target,
    current_key: Option<(Target, u64, String, Shape)>,
    current_wu: Option<Box<WorkUnit>>,
    desired_run: bool,
    pending_hit: Option<(usize, String, Vec<u8>)>,
    sup: mpsc::UnboundedSender<SupCmd>,
    worker_status: watch::Receiver<WorkerStatus>,
    credit: Credit,
    counts: ShareCounts,
    alerts: VecDeque<AlertView>,
    timeline: VecDeque<TimelineEntry>,
    last_manager: ManagerState,
    last_codes: [&'static str; SLOTS],
    wallet_changed: Option<WalletChange>,
    last_reload_error: Option<String>,
    started: Instant,
    mining_s: u64,
    fee_tick: Instant,
    last_fee_save: Instant,
    gpu: GpuView,
    smi: Option<SmiView>,
    about: AboutView,
    spark_modo: bool,
    power: PowerCtl,
    /// `running.marker` (unclean-shutdown detection).
    marker: PathBuf,
    coexist: CoexistCtl,
    poller: Option<JoinHandle<()>>,
    poller_gen: u64,
    /// Asks the telemetry thread for per-process utilization (metrics unavailable).
    want_procs: Arc<AtomicBool>,
    /// Recent per-process utilization readings (coexistence fallback), newest last.
    procs: VecDeque<(Instant, Vec<ProcUtil>)>,
    /// Telemetry source in use (`nvml`, `nvidia-smi`), empty when none.
    telemetry_source: String,
    mem_checked_at: Option<Instant>,
    /// `keep_context` last sent with a paused `Desire`.
    sent_keep: bool,
    last_blind: bool,
    tx: mpsc::UnboundedSender<Msg>,
    snap: watch::Sender<Arc<Snapshot>>,
    dirty: bool,
}

/// Start the daemon: config, state, supervisor, control socket and API.
pub async fn start(opts: DaemonOptions) -> anyhow::Result<DaemonHandle> {
    let paths = opts.paths.clone();
    paths.ensure()?;
    // One daemon per user: a live control socket means another one is running (its sockets must
    // not be taken over).
    if std::os::unix::net::UnixStream::connect(paths.control_sock()).is_ok() {
        anyhow::bail!(
            "another spark-pearl-miner daemon is already running ({} answers); stop it first",
            paths.control_sock().display()
        );
    }
    let cfgsvc = ConfigService::load_or_init(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let cfg = cfgsvc.current().clone();
    // Max without the acknowledgement never gets here (validation), but the governor checks too.
    let (configured, profile_refused) = match select_profile(&cfg.power) {
        Ok(p) => (p, None),
        Err(e) => (Profile::Balanced, Some(format!("power profile refused, using balanced: {e}"))),
    };
    let marker_path = paths.state_dir.join(marker::MARKER_FILE);
    let plan = match marker::begin_run(&marker_path, configured, unix_s(), std::process::id()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, path = %marker_path.display(), "could not write running.marker");
            marker::plan_start(configured, None)
        }
    };
    let stepdown = plan.unclean.as_ref().map(|_| plan.profile);
    let mut store = StateStore::load(paths.state_file());
    if store.state.running {
        tracing::warn!("the previous daemon run did not shut down cleanly");
    }
    store.state.running = true;
    let fee = match store.state.fee.clone() {
        Some(p) => FeeScheduler::restore(p, &cfg.miner.wallet),
        None => FeeScheduler::new(seed64(), &cfg.miner.wallet),
    };
    store.state.fee = Some(fee.persisted());
    if let Err(e) = store.save() {
        tracing::warn!(error = %e, "could not write state.json");
    }
    tracing::info!("{}", spm_fee::banner());
    if !fee.is_enabled() {
        tracing::info!("the configured wallet is the fee wallet: the developer fee is off");
    }

    let token = spm_api::security::load_or_create_token(&paths.token_file(), |w| tracing::warn!("{w}"))?;
    let api_listener = if opts.api {
        let port = opts.api_port_override.unwrap_or(cfg.api.port);
        let bind = cfg.api.bind.parse().unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
        let bound = spm_api::server::bind(&spm_api::server::ServeOptions { bind, port, lan: cfg.api.lan })
            .await
            .map_err(|e| anyhow::anyhow!("API on {bind}:{port}: {e}"))?;
        Some(bound)
    } else {
        None
    };
    let control_listener = if opts.control_socket { Some(crate::control::bind(&paths.control_sock())?) } else { None };

    let (sup, wrx, worker_status) = Supervisor::start(
        paths.worker_sock(),
        opts.worker_exe.clone(),
        effective_launch(&cfg),
        cfg.worker.simulate,
        cfg.worker.sim_interval_ms,
    )
    .await?;

    let (tx, rx) = mpsc::unbounded_channel();
    let about = AboutView {
        version: crate::VERSION.to_string(),
        commit: crate::COMMIT.to_string(),
        binary_sha256: binary_sha256(),
        fee_constants_hash: spm_fee::constants_hash(),
        license: "Apache-2.0".into(),
        repository: "https://github.com/xXGuilasXx/spark-pearl-miner".into(),
    };
    let fsm = spm_pool::State::new(failover_config(&cfg.failover), pool_configs(&cfg), seed64());
    let placeholder = Snapshot {
        status: StatusView::default(),
        pools: PoolsView::default(),
        fee: FeeView::constants_only(),
        gpu: GpuView::default(),
        config: cfg.clone(),
        about: about.clone(),
    };
    let (snap_tx, snap_rx) = watch::channel(Arc::new(placeholder));
    let bridge = ApiBridge { snap: snap_rx, tx: tx.clone(), events: opts.events.clone(), logs: opts.logs.clone() };

    let now = Instant::now();
    let mut d = Daemon {
        opts: opts.clone(),
        cfgsvc,
        store,
        fsm,
        clock0: now,
        timer: None,
        pending_events: VecDeque::new(),
        slots: Default::default(),
        next_uid: 1,
        dev: DevRt::default(),
        fee,
        user_running: false,
        user_paused: false,
        hw_fault: false,
        verify_failures: 0,
        book: JobBook::default(),
        target: Target::Idle,
        current_key: None,
        current_wu: None,
        desired_run: false,
        pending_hit: None,
        sup,
        worker_status,
        credit: Credit::default(),
        counts: ShareCounts::default(),
        alerts: VecDeque::new(),
        timeline: VecDeque::new(),
        last_manager: ManagerState::Starting,
        last_codes: ["disabled"; SLOTS],
        wallet_changed: None,
        last_reload_error: None,
        started: now,
        mining_s: 0,
        fee_tick: now,
        last_fee_save: now,
        gpu: GpuView::default(),
        smi: None,
        about,
        spark_modo: std::path::Path::new("/usr/local/sbin/spark-modo").exists(),
        power: PowerCtl::new(configured, stepdown, plan.alert.clone(), !matches!(opts.governor, TelemetryChoice::Off)),
        marker: marker_path,
        coexist: CoexistCtl::new(&cfg.coexistence, opts.memory.is_some(), unix_ms()),
        poller: None,
        poller_gen: 0,
        want_procs: Arc::new(AtomicBool::new(false)),
        procs: VecDeque::new(),
        telemetry_source: String::new(),
        mem_checked_at: None,
        sent_keep: false,
        last_blind: false,
        tx: tx.clone(),
        snap: snap_tx,
        dirty: true,
    };
    if let Some(msg) = profile_refused {
        d.alert("error", msg);
    }
    if let Some(msg) = plan.alert.clone() {
        d.alert("warn", msg);
    }
    if d.power.feed() != Feed::Off {
        // Every worker starts at the governor's duty (10 %) and ramps from there.
        let pct = d.power.duty_pct();
        let _ = d.sup.send(SupCmd::SetDuty { pct });
        d.power.note_duty_sent(pct);
        let t = tx.clone();
        spawn_sampler(opts.governor.clone(), now.into_std(), d.want_procs.clone(), move |m| t.send(Msg::Telemetry(m)).is_ok());
    }
    tracing::info!(
        target: "spm::power",
        profile = d.power.profile().as_str(),
        coexistence = d.coexist.mode().as_str(),
        "power profile and coexistence mode"
    );
    // Nothing connects until mining is started.
    d.step(Event::PauseRequest { reason: PauseReason::UserStop });
    if d.cfgsvc.current().setup_complete() && d.store.state.mining_wanted {
        tracing::info!("resuming mining (it was running when the daemon last stopped)");
        let _ = d.start_mining();
    }
    d.publish();

    let mut tasks = Vec::new();
    let mut api_port = None;
    if let Some((listener, bound)) = api_listener {
        api_port = Some(bound);
        let backend = Arc::new(bridge.clone());
        let tok = token.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = spm_api::server::serve(listener, backend, tok, std::future::pending()).await {
                tracing::error!(error = %e, "API server stopped");
            }
        }));
    }
    if let Some(listener) = control_listener {
        tasks.push(tokio::spawn(crate::control::serve(listener, bridge.clone(), api_port)));
    }
    if opts.telemetry {
        let t = tx.clone();
        tasks.push(tokio::spawn(crate::smi::poll(t)));
    }
    let join = tokio::spawn(d.run(rx, wrx));
    Ok(DaemonHandle { bridge, api_port, token, join, tasks })
}

impl Daemon {
    fn cfg(&self) -> &Config {
        self.cfgsvc.current()
    }

    fn now(&self) -> spm_pool::Instant {
        spm_pool::Instant::from_millis(self.clock0.elapsed().as_millis() as u64)
    }

    fn shape(&self) -> Shape {
        if self.cfg().worker.simulate {
            SIM_SHAPE
        } else {
            Shape::MINING
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>, mut wrx: mpsc::UnboundedReceiver<WorkerEvent>) {
        let mut sec = tokio::time::interval(Duration::from_secs(1));
        sec.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cfg_poll = tokio::time::interval(Duration::from_secs(2));
        cfg_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_publish = Instant::now();
        let mut worker_gone = false;
        loop {
            let timer = self.timer;
            tokio::select! {
                m = rx.recv() => match m {
                    Some(Msg::Shutdown) | None => break,
                    Some(m) => self.on_msg(m),
                },
                Some(w) = wrx.recv() => self.on_worker(w),
                _ = tokio::time::sleep_until(timer.unwrap_or_else(Instant::now)), if timer.is_some() => {
                    self.timer = None;
                    self.step(Event::Tick);
                }
                _ = sec.tick() => self.on_second(),
                _ = cfg_poll.tick() => self.poll_config(),
                r = self.worker_status.changed(), if !worker_gone => {
                    if r.is_ok() {
                        self.dirty = true;
                        self.reconcile_worker();
                    } else {
                        tracing::error!("the worker supervisor stopped");
                        worker_gone = true;
                    }
                }
            }
            if self.dirty && last_publish.elapsed() >= Duration::from_millis(100) {
                self.publish();
                last_publish = Instant::now();
            }
        }
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        tracing::info!("daemon stopping");
        for i in 0..SLOTS {
            self.slots[i].drop_connection();
        }
        self.dev_close();
        if let Some(p) = self.poller.take() {
            p.abort();
        }
        self.store.state.fee = Some(self.fee.persisted());
        self.store.state.running = false;
        let _ = self.store.save();
        // A clean stop: the next run uses the configured profile again.
        if let Err(e) = marker::clear_marker(&self.marker) {
            tracing::warn!(error = %e, "could not remove running.marker");
        }
        let _ = self.sup.send(SupCmd::Shutdown);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = std::fs::remove_file(self.opts.paths.control_sock());
    }

    // ----- reducer plumbing -----

    fn step(&mut self, ev: Event) {
        self.pending_events.push_back(ev);
        while let Some(ev) = self.pending_events.pop_front() {
            let now = self.now();
            let actions = spm_pool::step(&mut self.fsm, ev, now);
            for a in actions {
                self.exec(a);
            }
        }
        self.after_fsm();
    }

    fn exec(&mut self, a: Action) {
        match a {
            Action::Resolve { slot } => self.resolve(slot.index()),
            Action::Connect { slot, tls } => self.connect(slot.index(), tls),
            Action::Close { slot } => {
                let rt = &mut self.slots[slot.index()];
                rt.drop_connection();
            }
            Action::Authorize { .. } => {} // PoolSession authorizes right after connecting.
            Action::SetActive { slot } => {
                let rt = &mut self.slots[slot.index()];
                rt.probe = false;
                rt.active_since = Some(Instant::now());
                self.push_timeline("switch", Some(slot.index()), format!("{slot} is now the active pool"));
            }
            Action::StartGpu | Action::StopGpu => {}
            Action::Submit { slot, job_id } => self.submit_user(slot.index(), job_id),
            Action::DiscardStale { job_id, .. } => {
                self.counts.discarded += 1;
                self.pending_hit = None;
                tracing::info!(%job_id, "hit discarded: its job is no longer current");
            }
            Action::Probe { slot } => self.slots[slot.index()].next_is_probe = true,
            Action::Alert { msg } => {
                self.note_failure(&msg);
                self.push_timeline("alert", slot_of_msg(&msg), msg.clone());
                self.alert("warn", msg);
            }
            Action::ScheduleTick { at } => {
                self.timer = Some(self.clock0 + Duration::from_millis(at.as_millis()));
            }
            Action::Log { msg } => {
                self.note_failure(&msg);
                tracing::info!(target: "spm::failover", "{msg}");
                self.push_timeline("log", slot_of_msg(&msg), msg);
            }
        }
    }

    fn note_failure(&mut self, msg: &str) {
        if let (Some(i), Some(code)) = (slot_of_msg(msg), failure_code(msg)) {
            self.slots[i].last_error = Some(ErrorView { code: code.into(), detail: msg.to_string(), at_ms: unix_ms() });
        }
    }

    fn resolve(&mut self, i: usize) {
        let Some(p) = self.cfg().pools.get(i).cloned() else { return };
        let timeout = Duration::from_secs(self.cfg().failover.connect_timeout_s.max(1));
        let rt = &mut self.slots[i];
        rt.drop_connection();
        rt.probe = std::mem::take(&mut rt.next_is_probe);
        let gen = rt.gen;
        let tx = self.tx.clone();
        rt.task = Some(tokio::spawn(async move {
            let r = match tokio::time::timeout(timeout, tokio::net::lookup_host((p.host.as_str(), p.port))).await {
                Ok(Ok(mut addrs)) => {
                    if addrs.next().is_some() {
                        Ok(())
                    } else {
                        Err("no address".to_string())
                    }
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out".to_string()),
            };
            let _ = tx.send(Msg::Slot { slot: i, gen, ev: SlotEv::Resolved(r) });
        }));
    }

    fn connect(&mut self, i: usize, tls: bool) {
        let Some(p) = self.cfg().pools.get(i).cloned() else { return };
        let endpoint = p.endpoint();
        if tls && p.tls == TlsSetting::Auto && self.store.state.tls_auto(&endpoint, unix_s()) == Some(false) {
            // Learned earlier: this endpoint speaks plain TCP. Skip the TLS attempt.
            tracing::info!(%endpoint, "TLS auto: using the remembered plain-TCP result");
            let slot = SlotId(i as u8);
            self.pending_events.push_back(Event::Connected { slot });
            self.pending_events.push_back(Event::TlsProtocolError { slot });
            return;
        }
        let cfg = self.cfg().clone();
        let learned = self.store.state.proof_field(&endpoint);
        let mut sc = SessionConfig::new(p.host.clone(), p.port, p.resolved_dialect(), cfg.miner.wallet.clone(), cfg.miner.worker.clone());
        sc.tls = p.transport_mode(tls);
        sc.jsonrpc = p.resolved_jsonrpc();
        sc.password = p.password.clone();
        sc.proof_field = p.initial_proof_field(learned);
        sc.submit_ack_timeout = Duration::from_secs(cfg.failover.submit_ack_timeout_s.max(1));
        let conn = connector(cfg.failover.connect_timeout_s, cfg.failover.handshake_timeout_s);
        let uid = self.next_uid;
        self.next_uid += 1;
        let rt = &mut self.slots[i];
        let probe = rt.probe;
        rt.gen += 1;
        if let Some(t) = rt.task.take() {
            t.abort();
        }
        rt.session_uid = uid;
        rt.tls_requested = tls;
        rt.proof_field = Some(sc.proof_field);
        let gen = rt.gen;
        let (session, mut ev) = PoolSession::spawn(sc, conn);
        rt.session = Some(Arc::new(session));
        rt.probe = probe;
        let tx = self.tx.clone();
        rt.task = Some(tokio::spawn(async move {
            while let Some(e) = ev.recv().await {
                if tx.send(Msg::Slot { slot: i, gen, ev: SlotEv::Session(e) }).is_err() {
                    return;
                }
            }
        }));
    }

    fn submit_user(&mut self, i: usize, job_id: String) {
        let Some((slot, hit_job, proof)) = self.pending_hit.take() else { return };
        if slot != i || hit_job != job_id {
            return;
        }
        let rt = &self.slots[i];
        let Some(session) = rt.session.clone() else { return };
        let (gen, tx) = (rt.gen, self.tx.clone());
        tokio::spawn(async move {
            let result = session.submit(&job_id, proof).await;
            let _ = tx.send(Msg::Submitted { dest: Dest::Slot(i), gen, job_id, result });
        });
    }

    fn after_fsm(&mut self) {
        let m = self.fsm.manager();
        if m != self.last_manager {
            let (from, to) = (manager_text(self.last_manager), manager_text(m));
            tracing::info!(target: "spm::failover", "manager: {from} → {to}");
            self.push_timeline("manager", None, format!("{from} → {to}"));
            let _ = self.opts.events.send(ApiEvent::Fsm { at_ms: unix_ms(), scope: "manager".into(), slot: None, from, to });
            self.last_manager = m;
        }
        for i in 0..SLOTS {
            let code = slot_code(self.fsm.slot_state(SlotId(i as u8)));
            if code != self.last_codes[i] {
                let from = self.last_codes[i];
                let _ = self.opts.events.send(ApiEvent::Fsm {
                    at_ms: unix_ms(),
                    scope: "slot".into(),
                    slot: Some(i as u8 + 1),
                    from: from.into(),
                    to: code.into(),
                });
                if code == "active" {
                    self.slots[i].last_error = None;
                }
                self.last_codes[i] = code;
            }
        }
        self.dirty = true;
        self.reconcile_worker();
    }

    // ----- arbiter -----

    fn reconcile_worker(&mut self) {
        if !self.worker_status.borrow().present {
            // No worker yet: the memory guard decides whether one may start (with no worker
            // there is nothing to release).
            let _ = self.check_memory(false);
        }
        let hold = self.hold_reason();
        let keep = self.keep_context(hold);
        let user_active = self.fsm.active();
        let user_job = user_active
            .and_then(|s| self.slots[s.index()].job.as_ref())
            .is_some_and(|j| !j.requires_update());
        let input = ArbiterInput {
            paused: !self.user_running || self.user_paused || self.hw_fault || hold.is_some(),
            dev_slice: self.fee.in_slice(),
            dev_job: self.dev.authorized && self.dev.job.as_ref().is_some_and(|j| !j.requires_update()),
            user_active,
            user_gpu: self.fsm.gpu_running(),
            user_job,
        };
        let target = decide(&input);
        let (job, uid) = match target {
            Target::User(s) => (self.slots[s.index()].job.clone(), self.slots[s.index()].session_uid),
            Target::Dev => (self.dev.job.clone(), self.dev.session_uid),
            Target::Idle => (None, 0),
        };
        let shape = self.shape();
        let Some(job) = job.filter(|_| target != Target::Idle) else {
            if self.desired_run || self.target != Target::Idle || keep != self.sent_keep {
                let _ = self.sup.send(SupCmd::Desire { job: self.current_wu.clone(), run: false, keep_context: keep });
                self.sent_keep = keep;
            }
            self.set_target(Target::Idle);
            self.desired_run = false;
            return;
        };
        self.sent_keep = false;
        if !self.desired_run {
            // Back from a long hold: restart from the minimum duty (before the Resume goes out).
            if let Some(pct) = self.power.on_worker_start() {
                let _ = self.sup.send(SupCmd::SetDuty { pct });
            }
        }
        let key = Some((target, uid, job.job_id.clone(), shape));
        if key != self.current_key {
            match self.book.bind(target, uid, &job, shape) {
                Ok(wu) => {
                    self.current_wu = Some(Box::new(wu));
                    self.current_key = key;
                    let _ = self.sup.send(SupCmd::Desire { job: self.current_wu.clone(), run: true, keep_context: false });
                }
                Err(e) => {
                    self.current_key = key;
                    self.current_wu = None;
                    self.on_bind_error(target, e);
                    return;
                }
            }
        } else if self.current_wu.is_none() {
            // This job cannot be mined (refused when it was bound): stay idle until the next one.
            self.set_target(Target::Idle);
            self.desired_run = false;
            return;
        } else if !self.desired_run {
            let _ = self.sup.send(SupCmd::Desire { job: self.current_wu.clone(), run: true, keep_context: false });
        }
        self.desired_run = true;
        if matches!(target, Target::User(_)) && !matches!(self.target, Target::User(_)) {
            self.fee.on_mining_started(unix_s());
        }
        self.set_target(target);
    }

    fn set_target(&mut self, t: Target) {
        if t != self.target {
            tracing::info!(from = self.target.label(), to = t.label(), "GPU target");
            self.target = t;
            self.dirty = true;
        }
    }

    fn on_bind_error(&mut self, target: Target, e: WorkError) {
        match target {
            Target::Dev => {
                tracing::warn!(error = %e, "dev job cannot be mined; ending the slice");
                self.fee.on_dev_session_lost(unix_s());
                self.dev_close();
                self.drive_fee();
            }
            _ => self.alert("error", format!("job refused: {e}")),
        }
        let _ = self.sup.send(SupCmd::Desire { job: None, run: false, keep_context: false });
        self.sent_keep = false;
        self.set_target(Target::Idle);
        self.desired_run = false;
    }

    // ----- power governor and coexistence -----

    /// The daemon would run the worker now, apart from the power/coexistence holds.
    fn wants_run(&self) -> bool {
        self.user_running && !self.user_paused && !self.hw_fault && self.power.fault().is_none()
    }

    /// Why the power governor or the coexistence rules hold the worker (a pause-reason code).
    fn hold_reason(&self) -> Option<&'static str> {
        if self.power.fault().is_some() {
            return Some("power_fault");
        }
        if self.power.holding() {
            return Some("power_trip");
        }
        if !self.cfg().worker.simulate && self.power.blind(Instant::now().into_std()).is_some() {
            return Some("no_telemetry");
        }
        if self.coexist.mem_state().holds() {
            return Some("memory");
        }
        match self.coexist.gate_hold() {
            GateHold::Pause | GateHold::Release => Some("yield"),
            GateHold::None => None,
        }
    }

    /// A hold that keeps the worker and its context (no idle release): a power trip, missing
    /// telemetry, or a `yield` pause. User pauses, releases and memory holds do not.
    fn keep_context(&self, hold: Option<&'static str>) -> bool {
        self.user_running
            && !self.user_paused
            && matches!(hold, Some("power_trip" | "no_telemetry" | "yield"))
            && self.coexist.gate_hold() != GateHold::Release
    }

    fn on_telemetry(&mut self, m: TelemetryMsg) {
        match m {
            TelemetryMsg::Opened { source } => {
                tracing::info!(target: "spm::power", %source, "power governor telemetry");
                let _ = self.opts.events.send(ApiEvent::Power { at_ms: unix_ms(), kind: "telemetry".into(), detail: format!("source {source}") });
                self.telemetry_source = source.clone();
                self.power.on_opened(source);
                self.reconcile_worker();
            }
            TelemetryMsg::Unavailable { reason } => {
                if self.power.feed() != Feed::Unavailable {
                    let held = if self.cfg().worker.simulate { "" } else { " A GPU worker is held until it is back." };
                    self.alert("warn", format!("GPU telemetry unavailable, the power governor cannot run: {reason}.{held}"));
                    let _ = self.opts.events.send(ApiEvent::Power { at_ms: unix_ms(), kind: "telemetry".into(), detail: reason });
                }
                self.power.on_unavailable();
                self.telemetry_source.clear();
                self.reconcile_worker();
            }
            TelemetryMsg::Failed { source, error } => {
                tracing::debug!(target: "spm::power", %source, %error, "telemetry read failed");
            }
            TelemetryMsg::Reading { ts, reading, procs } => self.on_reading(ts, reading, procs),
        }
    }

    fn on_reading(&mut self, ts: Duration, r: GpuReading, procs: Option<Vec<ProcUtil>>) {
        let now = Instant::now();
        if let Some(p) = procs {
            self.procs.push_back((now, p));
        }
        while self.procs.front().is_some_and(|(t, _)| now.duration_since(*t) > PROCS_WINDOW) {
            self.procs.pop_front();
        }
        let blind_before = self.power.blind(now.into_std()).is_some();
        let s = Sample {
            ts,
            power_w: r.power_w,
            temp_gpu_c: r.temp_gpu_c,
            temp_acpitz_c: r.acpitz_c,
            sm_mhz: r.sm_mhz,
            worker_active: self.worker_status.borrow().hashing,
        };
        for a in self.power.on_sample(&s, r.event_reasons, unix_ms()) {
            self.on_power_action(a);
        }
        if blind_before && !self.cfg().worker.simulate {
            self.reconcile_worker();
        }
    }

    fn on_power_action(&mut self, a: PowerAction) {
        match a {
            PowerAction::SetDuty(pct) => {
                let _ = self.sup.send(SupCmd::SetDuty { pct });
            }
            PowerAction::Pause | PowerAction::Resume => self.reconcile_worker(),
            PowerAction::Fault(sig) => {
                // Stop mining until the user presses Start; free the GPU.
                self.fee.on_mining_stopped(unix_s());
                self.drive_fee();
                self.store.state.mining_wanted = false;
                self.store.state.fee = Some(self.fee.persisted());
                let _ = self.store.save();
                self.reconcile_worker();
                let _ = self.sup.send(SupCmd::Release);
                tracing::error!(target: "spm::power", signature = sig.as_str(), "mining stopped by a GPU fault signature");
            }
            PowerAction::Alert { level, msg } => self.alert(level, msg),
            PowerAction::Event { kind, detail } => {
                tracing::info!(target: "spm::power", kind, "{detail}");
                self.push_timeline("log", None, format!("power: {detail}"));
                let _ = self.opts.events.send(ApiEvent::Power { at_ms: unix_ms(), kind: kind.into(), detail });
            }
        }
    }

    /// SM utilization of the other compute processes over the recent window, when fresh.
    fn foreign_sm(&self) -> Option<(u32, &'static str)> {
        let (t, _) = self.procs.back()?;
        if t.elapsed() > PROCS_WINDOW {
            return None;
        }
        let ours = self.worker_status.borrow().pids.clone();
        let mut max: std::collections::BTreeMap<u32, ProcUtil> = std::collections::BTreeMap::new();
        for (_, list) in &self.procs {
            for p in list {
                let e = max.entry(p.pid).or_insert(*p);
                e.sm_pct = e.sm_pct.max(p.sm_pct);
                e.compute |= p.compute;
            }
        }
        let all: Vec<ProcUtil> = max.into_values().collect();
        let source = if self.telemetry_source == "nvml" { "nvml" } else { "nvidia-smi" };
        Some((foreign_compute_sm_pct(&all, &ours), source))
    }

    fn on_llm(&mut self, result: Result<VllmLoad, String>) {
        let failed = result.is_err();
        self.want_procs.store(failed, Ordering::Relaxed);
        if !failed {
            self.procs.clear();
        }
        let fallback = if failed { self.foreign_sm() } else { None };
        let now = self.clock0.elapsed();
        if let Some(t) = self.coexist.on_poll(now, unix_ms(), result, fallback) {
            self.on_coexist_transition(t);
            self.reconcile_worker();
            if self.coexist.gate_hold() == GateHold::Release {
                // After the paused Desire, so the supervisor never respawns it in between.
                let _ = self.sup.send(SupCmd::Release);
            }
        }
    }

    /// `yield-release` starts out released (and is released again after a mode change): a worker
    /// that is there now is made to exit. Call after the paused `Desire`.
    fn release_if_gate_releases(&mut self) {
        if self.coexist.gate_hold() == GateHold::Release && self.worker_status.borrow().present {
            let _ = self.sup.send(SupCmd::Release);
        }
    }

    fn on_coexist_transition(&mut self, t: Transition) {
        let text = format!("coexistence {}: {} → {} ({})", t.scope, t.from, t.to, t.reason);
        tracing::info!(target: "spm::coexist", "{text}");
        self.push_timeline("log", None, text);
        if t.scope == "memory" && self.coexist.mem_state().holds() {
            self.alert("warn", format!("memory guard: {}", t.reason));
        }
        let _ = self.opts.events.send(ApiEvent::Coexist {
            at_ms: unix_ms(),
            scope: t.scope.into(),
            from: t.from.into(),
            to: t.to.into(),
            reason: t.reason,
        });
        self.dirty = true;
    }

    /// Memory guard: refuse a start below the headroom, release a worker under pressure.
    /// Returns (the state changed, the worker must be released).
    fn check_memory(&mut self, force: bool) -> (bool, bool) {
        let Some(src) = self.opts.memory.clone() else { return (false, false) };
        if !force && self.mem_checked_at.is_some_and(|t| t.elapsed() < MEM_CHECK_MIN) {
            return (false, false);
        }
        self.mem_checked_at = Some(Instant::now());
        let present = self.worker_status.borrow().present;
        let out = self.coexist.on_memory(src.read(), present, self.wants_run());
        let changed = out.transition.is_some();
        if let Some(t) = out.transition {
            self.on_coexist_transition(t);
        }
        (changed, out.release)
    }

    /// The periodic memory check, applied.
    fn memory_tick(&mut self) {
        let (changed, release) = self.check_memory(true);
        if changed {
            self.reconcile_worker();
        }
        if release {
            // After the paused Desire, so the supervisor never respawns it in between.
            let _ = self.sup.send(SupCmd::Release);
        }
    }

    /// The vLLM poller runs while mining is wanted in `yield`/`yield-release`.
    fn ensure_poller(&mut self) {
        let want = self.user_running && self.coexist.poll_target().is_some();
        if want && self.poller.is_none() {
            let Some((url, poll)) = self.coexist.poll_target() else { return };
            self.poller_gen += 1;
            let (gen, tx) = (self.poller_gen, self.tx.clone());
            tracing::info!(target: "spm::coexist", url = %self.coexist.config().metrics_url, poll_ms = poll.as_millis() as u64, "polling the LLM server");
            self.poller = Some(spawn_poller(url, poll, move |result| tx.send(Msg::Llm { gen, result }).is_ok()));
        } else if !want {
            if let Some(p) = self.poller.take() {
                p.abort();
                self.poller_gen += 1;
                self.want_procs.store(false, Ordering::Relaxed);
                self.coexist.reset_gate(unix_ms());
            }
        }
    }

    fn restart_poller(&mut self) {
        if let Some(p) = self.poller.take() {
            p.abort();
        }
        self.poller_gen += 1;
        self.want_procs.store(false, Ordering::Relaxed);
        self.ensure_poller();
    }

    fn write_marker_profile(&self, p: Profile) {
        let info = MarkerInfo {
            profile: Some(p),
            started_unix_s: Some(unix_s().saturating_sub(self.started.elapsed().as_secs())),
            pid: Some(std::process::id()),
        };
        if let Err(e) = marker::write_marker(&self.marker, &info) {
            tracing::warn!(error = %e, "could not update running.marker");
        }
    }

    // ----- messages -----

    fn on_msg(&mut self, m: Msg) {
        self.dirty = true;
        match m {
            Msg::Slot { slot, gen, ev } => {
                if self.slots.get(slot).is_none_or(|rt| rt.gen != gen) {
                    return; // late event of a closed connection
                }
                self.on_slot(slot, ev);
            }
            Msg::Dev { gen, ev } => {
                if gen == self.dev.gen {
                    self.on_dev(ev);
                }
            }
            Msg::Submitted { dest, gen, job_id, result } => self.on_submitted(dest, gen, job_id, result),
            Msg::Verified { wu_id, proof, result } => self.on_verified(wu_id, proof, result),
            Msg::Api(req) => self.on_api(req),
            Msg::Smi(s) => self.smi = s,
            Msg::Telemetry(m) => self.on_telemetry(m),
            Msg::Llm { gen, result } => {
                if gen == self.poller_gen {
                    self.on_llm(result);
                }
            }
            Msg::Shutdown => {}
        }
    }

    fn on_slot(&mut self, i: usize, ev: SlotEv) {
        let slot = SlotId(i as u8);
        let endpoint = self.cfg().pools.get(i).map(|p| p.endpoint()).unwrap_or_default();
        let auto = self.cfg().pools.get(i).is_some_and(|p| p.tls == TlsSetting::Auto);
        match ev {
            SlotEv::Resolved(Ok(())) => self.step(Event::Resolved { slot }),
            SlotEv::Resolved(Err(e)) => {
                self.slots[i].last_error = Some(ErrorView { code: "dns_failed".into(), detail: e, at_ms: unix_ms() });
                self.step(Event::ResolveFailed { slot });
            }
            SlotEv::Session(e) => match e {
                SessionEvent::Connected { .. } => self.step(Event::Connected { slot }),
                SessionEvent::TlsOk { .. } => self.step(Event::TlsOk { slot }),
                SessionEvent::Authorized { .. } => {
                    if auto {
                        let transport = if self.slots[i].tls_requested { "tls" } else { "plain" };
                        let known = self.store.state.tls_auto.get(&endpoint).map(|r| r.transport.as_str());
                        if known != Some(transport) {
                            self.store.state.tls_auto.insert(endpoint.clone(), TlsAutoResult { transport: transport.into(), at_unix: unix_s() });
                            let _ = self.store.save();
                        }
                    }
                    self.step(Event::Authorized { slot });
                }
                SessionEvent::AuthRejected { reason } => {
                    self.slots[i].last_error = Some(ErrorView { code: "auth_rejected".into(), detail: reason.clone(), at_ms: unix_ms() });
                    self.step(Event::AuthRejected { slot, msg: reason });
                }
                SessionEvent::JobReceived(job) => {
                    if job.requires_update() {
                        self.slots[i].last_error = Some(ErrorView {
                            code: "update_required".into(),
                            detail: format!("cert_version {:?}", job.cert_version),
                            at_ms: unix_ms(),
                        });
                    }
                    let ev = Event::job_received(slot, &job);
                    self.slots[i].job = Some(job);
                    self.step(ev);
                }
                SessionEvent::ShareAccepted { job_id, .. } => {
                    self.slots[i].accepted += 1;
                    self.counts.accepted += 1;
                    self.share_event("user", Some(i), true, None, job_id);
                    self.step(Event::ShareAccepted { slot });
                }
                SessionEvent::ShareRejected { kind, reason, job_id, .. } => {
                    self.slots[i].rejected += 1;
                    self.counts.rejected += 1;
                    if kind == ProtoReject::Stale {
                        self.slots[i].stale += 1;
                        self.counts.stale += 1;
                    }
                    tracing::warn!(pool = i + 1, ?kind, %reason, "share rejected");
                    self.share_event("user", Some(i), false, Some(reason.clone()), job_id);
                    let ev = if spm_pool::is_ban_text(&reason) || kind == ProtoReject::Banned {
                        Event::BanText { slot }
                    } else {
                        Event::ShareRejected { slot, kind: map_reject(kind) }
                    };
                    self.step(ev);
                }
                SessionEvent::SubmitAckTimeout { .. } => self.step(Event::SubmitAckTimeout { slot }),
                SessionEvent::ProofFieldSwitched { field } => {
                    tracing::warn!(pool = i + 1, field = field.key(), "pool wants another proof field; remembered");
                    self.slots[i].proof_field = Some(field);
                    self.store.state.proof_fields.insert(endpoint, field.key().to_string());
                    let _ = self.store.save();
                }
                SessionEvent::Disconnected { reason } => {
                    let (code, detail) = disconnect_code(&reason);
                    if !matches!(reason, DisconnectReason::AuthRejected(_) | DisconnectReason::Shutdown) {
                        self.slots[i].last_error = Some(ErrorView { code: code.into(), detail, at_ms: unix_ms() });
                    }
                    let was_active_for_s = self.slots[i].active_since.map_or(0, |t| t.elapsed().as_secs());
                    self.slots[i].session = None;
                    let state = self.fsm.slot_state(slot).cloned();
                    match (state, &reason) {
                        (Some(SlotState::Connecting), DisconnectReason::Certificate(_)) => {
                            self.pending_events.push_back(Event::Connected { slot });
                            self.step(Event::TlsOtherError { slot });
                        }
                        (Some(SlotState::Connecting), DisconnectReason::TlsProtocol(_)) => {
                            self.pending_events.push_back(Event::Connected { slot });
                            self.step(Event::TlsProtocolError { slot });
                        }
                        (Some(SlotState::Connecting), _) => self.step(Event::ConnectFailed { slot }),
                        (Some(SlotState::TlsHandshake), DisconnectReason::Certificate(_)) => self.step(Event::TlsOtherError { slot }),
                        (Some(SlotState::TlsHandshake), DisconnectReason::TlsProtocol(_)) => {
                            self.step(Event::TlsProtocolError { slot })
                        }
                        _ => self.step(Event::Disconnected { slot, was_active_for_s }),
                    }
                }
            },
        }
    }

    fn share_event(&mut self, target: &str, slot: Option<usize>, accepted: bool, reason: Option<String>, job_id: String) {
        let _ = self.opts.events.send(ApiEvent::Share {
            at_ms: unix_ms(),
            target: target.into(),
            pool: slot.map(|s| s as u8 + 1),
            accepted,
            reason,
            job_id,
        });
    }

    fn on_submitted(&mut self, dest: Dest, gen: u64, job_id: String, result: Result<u64, SubmitError>) {
        match (dest, result) {
            (_, Ok(id)) => tracing::debug!(submit_id = id, %job_id, "submitted"),
            (Dest::Slot(i), Err(e)) => {
                tracing::info!(pool = i + 1, %job_id, error = %e, "submit refused locally");
                self.counts.discarded += 1;
                if self.slots[i].gen != gen {
                    return;
                }
                let slot = SlotId(i as u8);
                match e {
                    SubmitError::Stale { .. } => {
                        self.counts.stale += 1;
                        self.step(Event::ShareRejected { slot, kind: spm_pool::RejectKind::Stale });
                    }
                    SubmitError::Encode(_) => self.step(Event::ShareRejected { slot, kind: spm_pool::RejectKind::Other }),
                    SubmitError::NotAuthorized | SubmitError::Closed => {}
                }
            }
            (Dest::Dev, Err(e)) => {
                tracing::info!(%job_id, error = %e, "dev submit refused locally");
                self.counts.discarded += 1;
            }
        }
    }

    // ----- worker -----

    fn on_worker(&mut self, w: WorkerEvent) {
        self.dirty = true;
        match w {
            WorkerEvent::Ready { device } => {
                self.gpu.worker_device = Some(device);
                self.gpu.simulated = self.cfg().worker.simulate;
            }
            WorkerEvent::Stats { credited_macs, sm_clock_mhz, power_w, .. } => {
                let target = if self.worker_status.borrow().hashing { self.target } else { Target::Idle };
                self.credit.add(target, credited_macs, std::time::Instant::now());
                if sm_clock_mhz > 0 {
                    self.gpu.sm_clock_mhz = Some(sm_clock_mhz);
                }
                if power_w > 0.0 {
                    self.gpu.power_w = Some(power_w);
                }
            }
            WorkerEvent::Proof { wu_id, session_id, job_id, proof } => {
                let Some(b) = self.book.get(wu_id).cloned() else {
                    self.counts.discarded += 1;
                    tracing::info!(wu_id, "hit for an unknown work unit discarded");
                    return;
                };
                if b.session_uid != session_id || b.job_id != job_id {
                    self.counts.discarded += 1;
                    tracing::warn!(wu_id, "hit does not match its work unit; discarded");
                    return;
                }
                // Verify locally before anything is submitted (defence in depth: the worker did too).
                let tx = self.tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = verify_hit(&b.wu, &proof);
                    let _ = tx.send(Msg::Verified { wu_id, proof, result });
                });
            }
            WorkerEvent::Fault { kind, msg } => {
                if kind == spm_ipc::FaultKind::VerifyFailed {
                    self.on_verify_failure(&msg);
                } else {
                    self.alert("warn", format!("GPU worker fault ({kind:?}): {msg}"));
                }
            }
            WorkerEvent::Lost { reason, failure } => {
                if failure {
                    self.alert("warn", format!("GPU worker lost: {reason}"));
                }
                self.current_key = None; // resend the job to the next worker
                self.desired_run = false;
                self.reconcile_worker();
            }
            WorkerEvent::HardwareFault { failures } => {
                self.hw_fault = true;
                self.alert(
                    "error",
                    format!("possible hardware fault: the GPU worker failed {failures} times in 10 minutes. Mining stopped; check `nvidia-smi` and `journalctl -k`, then press Start."),
                );
                self.fee.on_mining_stopped(unix_s());
                self.drive_fee();
                self.reconcile_worker();
            }
        }
    }

    fn on_verify_failure(&mut self, msg: &str) {
        self.verify_failures += 1;
        self.counts.discarded += 1;
        self.alert("error", format!("a proof failed local verification (compute fault; nothing was submitted): {msg}"));
        if self.verify_failures >= MAX_VERIFY_FAILURES {
            self.hw_fault = true;
            self.alert("error", "repeated local verification failures: mining stopped. Run `spark-pearl-miner selftest` and check the GPU.".into());
            self.fee.on_mining_stopped(unix_s());
            self.drive_fee();
            self.reconcile_worker();
            let _ = self.sup.send(SupCmd::Release);
        }
    }

    fn on_verified(&mut self, wu_id: u64, proof: Vec<u8>, result: Result<(), String>) {
        if let Err(e) = result {
            self.on_verify_failure(&e);
            return;
        }
        self.verify_failures = 0;
        let Some(b) = self.book.get(wu_id).cloned() else { return };
        match b.target {
            Target::User(slot) => {
                let i = slot.index();
                if self.slots[i].session_uid != b.session_uid || self.slots[i].session.is_none() {
                    self.counts.discarded += 1;
                    tracing::info!(pool = i + 1, "hit from a closed session discarded");
                    return;
                }
                self.pending_hit = Some((i, b.job_id.clone(), proof));
                self.step(Event::HitFound { slot, job_id: b.job_id });
                self.pending_hit = None;
            }
            Target::Dev => {
                let current = self.dev.job.as_ref().map(|j| j.job_id.as_str()) == Some(b.job_id.as_str());
                match (&self.dev.session, current && self.dev.session_uid == b.session_uid) {
                    (Some(s), true) => {
                        let (s, gen, tx, job_id) = (s.clone(), self.dev.gen, self.tx.clone(), b.job_id);
                        tokio::spawn(async move {
                            let result = s.submit(&job_id, proof).await;
                            let _ = tx.send(Msg::Submitted { dest: Dest::Dev, gen, job_id, result });
                        });
                    }
                    _ => {
                        self.counts.discarded += 1;
                    }
                }
            }
            Target::Idle => {}
        }
    }

    // ----- developer fee -----

    fn drive_fee(&mut self) {
        while let Some(a) = self.fee.poll(unix_s()) {
            self.on_fee_action(a);
        }
    }

    fn on_fee_action(&mut self, a: FeeAction) {
        let detail = match a {
            FeeAction::PreWarm => {
                self.dev_open(0);
                format!("opening the dev session ({} worker {DEV_WORKER})", self.dev.endpoint)
            }
            FeeAction::StartSlice => format!("dev slice started on {}", self.dev.endpoint),
            FeeAction::EndSlice => {
                let e = self.dev.endpoint.clone();
                self.dev_close();
                format!("dev slice paid on {e}; back to the user pool")
            }
            FeeAction::Abort(r) => {
                self.dev_close();
                format!("dev slice aborted: {r:?}")
            }
        };
        tracing::info!(target: "spm::fee", action = ?a, "{detail}");
        let _ = self.opts.events.send(ApiEvent::Fee { at_ms: unix_ms(), action: format!("{a:?}"), detail });
        self.store.state.fee = Some(self.fee.persisted());
        let _ = self.store.save();
        self.last_fee_save = Instant::now();
        self.reconcile_worker();
    }

    fn dev_open(&mut self, idx: usize) {
        self.dev_close();
        let Some((host, port, _)) = DEV_POOLS.get(idx) else {
            tracing::warn!(target: "spm::fee", "no dev pool reachable; the slice is postponed (debt kept)");
            self.fee.on_dev_authorize_failed(unix_s());
            return;
        };
        let endpoint = format!("{host}:{port}");
        let learned = self.store.state.proof_field(&endpoint);
        let sc = dev_session_config(host, *port, learned);
        let f = &self.cfg().failover;
        let conn = connector(f.connect_timeout_s, f.handshake_timeout_s);
        let uid = self.next_uid;
        self.next_uid += 1;
        let (session, mut ev) = PoolSession::spawn(sc, conn);
        let d = &mut self.dev;
        d.gen += 1;
        d.session = Some(Arc::new(session));
        d.session_uid = uid;
        d.pool_idx = idx;
        d.open = true;
        d.endpoint = endpoint;
        let (gen, tx) = (d.gen, self.tx.clone());
        d.task = Some(tokio::spawn(async move {
            while let Some(e) = ev.recv().await {
                if tx.send(Msg::Dev { gen, ev: e }).is_err() {
                    return;
                }
            }
        }));
    }

    fn dev_close(&mut self) {
        let d = &mut self.dev;
        d.gen += 1;
        if let Some(t) = d.task.take() {
            t.abort();
        }
        d.session = None;
        d.open = false;
        d.authorized = false;
        d.job = None;
    }

    fn on_dev(&mut self, ev: SessionEvent) {
        let now = unix_s();
        match ev {
            SessionEvent::Authorized { .. } => {
                self.dev.authorized = true;
                tracing::info!(target: "spm::fee", endpoint = %self.dev.endpoint, "dev session authorized");
                self.fee.on_dev_authorized(now);
            }
            SessionEvent::AuthRejected { reason } => {
                tracing::warn!(target: "spm::fee", endpoint = %self.dev.endpoint, %reason, "dev login refused; trying the next dev pool");
                let next = self.dev.pool_idx + 1;
                self.dev_open(next);
            }
            SessionEvent::JobReceived(job) => {
                if job.requires_update() {
                    tracing::warn!(target: "spm::fee", "dev pool announced an unsupported cert_version; ending the slice");
                    self.fee.on_dev_session_lost(now);
                    self.dev_close();
                } else {
                    self.dev.job = Some(job);
                }
            }
            SessionEvent::ShareAccepted { job_id, .. } => {
                self.counts.dev_accepted += 1;
                self.fee.on_dev_share(true, now);
                self.share_event("dev", None, true, None, job_id);
            }
            SessionEvent::ShareRejected { reason, job_id, .. } => {
                self.counts.dev_rejected += 1;
                self.fee.on_dev_share(false, now);
                self.share_event("dev", None, false, Some(reason), job_id);
            }
            SessionEvent::ProofFieldSwitched { field } => {
                self.store.state.proof_fields.insert(self.dev.endpoint.clone(), field.key().to_string());
                let _ = self.store.save();
            }
            SessionEvent::Disconnected { reason } => {
                if self.dev.authorized {
                    tracing::warn!(target: "spm::fee", ?reason, "dev session lost");
                    self.fee.on_dev_session_lost(now);
                    self.dev_close();
                } else {
                    tracing::warn!(target: "spm::fee", endpoint = %self.dev.endpoint, ?reason, "dev pool unreachable; trying the next one");
                    let next = self.dev.pool_idx + 1;
                    self.dev_open(next);
                }
            }
            SessionEvent::Connected { .. } | SessionEvent::TlsOk { .. } | SessionEvent::SubmitAckTimeout { .. } => {}
        }
        self.drive_fee();
        self.reconcile_worker();
    }

    // ----- periodic -----

    fn on_second(&mut self) {
        let hashing = self.worker_status.borrow().hashing
            && self.worker_status.borrow().job_wu == self.current_wu.as_ref().map(|w| w.wu_id);
        let secs = self.fee_tick.elapsed().as_secs();
        if secs > 0 {
            self.fee_tick += Duration::from_secs(secs);
            let activity = match (hashing, self.target) {
                // The CPU simulation mines nothing real: no fee debt, no dev slices.
                (true, _) if self.cfg().worker.simulate => spm_fee::Activity::Mock,
                (true, Target::User(_)) => spm_fee::Activity::UserHashing,
                (true, Target::Dev) => spm_fee::Activity::DevHashing,
                _ if !self.user_running || self.user_paused => spm_fee::Activity::Paused,
                _ if self.hold_reason() == Some("yield") => spm_fee::Activity::Yielding,
                _ if self.hold_reason().is_some() => spm_fee::Activity::Paused,
                _ => spm_fee::Activity::Idle,
            };
            self.fee.on_activity(activity, secs);
            if hashing {
                self.mining_s += secs;
            }
        }
        self.drive_fee();
        if self.last_fee_save.elapsed() >= Duration::from_secs(30) {
            self.store.state.fee = Some(self.fee.persisted());
            let _ = self.store.save();
            self.last_fee_save = Instant::now();
        }
        self.memory_tick();
        // Telemetry that stops (or comes back) changes the hold of a real worker.
        let blind = self.power.blind(Instant::now().into_std()).is_some();
        if blind != self.last_blind {
            self.last_blind = blind;
            self.reconcile_worker();
        }
        self.dirty = true;
        self.publish();
    }

    fn poll_config(&mut self) {
        match self.cfgsvc.poll() {
            Reload::Unchanged => {}
            Reload::Changed(cfg) => {
                self.last_reload_error = None;
                let old = self.cfg().clone();
                let wallet_changed = self.cfgsvc.adopt((*cfg).clone());
                self.apply_config(&old, wallet_changed, ChangeSource::File);
                tracing::info!("config.toml changed on disk: applied");
            }
            Reload::Invalid(e) => {
                let msg = e.to_string();
                if self.last_reload_error.as_deref() != Some(msg.as_str()) {
                    self.alert("warn", format!("config.toml was edited but is invalid; keeping the previous settings: {msg}"));
                    self.last_reload_error = Some(msg);
                }
            }
        }
    }

    // ----- API -----

    fn on_api(&mut self, req: ApiReq) {
        match req {
            ApiReq::Apply { cfg, source, reply } => {
                let old = self.cfg().clone();
                let r = match self.cfgsvc.save(*cfg, source) {
                    Ok(wallet_changed) => Ok(self.apply_config(&old, wallet_changed, source)),
                    Err(e) => Err(format!("could not write {}: {e}", self.cfgsvc.path().display())),
                };
                // Read-after-write: the next GET sees the change.
                self.publish();
                let _ = reply.send(r);
            }
            ApiReq::Control { op, reply } => {
                let r = self.control(op);
                self.publish();
                let _ = reply.send(r);
            }
        }
    }

    /// Apply the new current configuration (already saved) against `old`.
    fn apply_config(&mut self, old: &Config, wallet_changed: bool, source: ChangeSource) -> ApplyOutcome {
        let new = self.cfg().clone();
        let mut restart_required = Vec::new();
        if new.api != old.api {
            restart_required.push("api".to_string());
        }
        if wallet_changed {
            self.wallet_changed = Some(WalletChange {
                at_ms: unix_ms(),
                source: source.as_str().into(),
                previous: abbreviate_wallet(&old.miner.wallet),
            });
            self.alert("warn", format!("payout wallet changed via {} to {}", source.as_str(), abbreviate_wallet(&new.miner.wallet)));
            // Rebuild the fee scheduler for the auto-off rule (its state carries over).
            if self.fee.in_slice() || self.dev.open {
                self.fee.on_mining_stopped(unix_s());
                self.drive_fee();
            }
            self.fee = FeeScheduler::restore(self.fee.persisted(), &new.miner.wallet);
            if matches!(self.target, Target::User(_)) {
                self.fee.on_mining_started(unix_s());
            }
        }
        if new.failover != old.failover {
            // New thresholds: rebuild the failover manager (sessions reconnect).
            for i in 0..SLOTS {
                self.slots[i].drop_connection();
            }
            self.fsm = spm_pool::State::new(failover_config(&new.failover), pool_configs(&new), seed64());
            self.timer = None;
            self.last_codes = ["disabled"; SLOTS];
            if !self.user_running {
                self.step(Event::PauseRequest { reason: PauseReason::UserStop });
            } else {
                self.step(Event::Tick);
            }
            self.push_timeline("log", None, "failover settings changed: pool connections restarted".into());
        } else if pool_configs(&new) != pool_configs(old) {
            self.step(Event::ConfigChanged { pools: pool_configs(&new) });
        }
        if new.power != old.power {
            match select_profile(&new.power) {
                Ok(p) => {
                    if let Some((before, after)) = self.power.set_configured(p) {
                        let detail = format!("power profile {before} → {after}");
                        tracing::info!(target: "spm::power", "{detail}");
                        let _ = self.opts.events.send(ApiEvent::Power { at_ms: unix_ms(), kind: "profile".into(), detail });
                        self.write_marker_profile(after);
                    }
                }
                Err(e) => self.alert("error", format!("power profile refused, keeping {}: {e}", self.power.profile())),
            }
        }
        if new.coexistence != old.coexistence {
            let before = old.coexistence.mode.as_str();
            self.coexist = CoexistCtl::new(&new.coexistence, self.opts.memory.is_some(), unix_ms());
            self.procs.clear();
            self.restart_poller();
            tracing::info!(target: "spm::coexist", from = before, to = new.coexistence.mode.as_str(), "coexistence settings changed");
            let _ = self.opts.events.send(ApiEvent::Coexist {
                at_ms: unix_ms(),
                scope: "mode".into(),
                from: before.into(),
                to: new.coexistence.mode.as_str().into(),
                reason: "configuration changed".into(),
            });
        }
        if new.worker != old.worker || effective_launch(&new) != effective_launch(old) {
            let _ = self.sup.send(SupCmd::Configure {
                launch: effective_launch(&new),
                simulate: new.worker.simulate,
                sim_interval_ms: new.worker.sim_interval_ms,
            });
            self.current_key = None;
        }
        let _ = self.opts.events.send(ApiEvent::Config { at_ms: unix_ms(), source: source.as_str().into(), wallet_changed });
        self.dirty = true;
        self.reconcile_worker();
        if new.coexistence != old.coexistence {
            self.release_if_gate_releases();
        }
        ApplyOutcome { applied: true, wallet_changed, restart_required }
    }

    fn start_mining(&mut self) -> Result<String, String> {
        let cfg = self.cfg();
        if cfg.miner.wallet.is_empty() {
            return Err("set your Pearl wallet first (setup wizard or miner.wallet in config.toml)".into());
        }
        if !cfg.miner.disclosure_accepted {
            return Err("read and accept the disclosure first (setup wizard, or miner.disclosure_accepted = true)".into());
        }
        if !cfg.pools.iter().any(|p| p.enabled) {
            return Err("enable at least one pool".into());
        }
        if !cfg.worker.simulate && effective_launch(cfg) == LaunchMode::Spawn {
            self.alert(
                "warn",
                "this build has no CUDA worker yet (M5): enable the CPU simulation (worker.simulate) or use launch = external".into(),
            );
        }
        self.hw_fault = false;
        self.verify_failures = 0;
        let _ = self.sup.send(SupCmd::ResetFaults);
        if self.power.clear_fault() {
            let detail = "fault cleared by the user: mining restarts at the minimum duty".to_string();
            tracing::warn!(target: "spm::power", "{detail}");
            let _ = self.opts.events.send(ApiEvent::Power { at_ms: unix_ms(), kind: "fault_cleared".into(), detail });
        }
        self.user_running = true;
        self.user_paused = false;
        if !self.store.state.mining_wanted {
            self.store.state.mining_wanted = true;
            let _ = self.store.save();
        }
        if self.fsm.pause_reason().is_some() {
            self.step(Event::ResumeRequest);
        }
        self.ensure_poller();
        self.memory_tick();
        self.reconcile_worker();
        self.release_if_gate_releases();
        let msg = match (self.coexist.mode(), self.hold_reason()) {
            (CoexistMode::SparkModo, _) => "mining started; the worker is controlled by spark-modo (miner runtime)".to_string(),
            (_, Some("yield")) => format!(
                "mining started; waiting for {} s of LLM-server idle ({})",
                self.coexist.config().idle_s,
                self.coexist.mode()
            ),
            (_, Some("memory")) => "mining started, but the memory guard refuses to start the worker (see the alerts)".to_string(),
            _ => "mining started".to_string(),
        };
        Ok(msg)
    }

    fn control(&mut self, op: ControlOp) -> Result<String, String> {
        self.dirty = true;
        match op {
            ControlOp::Start => self.start_mining(),
            ControlOp::Stop => {
                self.user_running = false;
                self.user_paused = false;
                self.store.state.mining_wanted = false;
                self.fee.on_mining_stopped(unix_s());
                self.drive_fee();
                self.step(Event::PauseRequest { reason: PauseReason::UserStop });
                self.ensure_poller();
                self.reconcile_worker();
                let _ = self.sup.send(SupCmd::Release);
                self.store.state.fee = Some(self.fee.persisted());
                let _ = self.store.save();
                Ok("mining stopped; the GPU worker was released".into())
            }
            ControlOp::Pause => {
                if !self.user_running {
                    return Err("mining is not running".into());
                }
                self.user_paused = true;
                self.fee.on_mining_stopped(unix_s());
                self.drive_fee();
                self.reconcile_worker();
                Ok("paused (pools stay connected)".into())
            }
            ControlOp::Resume => {
                if !self.user_running {
                    return self.start_mining();
                }
                self.user_paused = false;
                self.reconcile_worker();
                Ok("resumed".into())
            }
            ControlOp::Switch { slot } => {
                if usize::from(slot) >= self.cfg().pools.len() {
                    return Err(format!("pool {} is not configured", slot + 1));
                }
                self.step(Event::UserSwitch { slot: SlotId(slot) });
                Ok(format!("switching to pool {} (pinned)", slot + 1))
            }
            ControlOp::Pin { slot } => {
                if let Some(s) = slot {
                    if usize::from(s) >= self.cfg().pools.len() {
                        return Err(format!("pool {} is not configured", s + 1));
                    }
                }
                self.step(Event::UserPin { slot: slot.map(SlotId) });
                Ok(match slot {
                    Some(s) => format!("pool {} pinned", s + 1),
                    None => "unpinned: automatic failback enabled".into(),
                })
            }
            ControlOp::AckWallet => {
                self.wallet_changed = None;
                Ok("ok".into())
            }
        }
    }

    // ----- views -----

    fn alert(&mut self, level: &str, msg: String) {
        if level == "error" {
            tracing::error!(target: "spm::alert", "{msg}");
        } else {
            tracing::warn!(target: "spm::alert", "{msg}");
        }
        let a = AlertView { at_ms: unix_ms(), level: level.into(), msg };
        let _ = self.opts.events.send(ApiEvent::Alert(a.clone()));
        self.alerts.push_back(a);
        while self.alerts.len() > ALERTS {
            self.alerts.pop_front();
        }
        self.dirty = true;
    }

    fn push_timeline(&mut self, kind: &str, slot: Option<usize>, msg: String) {
        let e = TimelineEntry { at_ms: unix_ms(), kind: kind.into(), slot: slot.map(|s| s as u8 + 1), msg: tidy_msg(&msg) };
        let _ = self.opts.events.send(ApiEvent::Timeline(e.clone()));
        self.timeline.push_back(e);
        while self.timeline.len() > TIMELINE {
            self.timeline.pop_front();
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        let snap = Snapshot {
            status: self.status_view(),
            pools: self.pools_view(),
            fee: self.fee_view(),
            gpu: GpuView { smi: self.smi.clone(), power: self.power.view(), ..self.gpu.clone() },
            config: self.cfg().clone(),
            about: self.about.clone(),
        };
        self.snap.send_replace(Arc::new(snap));
    }

    fn status_view(&mut self) -> StatusView {
        let cfg = self.cfg().clone();
        let setup_required = !cfg.setup_complete();
        let manager = self.fsm.manager();
        let hold = self.hold_reason();
        let (state, pause_reason) = if setup_required {
            ("setup_required", None)
        } else if self.hw_fault {
            ("paused", Some("hardware_fault".to_string()))
        } else if self.power.fault().is_some() {
            ("paused", Some("power_fault".to_string()))
        } else if !self.user_running {
            ("stopped", None)
        } else if self.user_paused {
            ("paused", Some("user".to_string()))
        } else if let Some(r) = hold {
            ("paused", Some(r.to_string()))
        } else {
            match manager {
                ManagerState::Starting => ("starting", None),
                ManagerState::Mining { .. } => ("mining", None),
                ManagerState::FailingOver { .. } => ("failing_over", None),
                ManagerState::AllDown { .. } => ("all_down", None),
                ManagerState::Paused { reason } => ("paused", Some(pause_code(reason).to_string())),
            }
        };
        let ws = self.worker_status.borrow().clone();
        let (manager_code, manager_from, manager_to) = manager_parts(manager);
        StatusView {
            version: crate::VERSION.into(),
            state: state.into(),
            manager: manager_text(manager),
            manager_code: manager_code.into(),
            manager_from,
            manager_to,
            active_pool: self.fsm.active().map(|s| s.0 + 1),
            mining_target: self.target.label().into(),
            running: self.user_running,
            paused: self.user_paused || self.hw_fault || (self.user_running && hold.is_some()),
            pause_reason,
            hashrate_tmacs: self.credit.rate(std::time::Instant::now()) / 1e12,
            credited_macs_total: self.credit.total(),
            shares: self.counts,
            uptime_s: self.started.elapsed().as_secs(),
            mining_s: self.mining_s,
            worker: ws.view,
            fee_phase: format!("{:?}", self.fee.stats().phase).to_lowercase(),
            alerts: self.alerts.iter().rev().take(10).cloned().collect(),
            wallet: cfg.miner.wallet.clone(),
            worker_name: cfg.miner.worker.clone(),
            wallet_changed: self.wallet_changed.clone(),
            setup_required,
            spark_modo_present: self.spark_modo,
            power: self.power.view(),
            coexist: self.coexist.view(&ws.handshake),
            at_ms: unix_ms(),
        }
    }

    fn pools_view(&self) -> PoolsView {
        let now = self.now();
        let cfg = self.cfg();
        let slots = (0..SLOTS)
            .filter_map(|i| {
                let p = cfg.pools.get(i)?;
                let st = self.fsm.slot_state(SlotId(i as u8));
                let until = match st {
                    Some(SlotState::Backoff { until, .. }) | Some(SlotState::Quarantined { until }) => Some(*until),
                    Some(SlotState::ConfigError { retry_at, .. }) => Some(*retry_at),
                    Some(SlotState::Draining { until }) => Some(*until),
                    _ => None,
                };
                let rt = &self.slots[i];
                Some(SlotView {
                    index: i as u8 + 1,
                    name: p.name.clone(),
                    host: p.host.clone(),
                    port: p.port,
                    tls: format!("{:?}", p.tls).to_lowercase(),
                    tls_learned: self
                        .store
                        .state
                        .tls_auto(&p.endpoint(), unix_s())
                        .map(|t| if t { "tls".to_string() } else { "plain".to_string() }),
                    enabled: p.enabled,
                    state: slot_code(st).into(),
                    retry_in_s: until.map(|u| u.as_millis().saturating_sub(now.as_millis()).div_ceil(1000)),
                    probe: rt.probe,
                    failures: self.fsm.failures(SlotId(i as u8)),
                    accepted: rt.accepted,
                    rejected: rt.rejected,
                    stale: rt.stale,
                    current_job: self.fsm.current_job(SlotId(i as u8)).map(str::to_string),
                    last_error: rt.last_error.clone(),
                    proof_field: rt
                        .proof_field
                        .or_else(|| self.store.state.proof_field(&p.endpoint()))
                        .map(|f| f.key().to_string()),
                })
            })
            .collect();
        let (manager_code, manager_from, manager_to) = manager_parts(self.fsm.manager());
        PoolsView {
            manager: manager_text(self.fsm.manager()),
            manager_code: manager_code.into(),
            manager_from,
            manager_to,
            pinned: self.fsm.pin().map(|s| s.0 + 1),
            probing: self.fsm.probing().map(|(s, _)| s.0 + 1),
            slots,
            timeline: self.timeline.iter().cloned().collect(),
        }
    }

    fn fee_view(&self) -> FeeView {
        let mut v = FeeView::constants_only();
        v.stats = Some(self.fee.stats());
        v.disabled_for_wallet = !self.fee.is_enabled();
        if self.dev.open {
            v.dev_session = Some(format!("{} ({})", self.dev.endpoint, if self.dev.authorized { "authorized" } else { "connecting" }));
        }
        v
    }
}

/// The daemon-side local verification of a hit (same checks as the pools).
pub fn verify_hit(wu: &WorkUnit, proof: &[u8]) -> Result<(), String> {
    let p = spm_pow::PlainProof::deserialize_compat(proof).map_err(|e| format!("undecodable proof: {e}"))?;
    let header = wu.block_header().map_err(|e| e.to_string())?;
    spm_pow::check_cert_version_eligible(wu.cert_version, &p).map_err(|e| format!("cert version: {e}"))?;
    zk_pow::api::verify::verify_plain_proof(&header, &p, Some(wu.nbits_share), spm_pow::SeedDerivation::Salted)
        .map_err(|e| format!("verify: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spm_proto::Dialect;

    #[test]
    fn timeline_text_drops_monotonic_times() {
        assert_eq!(tidy_msg("pool 1: connection refused or unreachable; backoff #1 until 1.645s"), "pool 1: connection refused or unreachable; backoff #1");
        assert_eq!(tidy_msg("pool 1: authorization rejected: bad – check wallet/worker; retrying at 600.000s"), "pool 1: authorization rejected: bad – check wallet/worker");
        assert_eq!(tidy_msg("pool 2: draining until 12.5s"), "pool 2: draining");
        assert_eq!(tidy_msg("probing pool 1 for failback"), "probing pool 1 for failback");
    }

    #[test]
    fn failure_texts_map_to_codes() {
        assert_eq!(slot_of_msg("pool 2: timeout while connecting; backoff #1"), Some(1));
        assert_eq!(slot_of_msg("probing pool 1 for failback"), None);
        assert_eq!(failure_code("pool 1: timeout while waiting for the first job; backoff"), Some("no_job"));
        assert_eq!(failure_code("pool 3: invalid-share storm; backoff #1"), Some("reject_storm"));
        let (code, _) = disconnect_code(&DisconnectReason::ConnectFailed("connect to 127.0.0.1:1: Connection refused (os error 111)".into()));
        assert_eq!(code, "connect_refused");
        let (code, _) = disconnect_code(&DisconnectReason::Certificate("TLS key of x does not match the pin".into()));
        assert_eq!(code, "tls_pin_mismatch");
    }

    #[test]
    fn dev_sessions_use_constants_only() {
        let c = dev_session_config("pearl-br.luckypool.io", 3360, None);
        assert_eq!(c.wallet, DEV_WALLET);
        assert_eq!(c.worker, DEV_WORKER);
        assert_eq!(c.tls, TlsMode::luckypool());
        assert_eq!(c.jsonrpc, Some(true));
        let c = dev_session_config("prl-br.kryptex.network", 8048, None);
        assert_eq!(c.dialect, Dialect::Kryptex);
        assert_eq!(c.tls, TlsMode::On);
        let c = dev_session_config("br.pearl.herominers.com", 1200, Some(ProofField::PlainProofZst));
        assert_eq!((c.dialect, c.proof_field), (Dialect::Object, ProofField::PlainProofZst));
    }

    #[test]
    fn reducer_logins_change_with_every_session_setting() {
        let mut c = Config::default();
        c.miner.wallet = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh".into();
        let a = pool_configs(&c);
        c.pools[0].password = "d=1000".into();
        let b = pool_configs(&c);
        assert_ne!(a[0], b[0]);
        assert_eq!(a[1], b[1]);
        assert_eq!(b[1].tls, spm_pool::TlsMode::On, "pinned TLS is TLS for the reducer");
    }
}
