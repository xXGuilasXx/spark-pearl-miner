//! The power governor task: telemetry sources, the sampling thread and the adapter between
//! `spm_governor::Governor` and the daemon.
//!
//! * Telemetry: NVML at 10 Hz (cargo feature `nvml`, on by default; `libnvidia-ml.so` is loaded
//!   at runtime and only read-only queries are made, so no CUDA context). When NVML cannot be
//!   loaded, `nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active`
//!   at 2 Hz. Both add the hottest `acpitz` zone. The readings come from a dedicated thread (the
//!   calls block) and reach the daemon loop as messages.
//! * [`PowerCtl`] stamps nothing and owns no clock: the daemon hands it [`Sample`]s (with
//!   `worker_active` from the supervisor) and applies the [`PowerAction`]s it returns: `SetDuty`
//!   frames, a pause on a trip and a resume when the governor lets go, and a stop plus an alert
//!   on a GB10 fault signature.
//! * No telemetry at all means no power control: a real (non-simulated) worker is then held
//!   rather than run blind next to the ~90 W power-off band.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use spm_api::{ClockCapView, FaultView, PowerView, TelemetryView, TripView};
use spm_coexist::pmon::{parse_pmon, ProcUtil};
use spm_governor::{
    smi, thermal, CapStatus, ClockCapDetector, FaultSignature, Governor, Profile, Sample, Trip, DUTY_MIN_PCT, IDLE_RESTART,
};

/// Consecutive read errors after which the source is dropped and opened again.
pub const MAX_READ_ERRORS: u32 = 20;
/// While no source can be opened, try again this often.
pub const REOPEN_EVERY: Duration = Duration::from_secs(30);
/// Without a reading for this long, a real worker is held (the governor is blind).
pub const TELEMETRY_STALE: Duration = Duration::from_secs(3);
/// Timeout of one `nvidia-smi` call.
pub const SMI_TIMEOUT: Duration = Duration::from_secs(2);

/// One telemetry reading.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GpuReading {
    pub power_w: f64,
    pub temp_gpu_c: f64,
    pub sm_mhz: u32,
    /// `clocks_event_reasons.active` bits, when reported.
    pub event_reasons: Option<u64>,
    /// Hottest `acpitz` zone, °C.
    pub acpitz_c: Option<f64>,
    /// Sample time on the governor clock. `None` (the real sources): when it was read. Test
    /// sources set it to run the 60 s trip pauses in virtual time.
    pub at: Option<Duration>,
}

/// Where readings come from. Implementations may block (they run on the sampling thread).
pub trait TelemetrySource: Send {
    /// `nvml`, `nvidia-smi`, or a test name.
    fn name(&self) -> String;
    /// Time between two readings.
    fn period(&self) -> Duration;
    fn read(&mut self) -> Result<GpuReading, String>;
    /// SM utilization per process, for the coexistence fallback; `None` when unsupported or not
    /// available yet.
    fn processes(&mut self) -> Option<Vec<ProcUtil>> {
        None
    }
}

/// Which telemetry the governor uses.
#[derive(Clone, Default)]
pub enum TelemetryChoice {
    /// NVML (feature `nvml`), else `nvidia-smi`; neither → `no_telemetry`, and a real worker is
    /// held.
    #[default]
    Auto,
    /// No governor at all (tests, and development without a GPU).
    Off,
    /// A given source (tests).
    Custom(Arc<dyn Fn() -> Box<dyn TelemetrySource> + Send + Sync>),
}

impl std::fmt::Debug for TelemetryChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TelemetryChoice::Auto => "Auto",
            TelemetryChoice::Off => "Off",
            TelemetryChoice::Custom(_) => "Custom",
        })
    }
}

/// What the sampling thread reports.
#[derive(Debug, Clone)]
pub enum TelemetryMsg {
    Opened { source: String },
    Reading { ts: Duration, reading: GpuReading, procs: Option<Vec<ProcUtil>> },
    Failed { source: String, error: String },
    Unavailable { reason: String },
}

