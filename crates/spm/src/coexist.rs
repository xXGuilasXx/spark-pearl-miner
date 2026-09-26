//! The coexistence task: `coexistence.mode` in the daemon.
//!
//! * `exclusive`: no gating.
//! * `yield` / `yield-release`: a poller fetches vLLM's `/metrics` every `poll_ms` and the
//!   [`YieldGate`] decides; when the metrics are unavailable, the SM utilization of the *other*
//!   compute processes (NVML, or `nvidia-smi pmon`, from the telemetry thread) is the signal,
//!   and with neither the GPU counts as busy. A busy LLM server pauses the worker (context kept)
//!   or releases it (the process exits); `idle_s` of continuous idle lets it run again (a
//!   released worker is spawned again).
//! * `spark-modo`: the `miner` runtime of spark-modo starts and stops the worker; the daemon never
//!   spawns it and only reports.
//! * The memory guard applies in every mode: no worker start without 20 GiB of headroom after
//!   its 2 GiB budget, and a running worker is released below 16 GiB available or above 10 %
//!   memory pressure; it may start again only once the start condition holds.
//!
//! Like `crate::power`, [`CoexistCtl`] owns no clock and does no I/O except through what the
//! daemon hands it; the daemon applies the transitions it reports.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use spm_api::config::{CoexistenceConfig, CoexistenceMode};
use spm_api::{CoexistView, HandshakeView, MemoryView};
use spm_coexist::http::{fetch_vllm_load, HttpUrl};
use spm_coexist::memguard::{self, MemSnapshot, MemVerdict, GIB, WORKER_BUDGET_BYTES};
use spm_coexist::{CoexistConfig, CoexistMode, GateDecision, LoadSignal, VllmLoad, YieldGate, METRICS_TIMEOUT};
use tokio::task::JoinHandle;

use crate::supervisor::HandshakeInfo;

/// Where the memory guard reads (injectable for tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub meminfo: PathBuf,
    pub psi: PathBuf,
}

impl MemorySource {
    /// `/proc/meminfo` and `/proc/pressure/memory`.
    pub fn proc() -> Self {
        MemorySource { meminfo: PathBuf::from(memguard::MEMINFO_PATH), psi: PathBuf::from(memguard::PSI_MEMORY_PATH) }
    }

    pub fn read(&self) -> io::Result<MemSnapshot> {
        memguard::read_snapshot(&self.meminfo, &self.psi)
    }
}

/// The spm-coexist mode of a config value.
pub fn mode_of(m: CoexistenceMode) -> CoexistMode {
    match m {
        CoexistenceMode::SparkModo => CoexistMode::SparkModo,
        CoexistenceMode::Yield => CoexistMode::Yield,
        CoexistenceMode::YieldRelease => CoexistMode::YieldRelease,
        CoexistenceMode::Exclusive => CoexistMode::Exclusive,
    }
}

/// The spm-coexist settings of a config section.
pub fn settings_of(c: &CoexistenceConfig) -> CoexistConfig {
    CoexistConfig {
        mode: mode_of(c.mode),
        metrics_url: c.metrics_url.clone(),
        poll: Duration::from_millis(c.poll_ms),
        idle: Duration::from_secs(c.idle_s),
        busy_sm_pct: c.busy_sm_pct,
    }
}

fn gate_text(d: GateDecision) -> &'static str {
    match d {
        GateDecision::Mine => "mine",
        GateDecision::Pause => "pause",
        GateDecision::Release => "release",
        GateDecision::External => "external",
    }
}

/// How the gate holds the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateHold {
    None,
    /// Paused, CUDA context kept.
    Pause,
    /// Released: the process exits and is spawned again when the gate opens.
    Release,
}

/// A gate or memory transition for the daemon to apply and log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// `gate` | `memory`
    pub scope: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub reason: String,
}

/// Memory-guard state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemState {
    Off,
    Ok,
    /// The start check failed: the worker may not start.
    Refused,
    ExitLowMemory,
    ExitPressure,
    /// `MemAvailable` could not be read: no start.
    Unreadable,
}

