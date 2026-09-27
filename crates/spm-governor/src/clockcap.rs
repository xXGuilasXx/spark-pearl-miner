//! Detects from the telemetry whether the SM clock cap (`nvidia-smi -lgc 300,<mhz>`) is in
//! force. NVML has no getter for locked clocks, but with the cap installed the SM clock never
//! goes above it, idle or loaded (without it this GB10 idles at ~2418 MHz).

use std::time::Duration;

use crate::{Sample, MAX_SAMPLE_GAP};

/// Clock readings within this many MHz above the cap still count as capped (clock steps).
pub const CAP_TOLERANCE_MHZ: u32 = 30;
/// Loaded time needed before declaring the cap in force.
pub const CAP_CONFIRM_LOADED: Duration = Duration::from_secs(30);
/// "Loaded" means our worker computes at least at this duty.
pub const CAP_LOADED_MIN_DUTY_PCT: u8 = 90;
/// How long the clock must stay above the cap, without a break, before the cap counts as absent.
/// Right after a restart the GPU can sit at its idle boost (~2400 MHz on the GB10) for a moment
/// after `nvidia-smi -lgc` and until the worker loads it; an uncapped GPU stays there for good.
pub const CAP_EXCEED_FOR: Duration = Duration::from_secs(3);
/// How long an exceedance keeps the verdict at "uncapped". Once the clock has stayed under the cap
/// for this long (the cap was installed meanwhile, or the reading was a transient), the verdict
/// goes back to the loaded-time evidence, so the dashboard banner clears without a restart.
pub const CAP_FORGET: Duration = Duration::from_secs(10 * 60);

/// What the telemetry says about the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapStatus {
    /// Not enough loaded time yet.
    Unknown,
    /// At least [`CAP_CONFIRM_LOADED`] of load without exceeding the cap.
    Capped { max_seen_mhz: u32 },
    /// The SM clock went above the cap: the unit is not installed or was reset.
    Uncapped { max_seen_mhz: u32 },
}

impl CapStatus {
    /// Stable identifier for logs and the API.
    pub const fn as_str(&self) -> &'static str {
        match self {
            CapStatus::Unknown => "unknown",
            CapStatus::Capped { .. } => "capped",
            CapStatus::Uncapped { .. } => "uncapped",
        }
    }
}

/// Accumulates evidence about the cap.
#[derive(Debug, Clone)]
pub struct ClockCapDetector {
    cap_mhz: u32,
    max_seen_mhz: u32,
    loaded_for: Duration,
    last_loaded_ts: Option<Duration>,
    /// Start of the current unbroken run of samples above the cap (with tolerance).
    exceed_since: Option<Duration>,
    /// Timestamp of the last confirmed exceedance (a run of at least [`CAP_EXCEED_FOR`]).
    last_exceeded_ts: Option<Duration>,
    /// Timestamp of the last sample, to age the exceedance.
    last_ts: Option<Duration>,
}

impl ClockCapDetector {
    /// A detector for a cap of `cap_mhz` (the profile's
    /// [`recommended_clock_cap_mhz`](crate::Profile::recommended_clock_cap_mhz)).
    pub fn new(cap_mhz: u32) -> Self {
        ClockCapDetector {
            cap_mhz,
            max_seen_mhz: 0,
            loaded_for: Duration::ZERO,
            last_loaded_ts: None,
            exceed_since: None,
            last_exceeded_ts: None,
            last_ts: None,
        }
    }

    /// The cap this detector checks against, in MHz.
    pub fn cap_mhz(&self) -> u32 {
        self.cap_mhz
    }

    /// Feeds one sample with the duty in force while it was taken.
    pub fn observe(&mut self, s: &Sample, duty_pct: u8) {
        self.max_seen_mhz = self.max_seen_mhz.max(s.sm_mhz);
        self.last_ts = Some(s.ts);
        if s.sm_mhz > self.cap_mhz + CAP_TOLERANCE_MHZ {
            let since = *self.exceed_since.get_or_insert(s.ts);
            if s.ts.saturating_sub(since) >= CAP_EXCEED_FOR {
                self.last_exceeded_ts = Some(s.ts);
            }
        } else {
            self.exceed_since = None;
        }
        let loaded = s.worker_active && duty_pct >= CAP_LOADED_MIN_DUTY_PCT;
        if loaded {
            if let Some(last) = self.last_loaded_ts {
                let dt = s.ts.saturating_sub(last);
                if dt <= MAX_SAMPLE_GAP {
                    self.loaded_for += dt;
                }
            }
            self.last_loaded_ts = Some(s.ts);
        } else {
            self.last_loaded_ts = None;
        }
    }

    /// The verdict so far.
    pub fn status(&self) -> CapStatus {
        let recently_exceeded = match (self.last_exceeded_ts, self.last_ts) {
            (Some(t), Some(now)) => now.saturating_sub(t) < CAP_FORGET,
            _ => false,
        };
        if recently_exceeded {
            CapStatus::Uncapped { max_seen_mhz: self.max_seen_mhz }
        } else if self.loaded_for >= CAP_CONFIRM_LOADED {
            CapStatus::Capped { max_seen_mhz: self.max_seen_mhz }
        } else {
            CapStatus::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_s: u64, sm_mhz: u32) -> Sample {
        Sample { ts: Duration::from_secs(ts_s), power_w: 60.0, temp_gpu_c: 70.0, temp_acpitz_c: Some(80.0), sm_mhz, worker_active: true }
    }

    #[test]
    fn one_transient_reading_above_the_cap_is_not_an_absent_cap() {
        let mut d = ClockCapDetector::new(2000);
        d.observe(&sample(0, 2411), 100); // the boost caught right after a restart
        for t in 1..=40 {
            d.observe(&sample(t, 1980), 100);
        }
        assert!(matches!(d.status(), CapStatus::Capped { max_seen_mhz: 2411 }), "{:?}", d.status());
    }

    #[test]
    fn sustained_readings_above_the_cap_mean_uncapped_until_it_stays_under_for_a_while() {
        let mut d = ClockCapDetector::new(2000);
        for t in 0..=3 {
            d.observe(&sample(t, 2400), 100);
        }
        assert!(matches!(d.status(), CapStatus::Uncapped { max_seen_mhz: 2400 }));
        // Under the cap again (the cap unit was installed): still "uncapped" for CAP_FORGET…
        for t in 4..(4 + CAP_FORGET.as_secs() - 1) {
            d.observe(&sample(t, 1990), 100);
        }
        assert!(matches!(d.status(), CapStatus::Uncapped { .. }));
        // …then the verdict follows the loaded-time evidence again.
        d.observe(&sample(4 + CAP_FORGET.as_secs() + 1, 1990), 100);
        assert!(matches!(d.status(), CapStatus::Capped { .. }), "{:?}", d.status());
    }

    #[test]
    fn short_runs_above_the_cap_do_not_count() {
        let mut d = ClockCapDetector::new(2000);
        d.observe(&sample(0, 2400), 100);
        d.observe(&sample(2, 2400), 100); // 2 s above, then under: the run is broken
        d.observe(&sample(3, 1990), 100);
        d.observe(&sample(4, 2400), 100);
        d.observe(&sample(6, 2400), 100);
        assert!(!matches!(d.status(), CapStatus::Uncapped { .. }));
        d.observe(&sample(7, 2400), 100); // 3 s without a break
        assert!(matches!(d.status(), CapStatus::Uncapped { .. }));
    }
}