fn acpitz(root: &Path) -> Option<f64> {
    thermal::read_acpitz_max_c(root).ok().flatten()
}

/// Runs a command with a timeout and returns its standard output.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(out)
}

/// NVML through `spm_governor::nvml` (read-only; no CUDA context).
#[cfg(feature = "nvml")]
struct NvmlSource {
    s: spm_governor::nvml::NvmlSampler,
    thermal_root: PathBuf,
}

#[cfg(feature = "nvml")]
impl TelemetrySource for NvmlSource {
    fn name(&self) -> String {
        "nvml".into()
    }
    fn period(&self) -> Duration {
        spm_governor::SAMPLE_PERIOD
    }
    fn read(&mut self) -> Result<GpuReading, String> {
        let r = self.s.read().map_err(|e| format!("NVML: {e}"))?;
        Ok(GpuReading {
            power_w: r.power_w,
            temp_gpu_c: r.temp_gpu_c,
            sm_mhz: r.sm_mhz,
            event_reasons: r.event_reasons,
            acpitz_c: acpitz(&self.thermal_root),
            at: None,
        })
    }
    fn processes(&mut self) -> Option<Vec<ProcUtil>> {
        let v = self.s.process_sm_util().ok()?;
        Some(v.into_iter().map(|p| ProcUtil { pid: p.pid, compute: p.compute, sm_pct: p.sm_pct }).collect())
    }
}

/// `nvidia-smi` at 2 Hz, with `nvidia-smi pmon` for the per-process fallback (run in the
/// background: one pmon sample takes about a second).
pub struct SmiSource {
    exe: PathBuf,
    thermal_root: PathBuf,
    pmon: Option<std::process::Child>,
}

impl SmiSource {
    pub fn new(exe: PathBuf, thermal_root: PathBuf) -> Self {
        SmiSource { exe, thermal_root, pmon: None }
    }
}