impl MemState {
    pub fn as_str(self) -> &'static str {
        match self {
            MemState::Off => "off",
            MemState::Ok => "ok",
            MemState::Refused => "refused",
            MemState::ExitLowMemory => "exit_low_memory",
            MemState::ExitPressure => "exit_pressure",
            MemState::Unreadable => "unreadable",
        }
    }

    /// The worker must not run.
    pub fn holds(self) -> bool {
        !matches!(self, MemState::Off | MemState::Ok)
    }
}

/// What a memory check asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemOutcome {
    pub transition: Option<Transition>,
    /// Release the worker now (it is running short of memory).
    pub release: bool,
}

/// The daemon's side of the coexistence rules.
#[derive(Debug)]
pub struct CoexistCtl {
    cfg: CoexistenceConfig,
    mode: CoexistMode,
    gate: YieldGate,
    decision: GateDecision,
    since_ms: u64,
    transitions: u64,
    signal: &'static str,
    llm: Option<VllmLoad>,
    foreign: Option<u32>,
    metrics_error: Option<String>,
    idle_since: Option<Duration>,
    last_now: Duration,
    mem: MemState,
    mem_snapshot: Option<MemSnapshot>,
    mem_detail: Option<String>,
}

impl CoexistCtl {
    /// `memory_guard`: whether the guard runs (off only in tests without fixtures).
    pub fn new(cfg: &CoexistenceConfig, memory_guard: bool, now_ms: u64) -> Self {
        let s = settings_of(cfg);
        let gate = YieldGate::new(&s);
        CoexistCtl {
            cfg: cfg.clone(),
            mode: s.mode,
            decision: gate.decision(),
            gate,
            since_ms: now_ms,
            transitions: 0,
            signal: "none",
            llm: None,
            foreign: None,
            metrics_error: None,
            idle_since: None,
            last_now: Duration::ZERO,
            mem: if memory_guard { MemState::Ok } else { MemState::Off },
            mem_snapshot: None,
            mem_detail: None,
        }
    }

    pub fn mode(&self) -> CoexistMode {
        self.mode
    }

    pub fn config(&self) -> &CoexistenceConfig {
        &self.cfg
    }

    /// The metrics URL, when this mode polls one.
    pub fn poll_target(&self) -> Option<(HttpUrl, Duration)> {
        if !self.mode.polls_llm() {
            return None;
        }
        let url = HttpUrl::parse(&self.cfg.metrics_url).ok()?;
        Some((url, settings_of(&self.cfg).poll_interval()))
    }

    pub fn decision(&self) -> GateDecision {
        self.decision
    }

    pub fn gate_hold(&self) -> GateHold {
        match self.decision {
            GateDecision::Pause => GateHold::Pause,
            GateDecision::Release => GateHold::Release,
            GateDecision::Mine | GateDecision::External => GateHold::None,
        }
    }

    pub fn mem_state(&self) -> MemState {
        self.mem
    }

