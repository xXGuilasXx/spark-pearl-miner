//! SM clock and board power for the `Stats` frames, from NVML (read-only queries; the worker
//! already holds the CUDA context, NVML adds none). Missing NVML reads as zeros.

use nvml_wrapper::enum_wrappers::device::Clock;
use nvml_wrapper::Nvml;

pub struct Telemetry {
    nvml: Nvml,
}

impl Telemetry {
    /// Loads libnvidia-ml.so; `None` when it is missing or device 0 does not exist.
    pub fn new() -> Option<Self> {
        let nvml = Nvml::init().ok()?;
        nvml.device_by_index(0).ok()?;
        Some(Self { nvml })
    }

    /// `(sm_clock_mhz, power_w)`; a field NVML cannot read is 0.
    pub fn read(&self) -> (u32, f32) {
        let Ok(dev) = self.nvml.device_by_index(0) else {
            return (0, 0.0);
        };
        let clock = dev.clock_info(Clock::SM).unwrap_or(0);
        let power = dev
            .power_usage()
            .map(|mw| mw as f32 / 1000.0)
            .unwrap_or(0.0);
        (clock, power)
    }
}
