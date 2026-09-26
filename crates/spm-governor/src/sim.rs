//! A small first-order model of the GB10 power and thermal response, used to test the
//! controller and to dry-run the governor without a GPU.
//!
//! * Power settles towards `idle + duty · (load − idle) · (1 + leak · (T − 50 °C))` with a
//!   first-order lag (`tau_power_s`): the NVML reading is an average, and leakage makes a hot
//!   die draw more for the same work.
//! * Temperature settles towards `ambient + R_th · P` with a slower lag (`tau_thermal_s`).
//! * The reading carries uniform noise of ±`noise_w` from a seeded xorshift, so runs are
//!   reproducible.
//!
//! The defaults are rough GB10 numbers: 15 W idle (measured with a resident vLLM), ~100 W at
//! full duty on a cool die (a real mining kernel adds shared-memory and L2 traffic to the 51 W
//! register-only peak of MB1), 0.4 °C/W and a 60 s thermal time constant.

use std::time::Duration;

use crate::governor::{Decision, Governor};
use crate::{Sample, SAMPLE_PERIOD};

/// Plant parameters. Change them between steps to inject disturbances.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlantParams {
    /// GPU power with our worker idle, in watts.
    pub idle_w: f64,
    /// GPU power at 100 % duty with the die at 50 °C, in watts.
    pub load_w_at_50c: f64,
    /// Relative increase of the load-dependent power per °C above 50 °C.
    pub leak_per_c: f64,
    /// Time constant of the power reading, in seconds.
    pub tau_power_s: f64,
    /// Ambient temperature, in °C.
    pub ambient_c: f64,
    /// Thermal resistance die-to-ambient, in °C per watt.
    pub r_th_c_per_w: f64,
    /// Thermal time constant, in seconds.
    pub tau_thermal_s: f64,
    /// SM clock while computing, in MHz.
    pub sm_mhz_loaded: u32,
    /// SM clock while idle, in MHz.
    pub sm_mhz_idle: u32,
    /// Half-width of the uniform noise on the power reading, in watts.
    pub noise_w: f64,
    /// `acpitz` reads this much below the GPU sensor, in °C.
    pub acpitz_below_gpu_c: f64,
}

impl Default for PlantParams {
    fn default() -> Self {
        PlantParams {
            idle_w: 15.0,
            load_w_at_50c: 100.0,
            leak_per_c: 0.004,
            tau_power_s: 0.5,
            ambient_c: 30.0,
            r_th_c_per_w: 0.4,
            tau_thermal_s: 60.0,
            sm_mhz_loaded: 2200,
            sm_mhz_idle: 2200,
            noise_w: 0.8,
            acpitz_below_gpu_c: 8.0,
        }
    }
}

/// The simulated GPU.
#[derive(Debug, Clone)]
pub struct Plant {
    /// Parameters; public so tests can inject disturbances mid-run.
    pub params: PlantParams,
    power_w: f64,
    temp_c: f64,
    computing: bool,
    rng: u64,
}

impl Plant {
    /// A plant at idle, in thermal equilibrium. `seed` drives the reading noise.
    pub fn new(params: PlantParams, seed: u64) -> Self {
        Plant {
            params,
            power_w: params.idle_w,
            temp_c: params.ambient_c + params.r_th_c_per_w * params.idle_w,
            computing: false,
            rng: seed | 1,
        }
    }

    /// Power the plant settles to at `duty_pct` and the current temperature.
    pub fn steady_power_w(&self, duty_pct: f64) -> f64 {
        let p = &self.params;
        let leak = 1.0 + p.leak_per_c * (self.temp_c - 50.0);
        p.idle_w + duty_pct.clamp(0.0, 100.0) / 100.0 * (p.load_w_at_50c - p.idle_w) * leak
    }

    /// Runs the plant for `dt` at `duty_pct` (0 when the worker is paused or idle).
    pub fn advance(&mut self, duty_pct: f64, dt: Duration) {
        let dt_s = dt.as_secs_f64();
        let target_p = self.steady_power_w(duty_pct);
        self.power_w += (target_p - self.power_w) * (1.0 - (-dt_s / self.params.tau_power_s).exp());
        let target_t = self.params.ambient_c + self.params.r_th_c_per_w * self.power_w;
        self.temp_c += (target_t - self.temp_c) * (1.0 - (-dt_s / self.params.tau_thermal_s).exp());
        self.computing = duty_pct > 0.0;
    }

    /// The noiseless power, in watts.
    pub fn true_power_w(&self) -> f64 {
        self.power_w
    }

    /// The die temperature, in °C.
    pub fn temp_c(&self) -> f64 {
        self.temp_c
    }

    /// A telemetry sample at `ts` (the noisy power reading, both temperatures, the clock).
    pub fn sample(&mut self, ts: Duration, worker_active: bool) -> Sample {
        let noise = (self.next_unit() * 2.0 - 1.0) * self.params.noise_w;
        Sample {
            ts,
            power_w: self.power_w + noise,
            temp_gpu_c: self.temp_c,
            temp_acpitz_c: Some(self.temp_c - self.params.acpitz_below_gpu_c),
            sm_mhz: if self.computing {
                self.params.sm_mhz_loaded
            } else {
                self.params.sm_mhz_idle
            },
            worker_active,
        }
    }

    /// Uniform in [0, 1) from xorshift64.
    fn next_unit(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One tick of a closed-loop simulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimStep {
    pub sample: Sample,
    pub decision: Decision,
    /// The plant's noiseless power when the sample was taken.
    pub true_power_w: f64,
}

/// Runs the governor against the plant at [`SAMPLE_PERIOD`] for `duration`, starting at `start`.
/// `worker_wants_to_run` is the coexistence side (false: the worker yields, duty 0).
pub fn simulate(
    governor: &mut Governor,
    plant: &mut Plant,
    start: Duration,
    duration: Duration,
    worker_wants_to_run: bool,
) -> Vec<SimStep> {
    let steps = (duration.as_millis() / SAMPLE_PERIOD.as_millis()) as usize;
    let mut out = Vec::with_capacity(steps);
    let mut mining = worker_wants_to_run && governor.trip().is_none();
    for i in 0..steps {
        let ts = start + SAMPLE_PERIOD * i as u32;
        let true_power_w = plant.true_power_w();
        let sample = plant.sample(ts, mining);
        let decision = governor.step(&sample);
        mining = worker_wants_to_run && decision.should_mine();
        let duty = if mining { f64::from(decision.duty_pct) } else { 0.0 };
        plant.advance(duty, SAMPLE_PERIOD);
        out.push(SimStep { sample, decision, true_power_w });
    }
    out
}
