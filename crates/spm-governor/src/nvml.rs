//! NVML telemetry for the governor (cargo feature `nvml`).
//!
//! Read-only queries only: power, SM clock, GPU temperature, clock event reasons and
//! per-process SM utilization. NVML does not create a CUDA context, so the daemon can sample
//! at 10 Hz without ever showing up as a compute process. `libnvidia-ml.so` is loaded at
//! runtime by `nvml-wrapper`.

use std::path::Path;
use std::time::Duration;

use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::Nvml;

use crate::thermal;
use crate::Sample;

/// One NVML reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NvmlReading {
    /// GPU power draw, in watts.
    pub power_w: f64,
    /// GPU temperature, in °C.
    pub temp_gpu_c: f64,
    /// Current SM clock, in MHz.
    pub sm_mhz: u32,
    /// Clock event (throttle) reason bits, as `nvidia-smi` shows in `clocks_event_reasons.active`;
    /// `None` when the device does not report them.
    pub event_reasons: Option<u64>,
}

/// SM utilization of one process over the last sampling window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessSm {
    pub pid: u32,
    /// SM utilization, percent.
    pub sm_pct: u32,
    /// Whether the process holds a compute context (not just graphics, like the compositor).
    pub compute: bool,
}

/// The sampler. Holds the NVML handle; the device is looked up per call (cheap).
pub struct NvmlSampler {
    nvml: Nvml,
    index: u32,
    last_util_ts_us: u64,
}

impl NvmlSampler {
    /// Loads NVML and checks that device `index` exists.
    pub fn init(index: u32) -> Result<Self, NvmlError> {
        let nvml = Nvml::init()?;
        nvml.device_by_index(index)?;
        Ok(NvmlSampler { nvml, index, last_util_ts_us: 0 })
    }

    /// Power, temperature, SM clock and clock event reasons.
    pub fn read(&self) -> Result<NvmlReading, NvmlError> {
        let dev = self.nvml.device_by_index(self.index)?;
        Ok(NvmlReading {
            power_w: f64::from(dev.power_usage()?) / 1000.0,
            temp_gpu_c: f64::from(dev.temperature(TemperatureSensor::Gpu)?),
            sm_mhz: dev.clock_info(Clock::SM)?,
            event_reasons: dev.current_throttle_reasons().ok().map(|r| r.bits()),
        })
    }

    /// A governor [`Sample`]: NVML plus the hottest `acpitz` zone under `thermal_root`
    /// (normally [`thermal::THERMAL_ROOT`]).
    pub fn sample(
        &self,
        ts: Duration,
        worker_active: bool,
        thermal_root: &Path,
    ) -> Result<Sample, NvmlError> {
        let r = self.read()?;
        Ok(Sample {
            ts,
            power_w: r.power_w,
            temp_gpu_c: r.temp_gpu_c,
            temp_acpitz_c: thermal::read_acpitz_max_c(thermal_root).ok().flatten(),
            sm_mhz: r.sm_mhz,
            worker_active,
        })
    }

    /// SM utilization per process since the previous call (the coexistence fallback when the
    /// vLLM metrics endpoint is unavailable). Processes without samples in the window are
    /// omitted.
    pub fn process_sm_util(&mut self) -> Result<Vec<ProcessSm>, NvmlError> {
        let dev = self.nvml.device_by_index(self.index)?;
        let compute: Vec<u32> = dev.running_compute_processes()?.iter().map(|p| p.pid).collect();
        let samples = match dev.process_utilization_stats(self.last_util_ts_us) {
            Ok(s) => s,
            // NVML answers NotFound when no process had a sample in the window.
            Err(NvmlError::NotFound) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut out: Vec<ProcessSm> = Vec::new();
        for s in samples {
            self.last_util_ts_us = self.last_util_ts_us.max(s.timestamp);
            match out.iter_mut().find(|p| p.pid == s.pid) {
                Some(p) => p.sm_pct = p.sm_pct.max(s.sm_util),
                None => out.push(ProcessSm {
                    pid: s.pid,
                    sm_pct: s.sm_util,
                    compute: compute.contains(&s.pid),
                }),
            }
        }
        Ok(out)
    }
}
