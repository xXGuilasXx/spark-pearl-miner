//! Known GB10 fault signatures. Each one means "this unit has a hardware or firmware problem";
//! the governor stops mining and raises an alert instead of trying to work around it.

use std::fmt;
use std::time::Duration;

use crate::{Sample, MAX_SAMPLE_GAP};

/// A recognised hardware fault pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultSignature {
    /// SM clock below 850 MHz with 5–15 W under load for more than 10 s: the USB-PD power
    /// negotiation failed and the unit is running on a fallback power budget.
    UsbPd,
    /// Power pinned at ~30 W with low clocks under load: the firmware "safety mode" that
    /// NVIDIA support treats as an RMA symptom.
    SafetyMode,
    /// Power pinned at ~100 W: the thermal cap is holding the GPU even though the governor
    /// asked for less (or something else is loading the GPU that hard).
    ThermalCap100W,
}

impl FaultSignature {
    /// Stable identifier for logs and the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            FaultSignature::UsbPd => "usb_pd",
            FaultSignature::SafetyMode => "safety_mode",
            FaultSignature::ThermalCap100W => "thermal_cap_100w",
        }
    }

    /// The alert shown to the user.
    pub const fn alert(self) -> &'static str {
        match self {
            FaultSignature::UsbPd => {
                "GPU stuck below 850 MHz at 5-15 W under load: USB-PD power negotiation failure. \
                 Mining stopped. Power-cycle the unit with the original power supply and cable; \
                 if it persists, contact NVIDIA support."
            }
            FaultSignature::SafetyMode => {
                "GPU pinned at ~30 W with low clocks under load: firmware safety mode (an RMA \
                 symptom). Mining stopped. Collect nvidia-bug-report.sh and contact NVIDIA support."
            }
            FaultSignature::ThermalCap100W => {
                "GPU pinned at ~100 W: thermal cap engaged. Mining stopped. Check airflow and \
                 dust, and that nothing else is loading the GPU."
            }
        }
    }
}

impl fmt::Display for FaultSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// USB-PD: SM clock strictly below this, in MHz.
pub const USB_PD_MAX_SM_MHZ: u32 = 850;
/// USB-PD: power band, in watts (inclusive).
pub const USB_PD_POWER_W: (f64, f64) = (5.0, 15.0);
/// USB-PD: the pattern must last longer than this.
pub const USB_PD_HOLD: Duration = Duration::from_secs(10);

/// Safety mode: the power it pins to, in watts.
pub const SAFETY_MODE_POWER_W: f64 = 30.0;
/// Safety mode: accepted distance from [`SAFETY_MODE_POWER_W`], in watts.
pub const SAFETY_MODE_TOLERANCE_W: f64 = 3.0;
/// Safety mode: SM clock strictly below this counts as "low clocks", in MHz.
pub const SAFETY_MODE_MAX_SM_MHZ: u32 = 1400;
/// Safety mode: the pattern must last longer than this (long, because a ramp from a pause
/// passes through ~30 W for a few seconds).
pub const SAFETY_MODE_HOLD: Duration = Duration::from_secs(30);

/// 100 W cap: the power it pins to, in watts.
pub const THERMAL_CAP_POWER_W: f64 = 100.0;
/// 100 W cap: accepted distance from [`THERMAL_CAP_POWER_W`], in watts.
pub const THERMAL_CAP_TOLERANCE_W: f64 = 4.0;
/// 100 W cap: the pattern must last longer than this.
pub const THERMAL_CAP_HOLD: Duration = Duration::from_secs(10);

/// A "pinned" pattern tolerates at most this peak-to-peak spread, in watts.
pub const PINNED_SPREAD_W: f64 = 4.0;
/// The load-dependent signatures only count while the commanded duty is at least this high
/// (at low duty the NVML clock reading often lands between bursts).
pub const LOADED_MIN_DUTY_PCT: f64 = 50.0;

/// How long a condition has held, and the power spread seen while it held.
#[derive(Debug, Clone, Copy, Default)]
struct Window {
    since: Option<Duration>,
    min_w: f64,
    max_w: f64,
}

impl Window {
    /// Feeds one sample; returns how long the condition has held (zero when it does not hold).
    fn update(&mut self, holds: bool, ts: Duration, power_w: f64, max_spread_w: f64) -> Duration {
        if !holds {
            *self = Window::default();
            return Duration::ZERO;
        }
        match self.since {
            None => self.restart(ts, power_w),
            Some(_) => {
                self.min_w = self.min_w.min(power_w);
                self.max_w = self.max_w.max(power_w);
                if self.max_w - self.min_w > max_spread_w {
                    self.restart(ts, power_w);
                }
            }
        }
        ts.saturating_sub(self.since.unwrap_or(ts))
    }

    fn restart(&mut self, ts: Duration, power_w: f64) {
        *self = Window { since: Some(ts), min_w: power_w, max_w: power_w };
    }
}

/// Watches the telemetry for the signatures above. Pure: time comes from the samples.
#[derive(Debug, Clone, Default)]
pub struct FaultDetector {
    usb_pd: Window,
    safety_mode: Window,
    thermal_cap: Window,
    last_ts: Option<Duration>,
}

impl FaultDetector {
    /// A detector with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one sample together with the duty that was in force while it was taken.
    /// Returns a signature the first time its pattern has lasted long enough (and again on
    /// later samples while it keeps holding; the governor latches the first one).
    pub fn observe(&mut self, s: &Sample, duty_pct: f64) -> Option<FaultSignature> {
        if let Some(last) = self.last_ts {
            if s.ts.saturating_sub(last) > MAX_SAMPLE_GAP {
                // A telemetry hole: nothing is known about the gap, start over.
                *self = FaultDetector::default();
            }
        }
        self.last_ts = Some(s.ts);

        let loaded = s.worker_active && duty_pct >= LOADED_MIN_DUTY_PCT;
        let p = s.power_w;

        let usb_pd = loaded
            && s.sm_mhz < USB_PD_MAX_SM_MHZ
            && (USB_PD_POWER_W.0..=USB_PD_POWER_W.1).contains(&p);
        let held = self.usb_pd.update(usb_pd, s.ts, p, f64::INFINITY);
        if held > USB_PD_HOLD {
            return Some(FaultSignature::UsbPd);
        }

        let safety = loaded
            && s.sm_mhz < SAFETY_MODE_MAX_SM_MHZ
            && (p - SAFETY_MODE_POWER_W).abs() <= SAFETY_MODE_TOLERANCE_W;
        let held = self.safety_mode.update(safety, s.ts, p, PINNED_SPREAD_W);
        if held > SAFETY_MODE_HOLD {
            return Some(FaultSignature::SafetyMode);
        }

        // No load condition: if the GPU sits at 100 W while we are paused, that is worse.
        let cap = (p - THERMAL_CAP_POWER_W).abs() <= THERMAL_CAP_TOLERANCE_W;
        let held = self.thermal_cap.update(cap, s.ts, p, PINNED_SPREAD_W);
        if held > THERMAL_CAP_HOLD {
            return Some(FaultSignature::ThermalCap100W);
        }
        None
    }
}