impl TelemetrySource for SmiSource {
    fn name(&self) -> String {
        "nvidia-smi".into()
    }
    fn period(&self) -> Duration {
        smi::FALLBACK_PERIOD
    }
    fn read(&mut self) -> Result<GpuReading, String> {
        let out = run_with_timeout(Command::new(&self.exe).args(smi::query_args()), SMI_TIMEOUT)
            .map_err(|e| format!("nvidia-smi: {e}"))?;
        let r = smi::parse_query_line(out.lines().next().unwrap_or_default())?;
        Ok(GpuReading {
            power_w: r.power_w,
            temp_gpu_c: r.temp_gpu_c,
            sm_mhz: r.sm_mhz,
            event_reasons: r.event_reasons,
            acpitz_c: acpitz(&self.thermal_root),
            at: None,
        })
    }
    fn processes(&mut self) -> Option<Vec<ProcUtil>> {
        if let Some(child) = self.pmon.as_mut() {
            match child.try_wait() {
                Ok(None) => return None,
                Ok(Some(_)) => {
                    let mut out = String::new();
                    if let Some(mut s) = child.stdout.take() {
                        let _ = s.read_to_string(&mut out);
                    }
                    self.pmon = None;
                    return Some(parse_pmon(&out));
                }
                Err(_) => {
                    self.pmon = None;
                    return None;
                }
            }
        }
        self.pmon = Command::new(&self.exe)
            .args(["pmon", "-c", "1", "-s", "u"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        None
    }
}

impl Drop for SmiSource {
    fn drop(&mut self) {
        if let Some(mut c) = self.pmon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// NVML if it loads, else `nvidia-smi` if it answers.
fn open_auto(thermal_root: &Path) -> Result<Box<dyn TelemetrySource>, String> {
    #[cfg(feature = "nvml")]
    let nvml_err = match spm_governor::nvml::NvmlSampler::init(0) {
        Ok(s) => return Ok(Box::new(NvmlSource { s, thermal_root: thermal_root.to_path_buf() })),
        Err(e) => format!("NVML: {e}"),
    };
    #[cfg(not(feature = "nvml"))]
    let nvml_err = "NVML support not built in".to_string();
    let mut smi = SmiSource::new(PathBuf::from("nvidia-smi"), thermal_root.to_path_buf());
    match smi.read() {
        Ok(_) => {
            tracing::warn!(target: "spm::power", reason = %nvml_err, "NVML unavailable: power telemetry falls back to nvidia-smi at 2 Hz");
            Ok(Box::new(smi))
        }
        Err(e) => Err(format!("{nvml_err}; {e}")),
    }
}

/// Starts the sampling thread. `sink` returns `false` once the daemon is gone, which ends the
/// thread. `want_procs` asks for per-process utilization (coexistence fallback).
pub fn spawn_sampler(
    choice: TelemetryChoice,
    origin: Instant,
    want_procs: Arc<AtomicBool>,
    sink: impl Fn(TelemetryMsg) -> bool + Send + 'static,
) {
    if matches!(choice, TelemetryChoice::Off) {
        return;
    }
    let thermal_root = PathBuf::from(thermal::THERMAL_ROOT);
    let body = move || loop {
        let opened = match &choice {
            TelemetryChoice::Off => return,
            TelemetryChoice::Auto => open_auto(&thermal_root),
            TelemetryChoice::Custom(make) => Ok(make()),
        };
        let mut src = match opened {
            Ok(s) => s,
            Err(reason) => {
                if !sink(TelemetryMsg::Unavailable { reason }) {
                    return;
                }
                thread::sleep(REOPEN_EVERY);
                continue;
            }
        };
        let name = src.name();
        if !sink(TelemetryMsg::Opened { source: name.clone() }) {
            return;
        }
        let mut errors = 0;
        loop {
            let t0 = Instant::now();
            let msg = match src.read() {
                Ok(reading) => {
                    errors = 0;
                    let procs = if want_procs.load(Ordering::Relaxed) { src.processes() } else { None };
                    TelemetryMsg::Reading { ts: reading.at.unwrap_or_else(|| t0.saturating_duration_since(origin)), reading, procs }
                }
                Err(error) => {
                    errors += 1;
                    TelemetryMsg::Failed { source: name.clone(), error }
                }
            };
            if !sink(msg) {
                return;
            }
            if errors >= MAX_READ_ERRORS {
                break;
            }
            thread::sleep(src.period().saturating_sub(t0.elapsed()));
        }
    };
    if let Err(e) = thread::Builder::new().name("spm-telemetry".into()).spawn(body) {
        tracing::error!(target: "spm::power", error = %e, "could not start the telemetry thread");
    }
}

/// What the daemon must do after a sample.
#[derive(Debug, Clone, PartialEq)]
pub enum PowerAction {
    /// Send this duty to the worker (IPC `SetDuty`).
    SetDuty(u8),
    /// A trip fired: hold the worker (IPC `Pause`, context kept).
    Pause,
    /// The trip is over: the worker may run again (IPC `Resume`, after the `SetDuty`).
    Resume,
    /// A GB10 fault signature: stop mining until the user clears it.
    Fault(FaultSignature),
    Alert { level: &'static str, msg: String },
    /// SSE `power` event.
    Event { kind: &'static str, detail: String },
}

/// State of the telemetry feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Governor disabled.
    Off,
    /// Waiting for the first reading.
    Starting,
    Active,
    /// No source could be opened.
    Unavailable,
}

#[derive(Debug, Clone, Copy)]
struct Last {
    sample: Sample,
    reasons: Option<u64>,
    at_ms: u64,
    effective_target_w: f64,
}

/// The daemon's side of the governor.
#[derive(Debug)]
pub struct PowerCtl {
    gov: Governor,
    configured: Profile,
    /// Ceiling for this run after an unclean stop of the previous one.
    stepdown: Option<Profile>,
    unclean_alert: Option<String>,
    cap: ClockCapDetector,
    cap_reported: &'static str,
    feed: Feed,
    source: String,
    duty_sent: Option<u8>,
    trip: Option<Trip>,
    last_trip: Option<TripView>,
    trips_total: u64,
    fault: Option<FaultView>,
    last: Option<Last>,
    last_reading_at: Option<Instant>,
    /// Governor-clock time of the last sample taken while the worker computed.
    last_active_ts: Option<Duration>,
}

/// The governor profile of a config value.
pub fn profile_of(p: spm_api::config::PowerProfile) -> Profile {
    match p {
        spm_api::config::PowerProfile::Eco => Profile::Eco,
        spm_api::config::PowerProfile::Balanced => Profile::Balanced,
        spm_api::config::PowerProfile::Max => Profile::Max,
    }
}

/// `power.profile` checked against `power.max_acknowledged` (Max is refused without it).
pub fn select_profile(cfg: &spm_api::config::PowerConfig) -> Result<Profile, spm_governor::ProfileError> {
    Profile::select(profile_of(cfg.profile), cfg.max_acknowledged)
}

fn trip_view(t: &Trip, at_ms: u64, now_ts: Duration) -> TripView {
    TripView {
        code: t.reason.code().into(),
        detail: t.to_string(),
        at_ms,
        resume_in_s: t.resume_not_before.map(|r| r.saturating_sub(now_ts).as_secs()),
    }
}

impl PowerCtl {
    /// `stepdown`: the profile planned by `marker::begin_run` after an unclean stop.
    pub fn new(configured: Profile, stepdown: Option<Profile>, unclean_alert: Option<String>, enabled: bool) -> Self {
        let profile = stepdown.map_or(configured, |c| configured.min(c));
        PowerCtl {
            gov: Governor::new(profile),
            configured,
            stepdown,
            unclean_alert,
            cap: ClockCapDetector::new(profile.recommended_clock_cap_mhz()),
            cap_reported: "unknown",
            feed: if enabled { Feed::Starting } else { Feed::Off },
            source: if enabled { "none".into() } else { "off".into() },
            duty_sent: None,
            trip: None,
            last_trip: None,
            trips_total: 0,
            fault: None,
            last: None,
            last_reading_at: None,
            last_active_ts: None,
        }
    }

    /// The profile in force.
    pub fn profile(&self) -> Profile {
        self.stepdown.map_or(self.configured, |c| self.configured.min(c))
    }

    pub fn feed(&self) -> Feed {
        self.feed
    }

    /// The governor's current duty (what a new worker should start with).
    pub fn duty_pct(&self) -> u8 {
        self.gov.duty_pct()
    }

    /// A new configured profile. Returns `(before, after)` when the profile in force changed.
    pub fn set_configured(&mut self, p: Profile) -> Option<(Profile, Profile)> {
        let before = self.profile();
        self.configured = p;
        let after = self.profile();
        if after == before {
            return None;
        }
        self.gov.set_profile(after);
        if after.recommended_clock_cap_mhz() != self.cap.cap_mhz() {
            self.cap = ClockCapDetector::new(after.recommended_clock_cap_mhz());
            self.cap_reported = "unknown";
        }
        Some((before, after))
    }

    pub fn on_opened(&mut self, source: String) {
        self.source = source;
        if self.feed != Feed::Off {
            self.feed = Feed::Active;
        }
    }

    pub fn on_unavailable(&mut self) {
        if self.feed != Feed::Off {
            self.feed = Feed::Unavailable;
            self.source = "none".into();
        }
    }

    /// A trip (not a fault) holds the worker.
    pub fn holding(&self) -> bool {
        self.trip.is_some_and(|t| !t.is_fault())
    }

    /// A latched fault signature.
    pub fn fault(&self) -> Option<&FaultView> {
        self.fault.as_ref()
    }

    /// Why a real worker must not run for want of telemetry, if so.
    pub fn blind(&self, now: Instant) -> Option<&'static str> {
        match self.feed {
            Feed::Off => None,
            Feed::Unavailable => Some("no GPU telemetry (neither NVML nor nvidia-smi answers)"),
            Feed::Starting => Some("waiting for the first GPU telemetry sample"),
            Feed::Active => match self.last_reading_at {
                Some(t) if now.duration_since(t) <= TELEMETRY_STALE => None,
                _ => Some("GPU telemetry stopped"),
            },
        }
    }

    /// The user cleared a fault (Start). Returns whether there was one.
    pub fn clear_fault(&mut self) -> bool {
        if self.fault.take().is_some() {
            self.gov.clear_fault();
            self.trip = None;
            true
        } else {
            false
        }
    }

    /// Consumes one sample. `now_ms`: Unix time for the views; `reasons`: clock event bits.
    pub fn on_sample(&mut self, s: &Sample, reasons: Option<u64>, now_ms: u64) -> Vec<PowerAction> {
        let mut out = Vec::new();
        if self.feed == Feed::Off {
            return out;
        }
        self.feed = Feed::Active;
        self.last_reading_at = Some(Instant::now());
        if s.worker_active {
            self.last_active_ts = Some(s.ts);
        }
        let duty_in_force = if s.worker_active { self.duty_sent.unwrap_or(DUTY_MIN_PCT) } else { 0 };
        self.cap.observe(s, duty_in_force);
        let status = self.cap.status();
        if status.as_str() != self.cap_reported {
            self.cap_reported = status.as_str();
            match status {
                CapStatus::Capped { max_seen_mhz } => {
                    tracing::info!(target: "spm::power", max_seen_mhz, cap_mhz = self.cap.cap_mhz(), "SM clock cap detected")
                }
                CapStatus::Uncapped { max_seen_mhz } => tracing::warn!(
                    target: "spm::power",
                    max_seen_mhz,
                    cap_mhz = self.cap.cap_mhz(),
                    "the SM clock went above the recommended cap: the clock-cap unit is not in force (see POWER-THERMAL.md)"
                ),
                CapStatus::Unknown => {}
            }
        }
        let d = self.gov.step(s);
        self.last = Some(Last { sample: *s, reasons, at_ms: now_ms, effective_target_w: self.gov.effective_target_w(s) });
        match d.trip {
            Some(t) if d.fired => {
                self.trips_total += 1;
                self.last_trip = Some(trip_view(&t, now_ms, s.ts));
                self.trip = Some(t);
                if let Some(sig) = t.fault() {
                    self.fault = Some(FaultView { signature: sig.as_str().into(), alert: sig.alert().into(), at_ms: now_ms });
                    out.push(PowerAction::Fault(sig));
                    out.push(PowerAction::Alert { level: "error", msg: format!("power governor: {}", sig.alert()) });
                    out.push(PowerAction::Event { kind: "fault", detail: format!("{}: {}", sig.as_str(), sig.alert()) });
                } else {
                    out.push(PowerAction::Pause);
                    out.push(PowerAction::Alert { level: "warn", msg: format!("power governor: {t}") });
                    out.push(PowerAction::Event { kind: "trip", detail: t.to_string() });
                }
            }
            Some(_) => {}
            None => {
                let resumed = self.trip.take().is_some();
                if self.duty_sent != Some(d.duty_pct) {
                    self.duty_sent = Some(d.duty_pct);
                    out.push(PowerAction::SetDuty(d.duty_pct));
                }
                if resumed {
                    out.push(PowerAction::Resume);
                    out.push(PowerAction::Event {
                        kind: "resume",
                        detail: format!("mining resumes at {} % duty", d.duty_pct),
                    });
                }
            }
        }
        out
    }

    /// The duty was (re)sent to the worker outside [`PowerCtl::on_sample`].
    pub fn note_duty_sent(&mut self, pct: u8) {
        self.duty_sent = Some(pct);
    }

    /// The worker is about to run again. After [`IDLE_RESTART`] without computing (yielding,
    /// released) it must restart from the minimum duty; the governor restarts its ramp on the
    /// next active sample, and this makes the first frames match. Returns the duty to send.
    pub fn on_worker_start(&mut self) -> Option<u8> {
        if self.feed == Feed::Off {
            return None;
        }
        let now = self.last.map(|l| l.sample.ts)?;
        let idle_long = self.last_active_ts.is_none_or(|t| now.saturating_sub(t) >= IDLE_RESTART);
        (idle_long && self.duty_sent != Some(DUTY_MIN_PCT)).then(|| {
            self.duty_sent = Some(DUTY_MIN_PCT);
            DUTY_MIN_PCT
        })
    }

    pub fn view(&self) -> PowerView {
        let p = self.profile();
        let l = p.limits();
        let state = match self.feed {
            Feed::Off => "off",
            _ if self.fault.is_some() => "fault",
            Feed::Unavailable => "no_telemetry",
            Feed::Starting => "starting",
            Feed::Active if self.holding() => "tripped",
            Feed::Active if self.last.is_some_and(|l| l.sample.worker_active) => "running",
            Feed::Active => "idle",
        };
        let now_ts = self.last.map_or(Duration::ZERO, |l| l.sample.ts);
        let (cap_status, max_seen) = match self.cap.status() {
            CapStatus::Unknown => ("unknown", self.last.map_or(0, |l| l.sample.sm_mhz)),
            CapStatus::Capped { max_seen_mhz } => ("capped", max_seen_mhz),
            CapStatus::Uncapped { max_seen_mhz } => ("uncapped", max_seen_mhz),
        };
        PowerView {
            state: state.into(),
            source: self.source.clone(),
            profile: p.as_str().into(),
            configured_profile: self.configured.as_str().into(),
            stepped_down: p != self.configured,
            target_w: l.target_w,
            hard_stop_w: l.hard_stop_w,
            effective_target_w: self.last.map(|l| l.effective_target_w),
            duty_pct: self.duty_sent.unwrap_or_else(|| self.gov.duty_pct()),
            clock_cap: ClockCapView { cap_mhz: self.cap.cap_mhz(), status: cap_status.into(), max_seen_mhz: max_seen },
            trip: self.trip.map(|t| trip_view(&t, self.last_trip.as_ref().map_or(0, |v| v.at_ms), now_ts)),
            trips_total: self.trips_total,
            last_trip: self.last_trip.clone(),
            fault: self.fault.clone(),
            telemetry: self.last.map(|l| TelemetryView {
                power_w: l.sample.power_w,
                temp_gpu_c: l.sample.temp_gpu_c,
                temp_acpitz_c: l.sample.temp_acpitz_c,
                sm_clock_mhz: l.sample.sm_mhz,
                event_reasons: l.reasons,
                at_ms: l.at_ms,
            }),
            unclean_start: self.unclean_alert.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spm_governor::{OVER_POWER_PAUSE, OVER_POWER_SAMPLES};

    fn sample(t_ms: u64, power_w: f64, active: bool) -> Sample {
        Sample {
            ts: Duration::from_millis(t_ms),
            power_w,
            temp_gpu_c: 60.0,
            temp_acpitz_c: Some(50.0),
            sm_mhz: 2200,
            worker_active: active,
        }
    }

    fn ctl() -> PowerCtl {
        let mut c = PowerCtl::new(Profile::Balanced, None, None, true);
        c.on_opened("test".into());
        c
    }

    #[test]
    fn duty_ramps_then_an_over_power_trip_pauses_and_resumes_after_60_s() {
        let mut c = ctl();
        let a = c.on_sample(&sample(0, 20.0, true), None, 1);
        assert_eq!(a, vec![PowerAction::SetDuty(DUTY_MIN_PCT)]);
        // 30 W against a 75 W target: the duty climbs, at most 20 %/s.
        let mut duties = Vec::new();
        for i in 1..=20 {
            for a in c.on_sample(&sample(i * 100, 30.0, true), None, 1) {
                if let PowerAction::SetDuty(d) = a {
                    duties.push(d);
                }
            }
        }
        assert!(!duties.is_empty() && duties.windows(2).all(|w| w[1] > w[0]), "{duties:?}");
        assert!(*duties.last().unwrap() <= DUTY_MIN_PCT + 41, "{duties:?}");
        // 3 samples over the 85 W hard stop trip it.
        let t0 = 3_000;
        assert!(c.on_sample(&sample(t0, 90.0, true), None, 1).is_empty() || !c.holding());
        c.on_sample(&sample(t0 + 100, 90.0, true), None, 1);
        let a = c.on_sample(&sample(t0 + 200, 90.0, true), None, 2);
        assert_eq!(OVER_POWER_SAMPLES, 3);
        assert_eq!(a[0], PowerAction::Pause);
        assert!(matches!(&a[1], PowerAction::Alert { level: "warn", msg } if msg.contains("85 W hard stop")), "{a:?}");
        assert!(matches!(&a[2], PowerAction::Event { kind: "trip", .. }));
        assert!(c.holding());
        let v = c.view();
        assert_eq!((v.state.as_str(), v.trips_total), ("tripped", 1));
        assert_eq!(v.trip.as_ref().unwrap().code, "over_power");
        assert_eq!(v.trip.as_ref().unwrap().resume_in_s, Some(60));
        // Held (and silent) for 60 s even when the power is back down.
        let tripped_at = t0 + 200;
        let mut t = tripped_at + 100;
        while t < tripped_at + OVER_POWER_PAUSE.as_millis() as u64 {
            assert!(c.on_sample(&sample(t, 15.0, false), None, 3).is_empty(), "t={t}");
            t += 100;
        }
        // The controller had already backed off to the minimum above the stop, so the resume
        // needs no new duty frame.
        let a = c.on_sample(&sample(t, 15.0, false), None, 4);
        assert_eq!(a[0], PowerAction::Resume, "{a:?}");
        assert!(matches!(&a[1], PowerAction::Event { kind: "resume", .. }));
        assert!(!c.holding());
        let v = c.view();
        assert_eq!((v.duty_pct, v.last_trip.unwrap().code.as_str()), (DUTY_MIN_PCT, "over_power"));
        // A resume from a higher duty sends the minimum first.
        let mut c = ctl();
        c.on_sample(&sample(0, 20.0, true), None, 1);
        for i in 1..=20 {
            c.on_sample(&sample(i * 100, 30.0, true), None, 1);
        }
        for i in 0..3 {
            c.on_sample(&sample(2_100 + i * 100, 86.0, false), None, 1);
        }
        assert!(c.holding());
        assert!(c.view().duty_pct > DUTY_MIN_PCT);
        let a = c.on_sample(&sample(2_300 + OVER_POWER_PAUSE.as_millis() as u64, 15.0, false), None, 1);
        assert_eq!(a[..2], [PowerAction::SetDuty(DUTY_MIN_PCT), PowerAction::Resume], "{a:?}");
    }

    #[test]
    fn a_long_idle_restarts_the_worker_at_the_minimum_duty() {
        let mut c = ctl();
        c.on_sample(&sample(0, 20.0, true), None, 1);
        for i in 1..=20 {
            c.on_sample(&sample(i * 100, 30.0, true), None, 1);
        }
        assert!(c.view().duty_pct > 40);
        // A short yield keeps the duty ...
        c.on_sample(&sample(12_000, 15.0, false), None, 1);
        assert_eq!(c.on_worker_start(), None);
        // ... more than 30 s without computing does not.
        c.on_sample(&sample(33_000, 15.0, false), None, 1);
        assert_eq!(c.on_worker_start(), Some(DUTY_MIN_PCT));
        assert_eq!(c.on_worker_start(), None);
        assert_eq!(c.view().duty_pct, DUTY_MIN_PCT);
    }

    #[test]
    fn a_fault_signature_stops_until_cleared() {
        let mut c = ctl();
        // Pinned at 100 W for more than 10 s (whatever the worker does).
        let mut fired = Vec::new();
        for i in 0..=120 {
            let mut s = sample(i * 100, 100.0, false);
            s.sm_mhz = 1500;
            for a in c.on_sample(&s, None, 7) {
                if matches!(a, PowerAction::Fault(_) | PowerAction::Pause) {
                    fired.push(a);
                }
            }
        }
        // The over-power trip comes first (0.3 s), then the fault signature latches.
        assert_eq!(fired[0], PowerAction::Pause);
        assert_eq!(fired.last(), Some(&PowerAction::Fault(FaultSignature::ThermalCap100W)));
        let v = c.view();
        assert_eq!(v.state, "fault");
        assert_eq!(v.fault.as_ref().unwrap().signature, "thermal_cap_100w");
        assert!(!c.holding() && c.fault().is_some());
        // Nothing resumes on its own.
        for i in 121..400 {
            let acts = c.on_sample(&sample(i * 100, 15.0, false), None, 8);
            assert!(!acts.contains(&PowerAction::Resume), "{acts:?}");
        }
        assert!(c.clear_fault());
        assert!(!c.clear_fault());
        assert_eq!(c.view().state, "idle");
    }

    #[test]
    fn profiles_step_down_and_max_needs_the_acknowledgement() {
        let mut cfg = spm_api::config::PowerConfig::default();
        assert_eq!(select_profile(&cfg), Ok(Profile::Balanced));
        cfg.profile = spm_api::config::PowerProfile::Max;
        assert_eq!(select_profile(&cfg), Err(spm_governor::ProfileError::MaxNotAcknowledged));
        cfg.max_acknowledged = true;
        assert_eq!(select_profile(&cfg), Ok(Profile::Max));
        // An unclean stop capped this run at Eco: a higher configured profile stays capped.
        let mut c = PowerCtl::new(Profile::Balanced, Some(Profile::Eco), Some("unclean".into()), true);
        let v = c.view();
        assert_eq!((v.profile.as_str(), v.configured_profile.as_str(), v.stepped_down), ("eco", "balanced", true));
        assert_eq!((v.target_w, v.hard_stop_w, v.clock_cap.cap_mhz), (60.0, 70.0, 1800));
        assert_eq!(c.set_configured(Profile::Max), None);
        let mut c2 = ctl();
        assert_eq!(c2.set_configured(Profile::Eco), Some((Profile::Balanced, Profile::Eco)));
        assert_eq!(c2.view().clock_cap.cap_mhz, 1800);
        assert_eq!(c.view().unclean_start.as_deref(), Some("unclean"));
    }

    /// Reads this machine's GPU through the real sources (NVML, then nvidia-smi): read-only
    /// queries, never a CUDA context. Skipped unless SPM_NVML_TESTS=1 (CI has no GPU).
    #[test]
    fn real_sources_read_this_gpu() {
        if std::env::var("SPM_NVML_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipped (set SPM_NVML_TESTS=1)");
            return;
        }
        let root = Path::new(thermal::THERMAL_ROOT);
        let mut src = open_auto(root).expect("a telemetry source");
        let r = src.read().expect("a reading");
        eprintln!("{}: {r:?}", src.name());
        assert!(r.power_w > 1.0 && r.power_w < 300.0 && r.sm_mhz > 0 && r.temp_gpu_c > 0.0, "{r:?}");
        let mut smi = SmiSource::new(PathBuf::from("nvidia-smi"), root.to_path_buf());
        let r = smi.read().expect("nvidia-smi reading");
        eprintln!("nvidia-smi: {r:?}");
        assert!(r.power_w > 1.0 && r.sm_mhz > 0, "{r:?}");
        eprintln!("{} processes: {:?}", src.name(), src.processes());
        assert_eq!(smi.processes(), None, "pmon runs in the background");
        thread::sleep(Duration::from_secs(3));
        let procs = smi.processes().expect("pmon result");
        eprintln!("pmon: {procs:?}");
    }

    #[test]
    fn blind_governor_holds_a_real_worker() {
        let c = PowerCtl::new(Profile::Balanced, None, None, false);
        assert_eq!(c.blind(Instant::now()), None);
        assert_eq!(c.view().state, "off");
        let mut c = PowerCtl::new(Profile::Balanced, None, None, true);
        assert!(c.blind(Instant::now()).is_some());
        c.on_unavailable();
        assert_eq!(c.view().state, "no_telemetry");
        c.on_opened("nvml".into());
        c.on_sample(&sample(0, 15.0, false), None, 1);
        assert_eq!(c.blind(Instant::now()), None);
        assert!(c.blind(Instant::now() + TELEMETRY_STALE + Duration::from_secs(1)).is_some());
    }
}