    /// One poll result at `now` (daemon clock). `fallback`: SM utilization of the other compute
    /// processes and where it came from, when fresh. Returns the gate transition, if any.
    pub fn on_poll(
        &mut self,
        now: Duration,
        now_ms: u64,
        metrics: Result<VllmLoad, String>,
        fallback: Option<(u32, &'static str)>,
    ) -> Option<Transition> {
        self.last_now = now;
        let signal = match metrics {
            Ok(load) => {
                self.metrics_error = None;
                self.llm = Some(load);
                self.foreign = None;
                self.signal = "vllm";
                LoadSignal::Vllm(load)
            }
            Err(e) => {
                self.metrics_error = Some(e);
                self.llm = None;
                match fallback {
                    Some((pct, source)) => {
                        self.foreign = Some(pct);
                        self.signal = source;
                        LoadSignal::ForeignSmUtil { pct }
                    }
                    None => {
                        self.foreign = None;
                        self.signal = "unavailable";
                        LoadSignal::Unavailable
                    }
                }
            }
        };
        let busy = self.gate.is_busy(signal);
        if busy {
            self.idle_since = None;
        } else {
            self.idle_since.get_or_insert(now);
        }
        let before = self.decision;
        let after = self.gate.observe(now, signal);
        self.decision = after;
        if after == before {
            return None;
        }
        self.transitions += 1;
        self.since_ms = now_ms;
        let reason = match signal {
            _ if after == GateDecision::Mine => format!("the LLM server has been idle for {} s", self.cfg.idle_s),
            LoadSignal::Vllm(l) => format!("vLLM busy ({} running, {} waiting)", l.running, l.waiting),
            LoadSignal::ForeignSmUtil { pct } => {
                format!("metrics unavailable; another compute process at {pct} % SM (≥ {} %)", self.cfg.busy_sm_pct)
            }
            LoadSignal::Unavailable => "no load signal (metrics and per-process utilization unavailable): the GPU counts as busy".into(),
        };
        Some(Transition { scope: "gate", from: gate_text(before), to: gate_text(after), reason })
    }

    /// Stop polling (mining stopped): the next start needs `idle_s` of quiet again.
    pub fn reset_gate(&mut self, now_ms: u64) {
        let s = settings_of(&self.cfg);
        self.gate = YieldGate::new(&s);
        if self.decision != self.gate.decision() {
            self.transitions += 1;
            self.since_ms = now_ms;
        }
        self.decision = self.gate.decision();
        self.idle_since = None;
        self.llm = None;
        self.foreign = None;
        self.metrics_error = None;
        self.signal = "none";
    }

    /// One memory check. `worker_present`: a worker process exists (its budget is allocated);
    /// `wants_run`: the daemon would run it now.
    pub fn on_memory(&mut self, snap: io::Result<MemSnapshot>, worker_present: bool, wants_run: bool) -> MemOutcome {
        if self.mem == MemState::Off {
            return MemOutcome::default();
        }
        let before = self.mem;
        let mut release = false;
        match snap {
            Err(e) => {
                self.mem_snapshot = None;
                if !before.holds() && !worker_present && wants_run {
                    self.mem = MemState::Unreadable;
                    self.mem_detail = Some(format!("MemAvailable unreadable ({e}): the worker is not started"));
                }
            }
            Ok(m) => {
                self.mem_snapshot = Some(m);
                let start = memguard::check_start(&m, WORKER_BUDGET_BYTES);
                let running = memguard::check_running(&m);
                if worker_present && running.must_exit() {
                    release = true;
                    self.mem = match running {
                        MemVerdict::ExitPressure { .. } => MemState::ExitPressure,
                        _ => MemState::ExitLowMemory,
                    };
                    self.mem_detail = Some(match running {
                        MemVerdict::ExitLowMemory { available_bytes } => format!(
                            "only {:.1} GiB available (below {} GiB): releasing the worker",
                            available_bytes as f64 / GIB as f64,
                            memguard::EXIT_BELOW_AVAILABLE_BYTES / GIB
                        ),
                        MemVerdict::ExitPressure { some_avg10 } => format!(
                            "memory pressure {some_avg10:.1} % (above {} %): releasing the worker",
                            memguard::EXIT_ABOVE_PSI_SOME_AVG10
                        ),
                        MemVerdict::Ok => String::new(),
                    });
                } else if before.holds() {
                    // Hysteresis: only the start condition lifts a hold (and not while the
                    // pressure would make the worker exit again at once).
                    if start.is_ok() && !running.must_exit() {
                        self.mem = MemState::Ok;
                        self.mem_detail = None;
                    }
                } else if !worker_present && wants_run {
                    if let Err(r) = start {
                        self.mem = MemState::Refused;
                        self.mem_detail = Some(r.to_string());
                    } else if let MemVerdict::ExitPressure { some_avg10 } = running {
                        self.mem = MemState::Refused;
                        self.mem_detail = Some(format!(
                            "not starting: memory pressure {some_avg10:.1} % (above {} %)",
                            memguard::EXIT_ABOVE_PSI_SOME_AVG10
                        ));
                    }
                }
            }
        }
        let transition = (self.mem != before).then(|| Transition {
            scope: "memory",
            from: before.as_str(),
            to: self.mem.as_str(),
            reason: self.mem_detail.clone().unwrap_or_else(|| "enough memory again".into()),
        });
        MemOutcome { transition, release }
    }

    pub fn view(&self, hs: &HandshakeInfo) -> CoexistView {
        let idle_for_s = match self.mode {
            CoexistMode::Yield | CoexistMode::YieldRelease => {
                self.idle_since.map(|t| self.last_now.saturating_sub(t).as_secs_f64())
            }
            _ => None,
        };
        let spark_modo = self.mode == CoexistMode::SparkModo;
        CoexistView {
            mode: self.cfg.mode.as_str().into(),
            gate: gate_text(self.decision).into(),
            controlled_by: spark_modo.then(|| "spark-modo".to_string()),
            signal: self.signal.into(),
            llm_running: self.llm.map(|l| l.running),
            llm_waiting: self.llm.map(|l| l.waiting),
            foreign_sm_pct: self.foreign,
            metrics_error: self.metrics_error.clone(),
            idle_for_s,
            idle_needed_s: self.cfg.idle_s,
            transitions: self.transitions,
            since_ms: self.since_ms,
            memory: MemoryView {
                state: self.mem.as_str().into(),
                available_gib: self.mem_snapshot.map(|m| m.available_bytes as f64 / GIB as f64),
                psi_some_avg10: self.mem_snapshot.and_then(|m| m.psi_some_avg10),
                detail: self.mem_detail.clone(),
            },
            handshake: HandshakeView {
                last_ack: hs.last_ack.clone(),
                acks: hs.acks,
                pause_latency_ms: hs.pause_latency_ms,
                resume_latency_ms: hs.resume_latency_ms,
                escalations: hs.escalations,
            },
        }
    }
}

/// Polls the vLLM metrics every `poll` and hands each result to `sink` (which returns `false`
/// once the daemon is gone). The daemon aborts the task when the mode or the URL changes.
pub fn spawn_poller(
    url: HttpUrl,
    poll: Duration,
    sink: impl Fn(Result<VllmLoad, String>) -> bool + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let r = fetch_vllm_load(&url, METRICS_TIMEOUT).await.map_err(|e| e.to_string());
            if !sink(r) {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: CoexistenceMode) -> CoexistenceConfig {
        CoexistenceConfig { mode, idle_s: 1, ..CoexistenceConfig::default() }
    }

    const IDLE: VllmLoad = VllmLoad { running: 0.0, waiting: 0.0 };
    const BUSY: VllmLoad = VllmLoad { running: 2.0, waiting: 1.0 };

    fn ms(t: u64) -> Duration {
        Duration::from_millis(t)
    }

    fn snap(gib: u64, psi: Option<f64>) -> io::Result<MemSnapshot> {
        Ok(MemSnapshot { available_bytes: gib * GIB, psi_some_avg10: psi })
    }

    #[test]
    fn yield_gate_transitions_and_fallbacks() {
        let mut c = CoexistCtl::new(&cfg(CoexistenceMode::Yield), true, 0);
        assert_eq!(c.gate_hold(), GateHold::Pause);
        assert_eq!(c.on_poll(ms(0), 1, Ok(IDLE), None), None);
        let t = c.on_poll(ms(1_000), 2, Ok(IDLE), None).unwrap();
        assert_eq!((t.from, t.to), ("pause", "mine"));
        assert_eq!(c.gate_hold(), GateHold::None);
        let t = c.on_poll(ms(1_100), 3, Ok(BUSY), None).unwrap();
        assert_eq!((t.from, t.to), ("mine", "pause"));
        assert!(t.reason.contains("2 running"), "{}", t.reason);
        // Metrics gone: the NVML fallback decides; nothing at all counts as busy.
        assert_eq!(c.on_poll(ms(1_200), 4, Err("refused".into()), Some((3, "nvml"))), None);
        let t = c.on_poll(ms(2_200), 5, Err("refused".into()), Some((3, "nvml"))).unwrap();
        assert_eq!(t.to, "mine");
        let v = c.view(&HandshakeInfo::default());
        assert_eq!((v.signal.as_str(), v.foreign_sm_pct, v.metrics_error.as_deref()), ("nvml", Some(3), Some("refused")));
        let t = c.on_poll(ms(2_300), 6, Err("refused".into()), None).unwrap();
        assert_eq!(t.to, "pause");
        assert!(t.reason.contains("counts as busy"));
        assert_eq!(c.view(&HandshakeInfo::default()).transitions, 4);
        c.reset_gate(7);
        assert_eq!(c.gate_hold(), GateHold::Pause);
    }

    #[test]
    fn modes_without_polling() {
        let c = CoexistCtl::new(&cfg(CoexistenceMode::Exclusive), true, 0);
        assert_eq!((c.gate_hold(), c.poll_target().is_none()), (GateHold::None, true));
        let c = CoexistCtl::new(&cfg(CoexistenceMode::SparkModo), true, 0);
        let v = c.view(&HandshakeInfo::default());
        assert_eq!((v.gate.as_str(), v.controlled_by.as_deref()), ("external", Some("spark-modo")));
        let c = CoexistCtl::new(&cfg(CoexistenceMode::YieldRelease), true, 0);
        assert_eq!(c.gate_hold(), GateHold::Release);
        let (url, poll) = c.poll_target().unwrap();
        assert_eq!((url.port, poll), (8001, Duration::from_millis(200)));
    }

    #[test]
    fn memory_guard_refuses_releases_and_needs_the_start_condition_back() {
        let mut c = CoexistCtl::new(&CoexistenceConfig::default(), true, 0);
        // Not wanted: no verdict.
        assert_eq!(c.on_memory(snap(10, None), false, false), MemOutcome::default());
        // 21 GiB − 2 GiB budget < 20 GiB: refused.
        let o = c.on_memory(snap(21, None), false, true);
        assert_eq!(o.transition.as_ref().map(|t| (t.from, t.to)), Some(("ok", "refused")));
        assert!(!o.release && c.mem_state().holds());
        assert!(o.transition.unwrap().reason.contains("20 GiB"));
        let o = c.on_memory(snap(22, None), false, true);
        assert_eq!(o.transition.map(|t| t.to), Some("ok"));
        // Running: pressure releases; 18 GiB (between 16 and 22) does not lift the hold.
        let o = c.on_memory(snap(40, Some(12.5)), true, true);
        assert!(o.release);
        assert_eq!(o.transition.map(|t| t.to), Some("exit_pressure"));
        let o = c.on_memory(snap(18, Some(0.0)), false, true);
        assert_eq!(o, MemOutcome::default());
        assert!(c.mem_state().holds());
        // Headroom back but still under pressure: no restart.
        assert_eq!(c.on_memory(snap(40, Some(12.0)), false, true), MemOutcome::default());
        let o = c.on_memory(snap(30, Some(0.0)), false, true);
        assert_eq!(o.transition.map(|t| t.to), Some("ok"));
        let o = c.on_memory(snap(15, None), true, true);
        assert_eq!((o.release, o.transition.map(|t| t.to)), (true, Some("exit_low_memory")));
        // A start under pressure is refused too (the worker would exit at once).
        let mut c = CoexistCtl::new(&CoexistenceConfig::default(), true, 0);
        let o = c.on_memory(snap(64, Some(30.0)), false, true);
        assert_eq!(o.transition.as_ref().map(|t| t.to), Some("refused"));
        assert!(o.transition.unwrap().reason.contains("pressure"));
        // Unreadable meminfo refuses a start.
        let mut c = CoexistCtl::new(&CoexistenceConfig::default(), true, 0);
        let o = c.on_memory(Err(io::Error::other("gone")), false, true);
        assert_eq!(o.transition.map(|t| t.to), Some("unreadable"));
        let mut off = CoexistCtl::new(&CoexistenceConfig::default(), false, 0);
        assert_eq!(off.on_memory(snap(1, None), false, true), MemOutcome::default());
        assert_eq!(off.view(&HandshakeInfo::default()).memory.state, "off");
    }
}
