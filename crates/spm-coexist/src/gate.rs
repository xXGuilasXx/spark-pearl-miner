//! The yield decision: a pure state machine fed with one load observation per poll and the
//! daemon's monotonic clock.

use std::time::Duration;

use crate::prom::VllmLoad;
use crate::{CoexistConfig, CoexistMode};

/// What one poll found out about the other GPU user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadSignal {
    /// The vLLM gauges.
    Vllm(VllmLoad),
    /// Metrics unavailable; highest SM utilization among the *other* compute processes
    /// (NVML per-process utilization, our worker excluded).
    ForeignSmUtil { pct: u32 },
    /// Neither source answered. Treated as busy: never mine blind next to an LLM server.
    Unavailable,
}

/// What the daemon should do with the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateDecision {
    /// Run the worker (spawn it if it was released).
    Mine,
    /// Keep the worker and its CUDA context but stop GPU work (IPC `Pause` / SIGUSR1).
    Pause,
    /// Make the worker exit so its CUDA context and memory are freed (IPC `Release`).
    Release,
    /// spark-modo mode: the `miner` runtime owns the worker; report only.
    External,
}

/// The yield gate.
#[derive(Debug, Clone)]
pub struct YieldGate {
    mode: CoexistMode,
    idle: Duration,
    busy_sm_pct: u32,
    idle_since: Option<Duration>,
    decision: GateDecision,
}

impl YieldGate {
    /// A gate that starts out not mining (Yield/YieldRelease need `idle` of quiet first).
    pub fn new(cfg: &CoexistConfig) -> Self {
        let decision = match cfg.mode {
            CoexistMode::SparkModo => GateDecision::External,
            CoexistMode::Exclusive => GateDecision::Mine,
            CoexistMode::Yield => GateDecision::Pause,
            CoexistMode::YieldRelease => GateDecision::Release,
        };
        YieldGate {
            mode: cfg.mode,
            idle: cfg.idle,
            busy_sm_pct: cfg.busy_sm_pct,
            idle_since: None,
            decision,
        }
    }

    /// The current decision.
    pub fn decision(&self) -> GateDecision {
        self.decision
    }

    /// Whether a signal means "the other GPU user is busy".
    pub fn is_busy(&self, signal: LoadSignal) -> bool {
        match signal {
            LoadSignal::Vllm(load) => !load.is_idle(),
            LoadSignal::ForeignSmUtil { pct } => pct >= self.busy_sm_pct,
            LoadSignal::Unavailable => true,
        }
    }

    /// Feeds one poll result taken at `now` (monotonic) and returns the decision.
    pub fn observe(&mut self, now: Duration, signal: LoadSignal) -> GateDecision {
        let yielded = match self.mode {
            CoexistMode::SparkModo | CoexistMode::Exclusive => return self.decision,
            CoexistMode::Yield => GateDecision::Pause,
            CoexistMode::YieldRelease => GateDecision::Release,
        };
        if self.is_busy(signal) {
            self.idle_since = None;
            self.decision = yielded;
        } else {
            let since = *self.idle_since.get_or_insert(now);
            if now.saturating_sub(since) >= self.idle {
                self.decision = GateDecision::Mine;
            }
        }
        self.decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: CoexistMode) -> CoexistConfig {
        CoexistConfig { mode, ..CoexistConfig::default() }
    }

    fn ms(t: u64) -> Duration {
        Duration::from_millis(t)
    }

    const IDLE: LoadSignal = LoadSignal::Vllm(VllmLoad { running: 0.0, waiting: 0.0 });
    const RUNNING: LoadSignal = LoadSignal::Vllm(VllmLoad { running: 1.0, waiting: 0.0 });
    const WAITING: LoadSignal = LoadSignal::Vllm(VllmLoad { running: 0.0, waiting: 2.0 });

    #[test]
    fn yield_mines_only_after_5_s_of_continuous_idle() {
        let mut g = YieldGate::new(&cfg(CoexistMode::Yield));
        assert_eq!(g.decision(), GateDecision::Pause);
        for t in (0..5_000).step_by(200) {
            assert_eq!(g.observe(ms(t), IDLE), GateDecision::Pause, "t={t}");
        }
        assert_eq!(g.observe(ms(5_000), IDLE), GateDecision::Mine);
        // Any request pauses at once.
        assert_eq!(g.observe(ms(5_200), RUNNING), GateDecision::Pause);
        // A blip of idle does not resume; the 5 s restart from the last busy poll.
        assert_eq!(g.observe(ms(5_400), IDLE), GateDecision::Pause);
        assert_eq!(g.observe(ms(7_000), WAITING), GateDecision::Pause);
        assert_eq!(g.observe(ms(7_200), IDLE), GateDecision::Pause);
        assert_eq!(g.observe(ms(12_000), IDLE), GateDecision::Pause);
        assert_eq!(g.observe(ms(12_200), IDLE), GateDecision::Mine);
    }

    #[test]
    fn yield_release_releases_instead_of_pausing() {
        let mut g = YieldGate::new(&cfg(CoexistMode::YieldRelease));
        assert_eq!(g.decision(), GateDecision::Release);
        assert_eq!(g.observe(ms(0), IDLE), GateDecision::Release);
        assert_eq!(g.observe(ms(5_000), IDLE), GateDecision::Mine);
        assert_eq!(g.observe(ms(5_100), WAITING), GateDecision::Release);
    }

    #[test]
    fn fallback_signal_and_unavailable() {
        let mut g = YieldGate::new(&cfg(CoexistMode::Yield));
        assert_eq!(g.observe(ms(0), LoadSignal::ForeignSmUtil { pct: 3 }), GateDecision::Pause);
        assert_eq!(g.observe(ms(5_000), LoadSignal::ForeignSmUtil { pct: 9 }), GateDecision::Mine);
        assert_eq!(
            g.observe(ms(5_200), LoadSignal::ForeignSmUtil { pct: 10 }),
            GateDecision::Pause
        );
        assert_eq!(g.observe(ms(5_400), IDLE), GateDecision::Pause);
        // Nothing answers: fail closed.
        assert_eq!(g.observe(ms(20_000), LoadSignal::Unavailable), GateDecision::Pause);
        assert_eq!(g.observe(ms(40_000), LoadSignal::Unavailable), GateDecision::Pause);
    }

    #[test]
    fn exclusive_and_spark_modo_ignore_the_signal() {
        let mut ex = YieldGate::new(&cfg(CoexistMode::Exclusive));
        let mut sm = YieldGate::new(&cfg(CoexistMode::SparkModo));
        for (t, s) in [(0, RUNNING), (100, LoadSignal::Unavailable), (200, IDLE)] {
            assert_eq!(ex.observe(ms(t), s), GateDecision::Mine);
            assert_eq!(sm.observe(ms(t), s), GateDecision::External);
        }
    }

    #[test]
    fn custom_idle_time() {
        let c = CoexistConfig { idle: Duration::from_secs(1), ..cfg(CoexistMode::Yield) };
        let mut g = YieldGate::new(&c);
        assert_eq!(g.observe(ms(0), IDLE), GateDecision::Pause);
        assert_eq!(g.observe(ms(1_000), IDLE), GateDecision::Mine);
    }
}
