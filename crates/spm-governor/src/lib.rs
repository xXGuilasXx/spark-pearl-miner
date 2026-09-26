//! spm-governor — keeps a mining DGX Spark out of its power-off region.
//!
//! GB10 has no software power limit (`nvidia-smi -pl` is unsupported) and some units power off
//! hard under sustained GPU load around 88–92 W. The levers we have are an SM clock cap
//! (`nvidia-smi -lgc 300,2200`, root, installed once by the optional boot unit) and the duty
//! cycle of our own worker. This crate is the policy for the second one:
//!
//! * [`Profile`]: Eco / **Balanced** (75 W target, 85 W hard stop, the default) / Max (88 W,
//!   92 W, needs an explicit acknowledgement), each with a recommended clock cap.
//! * [`Governor::step`]: a PI duty-cycle controller fed with 10 Hz telemetry ([`Sample`]),
//!   returning a [`Decision`] (duty 10–100 % plus an optional [`Trip`]). Trips: power above the
//!   hard stop on 3 consecutive samples, GPU above 83 °C or `acpitz` above 95 °C (pause 60 s);
//!   the GB10 [`FaultSignature`]s (alert and stop until cleared).
//! * [`marker`]: the `running.marker` file. A marker left by the previous run means it did not
//!   stop cleanly (possibly a power-off), so the next run steps the profile down one notch.
//! * [`ClockCapDetector`]: tells from the telemetry whether the clock cap is in force.
//! * [`thermal`]: the `acpitz` reader; `nvml` (cargo feature `nvml`): the NVML sampler.
//! * [`sim`]: a first-order power/thermal plant used by the tests and for dry runs.
//!
//! Everything except the two readers and the marker file helpers is pure: no threads, no clock,
//! no I/O. The daemon owns the 10 Hz loop, stamps samples with its monotonic clock and applies
//! the decisions (IPC `SetDuty`, `Pause`, `Release`).

#![forbid(unsafe_code)]

use std::time::Duration;

pub mod clockcap;
pub mod fault;
mod governor;
pub mod marker;
pub mod profile;
pub mod sim;
pub mod thermal;

#[cfg(feature = "nvml")]
pub mod nvml;

pub use clockcap::{CapStatus, ClockCapDetector};
pub use fault::{FaultDetector, FaultSignature};
pub use governor::*;
pub use profile::{Profile, ProfileError, ProfileLimits, DEFAULT_CLOCK_CAP_MHZ};

/// Telemetry period the controller is tuned for (NVML at 10 Hz).
pub const SAMPLE_PERIOD: Duration = Duration::from_millis(100);

/// A hole in the telemetry longer than this resets the controller ramp and the fault windows.
pub const MAX_SAMPLE_GAP: Duration = Duration::from_secs(2);

/// One telemetry sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// When the sample was taken, on the daemon's monotonic clock (arbitrary origin).
    pub ts: Duration,
    /// GPU power draw in watts (NVML `power.draw`).
    pub power_w: f64,
    /// GPU temperature in °C (NVML).
    pub temp_gpu_c: f64,
    /// Hottest `acpitz` thermal zone in °C, when readable.
    pub temp_acpitz_c: Option<f64>,
    /// Current SM clock in MHz (NVML).
    pub sm_mhz: u32,
    /// Whether our worker was computing during this sample (not paused, released or yielding).
    /// The controller only adapts, and the load-dependent fault signatures only count, while it
    /// is.
    pub worker_active: bool,
}
