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
    exceeded: bool,
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
            exceeded: false,
        }
    }

    /// The cap this detector checks against, in MHz.
    pub fn cap_mhz(&self) -> u32 {
        self.cap_mhz
    }

    /// Feeds one sample with the duty in force while it was taken.
    pub fn observe(&mut self, s: &Sample, duty_pct: u8) {
        self.max_seen_mhz = self.max_seen_mhz.max(s.sm_mhz);
        if s.sm_mhz > self.cap_mhz + CAP_TOLERANCE_MHZ {
            self.exceeded = true;
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
        if self.exceeded {
            CapStatus::Uncapped { max_seen_mhz: self.max_seen_mhz }
        } else if self.loaded_for >= CAP_CONFIRM_LOADED {
            CapStatus::Capped { max_seen_mhz: self.max_seen_mhz }
        } else {
            CapStatus::Unknown
        }
    }
}
