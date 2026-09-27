//! spm-coexist — sharing a DGX Spark with a resident LLM server.
//!
//! Two CUDA contexts on one GPU are time-sliced: while both are busy each gets roughly half the
//! GPU, so mining next to a busy vLLM costs it about 50 % of its throughput (and us the same).
//! The rule is therefore simple: mine only while the LLM server is idle, and get out of the way
//! fast when a request arrives. Four modes:
//!
//! * [`CoexistMode::SparkModo`] — on a box managed by `spark-modo` the worker runs only as its
//!   `miner` runtime (exclusive GPU lease); `spark-modo` starts and stops it. The daemon does not
//!   gate anything, it just reports.
//! * [`CoexistMode::Yield`] — poll vLLM's `/metrics` every 100–250 ms and mine only
//!   after `idle` (5 s) of `num_requests_running == 0 && num_requests_waiting == 0`; pause the
//!   worker (context kept) as soon as either is non-zero. When the metrics are unavailable, fall
//!   back to NVML per-process SM utilization of other compute processes.
//! * [`CoexistMode::YieldRelease`] — like Yield, but a busy LLM makes the daemon Release the
//!   worker: the process exits and its CUDA context (and memory) is freed.
//! * [`CoexistMode::Exclusive`] (default, as in `spm-api`'s config) — mine regardless of other
//!   GPU users; the user presses Stop to use the GPU for something else.
//!
//! Every mode is subject to the [`memguard`] (unified memory: refuse to start without 20 GiB of
//! headroom, exit below 16 GiB available or above 10 % memory pressure).
//!
//! Pieces: [`prom`] (Prometheus text parser for the two vLLM gauges), [`http`] (minimal
//! HTTP/1.1 GET over tokio), [`gate`] (the pure yield decision with an injected clock),
//! [`pmon`] (the SM-utilization fallback), [`memguard`] and [`handshake`] (the SIGUSR1/SIGUSR2
//! pause/resume protocol between a controller and the worker).

#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

pub mod gate;
pub mod handshake;
pub mod http;
pub mod memguard;
pub mod pmon;
pub mod prom;

pub use gate::{GateDecision, LoadSignal, YieldGate};
pub use prom::VllmLoad;

/// vLLM's metrics endpoint on the author's box (`spark-vllm.service`).
pub const DEFAULT_METRICS_URL: &str = "http://127.0.0.1:8001/metrics";
/// Fastest allowed metrics poll.
pub const POLL_MIN: Duration = Duration::from_millis(100);
/// Slowest allowed metrics poll.
pub const POLL_MAX: Duration = Duration::from_millis(250);
/// Default metrics poll: a request waits at most this long (plus the ≤ 10 ms pause) for the
/// GPU, and vLLM renders its ~60 KB of metrics 5 times a second.
pub const DEFAULT_POLL: Duration = Duration::from_millis(200);
/// Idle time required before mining starts or resumes.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(5);
/// A metrics fetch slower than this counts as "metrics unavailable" for that poll.
pub const METRICS_TIMEOUT: Duration = Duration::from_millis(250);
/// Fallback: another compute process at or above this SM utilization means "busy", percent.
pub const DEFAULT_BUSY_SM_PCT: u32 = 10;

/// How the miner shares the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CoexistMode {
    /// The `spark-modo` `miner` runtime owns the worker; the daemon only reports.
    SparkModo,
    /// Pause the worker while the LLM server is busy.
    Yield,
    /// Release the worker (exit, free the CUDA context) while the LLM server is busy.
    YieldRelease,
    /// Mine regardless (the default).
    #[default]
    Exclusive,
}

impl CoexistMode {
    /// Config/API spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            CoexistMode::SparkModo => "spark-modo",
            CoexistMode::Yield => "yield",
            CoexistMode::YieldRelease => "yield-release",
            CoexistMode::Exclusive => "exclusive",
        }
    }

    /// Whether the daemon starts, pauses and stops the worker itself (false for spark-modo).
    pub const fn daemon_controls_worker(self) -> bool {
        !matches!(self, CoexistMode::SparkModo)
    }

    /// Whether the daemon polls the LLM server.
    pub const fn polls_llm(self) -> bool {
        matches!(self, CoexistMode::Yield | CoexistMode::YieldRelease)
    }
}

impl fmt::Display for CoexistMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CoexistMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "spark-modo" | "sparkmodo" => Ok(CoexistMode::SparkModo),
            "yield" => Ok(CoexistMode::Yield),
            "yield-release" => Ok(CoexistMode::YieldRelease),
            "exclusive" => Ok(CoexistMode::Exclusive),
            other => Err(format!(
                "unknown coexistence mode {other:?} (spark-modo, yield, yield-release, exclusive)"
            )),
        }
    }
}

/// Coexistence settings (the `[coexist]` table of the config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoexistConfig {
    pub mode: CoexistMode,
    /// vLLM metrics URL (`http://` only).
    pub metrics_url: String,
    /// Requested poll period; clamped by [`CoexistConfig::poll_interval`].
    pub poll: Duration,
    /// Idle time before mining.
    pub idle: Duration,
    /// Fallback busy threshold, percent SM.
    pub busy_sm_pct: u32,
}

impl Default for CoexistConfig {
    fn default() -> Self {
        CoexistConfig {
            mode: CoexistMode::default(),
            metrics_url: DEFAULT_METRICS_URL.to_string(),
            poll: DEFAULT_POLL,
            idle: DEFAULT_IDLE,
            busy_sm_pct: DEFAULT_BUSY_SM_PCT,
        }
    }
}

impl CoexistConfig {
    /// The poll period, clamped to [`POLL_MIN`]..=[`POLL_MAX`].
    pub fn poll_interval(&self) -> Duration {
        self.poll.clamp(POLL_MIN, POLL_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_print() {
        for m in [
            CoexistMode::SparkModo,
            CoexistMode::Yield,
            CoexistMode::YieldRelease,
            CoexistMode::Exclusive,
        ] {
            assert_eq!(m.as_str().parse::<CoexistMode>(), Ok(m));
        }
        assert_eq!("yield_release".parse::<CoexistMode>(), Ok(CoexistMode::YieldRelease));
        assert!("share".parse::<CoexistMode>().is_err());
        assert_eq!(CoexistMode::default(), CoexistMode::Exclusive);
        assert!(!CoexistMode::SparkModo.daemon_controls_worker());
        assert!(
            CoexistMode::Exclusive.daemon_controls_worker() && !CoexistMode::Exclusive.polls_llm()
        );
    }

    #[test]
    fn poll_interval_is_clamped_to_100_250_ms() {
        let mut c = CoexistConfig::default();
        assert_eq!(c.poll_interval(), Duration::from_millis(200));
        c.poll = Duration::from_millis(10);
        assert_eq!(c.poll_interval(), POLL_MIN);
        c.poll = Duration::from_secs(2);
        assert_eq!(c.poll_interval(), POLL_MAX);
        assert_eq!(c.idle, Duration::from_secs(5));
    }
}
