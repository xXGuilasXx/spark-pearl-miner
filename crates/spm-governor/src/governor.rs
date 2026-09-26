//! The duty-cycle controller and its trips.

use std::fmt;
use std::time::Duration;

use crate::fault::{FaultDetector, FaultSignature};
use crate::profile::{Profile, ProfileLimits};
use crate::{Sample, MAX_SAMPLE_GAP};

/// Lowest duty the controller commands while mining, in percent.
pub const DUTY_MIN_PCT: u8 = 10;
/// Highest duty, in percent.
pub const DUTY_MAX_PCT: u8 = 100;
/// Proportional gain, percent of duty per watt of error.
pub const KP_PCT_PER_W: f64 = 0.5;
/// Integral gain, percent of duty per watt-second of error. `KI/KP` equals the inverse of the
/// ~1 s power response of the GPU, which cancels the plant pole and gives a ~2 s first-order
/// closed loop without overshoot.
pub const KI_PCT_PER_W_S: f64 = 0.5;
/// Largest duty increase per second. Decreases are never limited. A full ramp from 10 % to
/// 100 % takes 4.5 s, so a start or a resume cannot jump straight to full load.
pub const DUTY_RAMP_PCT_PER_S: f64 = 20.0;

/// Consecutive samples above the hard stop that trip [`TripReason::OverPower`].
pub const OVER_POWER_SAMPLES: u32 = 3;
/// Pause after an over-power trip.
pub const OVER_POWER_PAUSE: Duration = Duration::from_secs(60);
/// GPU temperature that trips [`TripReason::GpuOverTemp`] when exceeded, in °C.
pub const GPU_TEMP_TRIP_C: f64 = 83.0;
/// `acpitz` temperature that trips [`TripReason::AcpitzOverTemp`] when exceeded, in °C.
pub const ACPITZ_TRIP_C: f64 = 95.0;
/// Minimum pause after an over-temperature trip.
pub const OVER_TEMP_PAUSE: Duration = Duration::from_secs(60);
/// After any pause, mining resumes only once both temperatures are this far below their trips.
pub const TEMP_RESUME_MARGIN_C: f64 = 5.0;
/// Above this GPU temperature the power target is lowered, in °C ...
pub const GPU_TEMP_DERATE_START_C: f64 = 78.0;
/// ... by this many watts per °C, so the controller backs off before the 83 °C trip.
pub const DERATE_W_PER_C: f64 = 3.0;
/// Samples (2 s at 10 Hz) during which the previous hard stop still applies after a switch to
/// a lower profile.
pub const PROFILE_SWITCH_GRACE_SAMPLES: u32 = 20;
/// After the worker has been idle this long (yielding, released), the next start ramps up
/// from [`DUTY_MIN_PCT`] again.
pub const IDLE_RESTART: Duration = Duration::from_secs(30);

/// Why the governor is holding the worker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TripReason {
    /// Power above the profile's hard stop on [`OVER_POWER_SAMPLES`] consecutive samples.
    OverPower { power_w: f64, hard_stop_w: f64 },
    /// GPU temperature above [`GPU_TEMP_TRIP_C`].
    GpuOverTemp { temp_c: f64, limit_c: f64 },
    /// `acpitz` temperature above [`ACPITZ_TRIP_C`].
    AcpitzOverTemp { temp_c: f64, limit_c: f64 },
    /// A hardware fault signature: stop mining and alert until [`Governor::clear_fault`].
    Fault(FaultSignature),
}

/// A trip in force.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trip {
    pub reason: TripReason,
    /// When it fired.
    pub since: Duration,
    /// Earliest time mining may resume; `None` for a fault, which holds until cleared.
    pub resume_not_before: Option<Duration>,
}

impl Trip {
    /// The fault signature, when this trip is a fault (the daemon stops the worker).
    pub fn fault(&self) -> Option<FaultSignature> {
        match self.reason {
            TripReason::Fault(sig) => Some(sig),
            _ => None,
        }
    }

    /// Whether this trip stops mining (a fault) rather than pausing it.
    pub fn is_fault(&self) -> bool {
        self.fault().is_some()
    }
}

impl fmt::Display for Trip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TripReason::OverPower { power_w, hard_stop_w } => write!(
                f,
                "GPU power {power_w:.1} W above the {hard_stop_w:.0} W hard stop: mining paused for 60 s"
            ),
            TripReason::GpuOverTemp { temp_c, limit_c } => write!(
                f,
                "GPU at {temp_c:.0} °C (limit {limit_c:.0} °C): mining paused until it cools down"
            ),
            TripReason::AcpitzOverTemp { temp_c, limit_c } => write!(
                f,
                "board (acpitz) at {temp_c:.0} °C (limit {limit_c:.0} °C): mining paused until it cools down"
            ),
            TripReason::Fault(sig) => f.write_str(sig.alert()),
        }
    }
}

/// What the daemon applies after one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Duty cycle for the worker, `DUTY_MIN_PCT..=DUTY_MAX_PCT`. While a trip holds, this is the
    /// duty the worker will restart with.
    pub duty_pct: u8,
    /// `Some` while a trip holds the worker: pause it, or stop it for a fault.
    pub trip: Option<Trip>,
    /// `true` only on the sample where the trip fired: log it and raise the alert once.
    pub fired: bool,
}

impl Decision {
    /// Whether the worker may compute.
    pub fn should_mine(&self) -> bool {
        self.trip.is_none()
    }
}

/// The power governor. Feed it one [`Sample`] per telemetry tick with [`Governor::step`].
#[derive(Debug, Clone)]
pub struct Governor {
    profile: Profile,
    limits: ProfileLimits,
    duty: f64,
    prev_err: Option<f64>,
    last_ts: Option<Duration>,
    over_power_streak: u32,
    trip: Option<Trip>,
    faults: FaultDetector,
    inactive_since: Option<Duration>,
    /// After a switch to a lower profile, the previous (higher) hard stop still applies for
    /// this many samples while the power comes down.
    switch_grace: Option<(u32, f64)>,
}

impl Governor {
    /// A governor for `profile`, starting at [`DUTY_MIN_PCT`]. Validate the profile with
    /// [`Profile::select`] first (Max needs the acknowledgement flag).
    pub fn new(profile: Profile) -> Self {
        Governor {
            profile,
            limits: profile.limits(),
            duty: f64::from(DUTY_MIN_PCT),
            prev_err: None,
            last_ts: None,
            over_power_streak: 0,
            trip: None,
            faults: FaultDetector::new(),
            inactive_since: None,
            switch_grace: None,
        }
    }

    /// The active profile.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Switches profile on the fly and steers to the new target. Going down, the duty is cut in
    /// proportion to the targets at once and the previous hard stop stays in force for
    /// [`PROFILE_SWITCH_GRACE_SAMPLES`] while the power comes down, so a switch from the GUI
    /// does not trip the new, lower stop.
    pub fn set_profile(&mut self, profile: Profile) {
        let old = self.limits;
        let new = profile.limits();
        if new.target_w < old.target_w {
            self.duty = (self.duty * new.target_w / old.target_w).max(f64::from(DUTY_MIN_PCT));
            self.switch_grace = Some((PROFILE_SWITCH_GRACE_SAMPLES, old.hard_stop_w));
        } else {
            self.switch_grace = None;
        }
        self.profile = profile;
        self.limits = new;
        self.prev_err = None;
        self.over_power_streak = 0;
    }

    /// The current duty, in percent.
    pub fn duty_pct(&self) -> u8 {
        // The clamp keeps the cast in range.
        self.duty.round().clamp(f64::from(DUTY_MIN_PCT), f64::from(DUTY_MAX_PCT)) as u8
    }

    /// The trip in force, if any.
    pub fn trip(&self) -> Option<Trip> {
        self.trip
    }

    /// Clears a latched fault after the user acknowledged it. Returns whether there was one.
    /// Mining restarts from [`DUTY_MIN_PCT`].
    pub fn clear_fault(&mut self) -> bool {
        if self.trip.is_some_and(|t| t.is_fault()) {
            self.trip = None;
            self.faults = FaultDetector::new();
            self.restart_ramp();
            true
        } else {
            false
        }
    }

    /// The power target for this sample: the profile target, lowered by
    /// [`DERATE_W_PER_C`] per °C of GPU temperature above [`GPU_TEMP_DERATE_START_C`].
    pub fn effective_target_w(&self, s: &Sample) -> f64 {
        let excess_c = (s.temp_gpu_c - GPU_TEMP_DERATE_START_C).max(0.0);
        (self.limits.target_w - excess_c * DERATE_W_PER_C).max(0.0)
    }

    /// Consumes one sample and returns what to do until the next one.
    pub fn step(&mut self, s: &Sample) -> Decision {
        let dt = self.last_ts.map_or(Duration::ZERO, |t| s.ts.saturating_sub(t));
        self.last_ts = Some(s.ts);
        let gap = dt > MAX_SAMPLE_GAP;

        // 1. A latched fault holds until the user clears it.
        if self.trip.is_some_and(|t| t.is_fault()) {
            return self.holding();
        }
        // The detector sees the duty that was in force while the sample was taken.
        if let Some(sig) = self.faults.observe(s, self.duty) {
            return self.fire(Trip {
                reason: TripReason::Fault(sig),
                since: s.ts,
                resume_not_before: None,
            });
        }

        // 2. A pause in force: resume once its time is up and both temperatures have cooled.
        //    The sample was taken while paused, so it says nothing about our duty: resume at
        //    the minimum and let the next samples drive the ramp.
        if let Some(trip) = self.trip {
            let time_up = trip.resume_not_before.is_some_and(|t| s.ts >= t);
            if time_up && temps_allow_resume(s) {
                self.trip = None;
                self.over_power_streak = 0;
                self.inactive_since = None;
                self.restart_ramp();
                return self.running();
            }
            return self.holding();
        }

        // 3. New trips. Temperatures and power count even when our worker is idle: if the GPU is
        //    already that hot or that loaded, we must not add to it.
        if s.temp_gpu_c > GPU_TEMP_TRIP_C {
            return self.fire(Trip {
                reason: TripReason::GpuOverTemp { temp_c: s.temp_gpu_c, limit_c: GPU_TEMP_TRIP_C },
                since: s.ts,
                resume_not_before: Some(s.ts + OVER_TEMP_PAUSE),
            });
        }
        if let Some(t) = s.temp_acpitz_c.filter(|t| *t > ACPITZ_TRIP_C) {
            return self.fire(Trip {
                reason: TripReason::AcpitzOverTemp { temp_c: t, limit_c: ACPITZ_TRIP_C },
                since: s.ts,
                resume_not_before: Some(s.ts + OVER_TEMP_PAUSE),
            });
        }
        let hard_stop_w = match self.switch_grace {
            Some((left, old_stop_w)) if left > 0 => {
                self.switch_grace = Some((left - 1, old_stop_w));
                old_stop_w.max(self.limits.hard_stop_w)
            }
            _ => {
                self.switch_grace = None;
                self.limits.hard_stop_w
            }
        };
        if s.power_w > hard_stop_w {
            self.over_power_streak += 1;
        } else {
            self.over_power_streak = 0;
        }
        if self.over_power_streak >= OVER_POWER_SAMPLES {
            return self.fire(Trip {
                reason: TripReason::OverPower { power_w: s.power_w, hard_stop_w },
                since: s.ts,
                resume_not_before: Some(s.ts + OVER_POWER_PAUSE),
            });
        }

        // 4. The PI controller only adapts while the worker computes (otherwise the power
        //    reading says nothing about our duty).
        if !s.worker_active {
            self.prev_err = None;
            self.inactive_since.get_or_insert(s.ts);
            return self.running();
        }
        if let Some(since) = self.inactive_since.take() {
            if s.ts.saturating_sub(since) >= IDLE_RESTART {
                self.restart_ramp();
            }
        }
        let dt_s = if gap {
            self.restart_ramp();
            0.0
        } else {
            dt.as_secs_f64()
        };

        let err = self.effective_target_w(s) - s.power_w;
        let prev = self.prev_err.unwrap_or(err);
        // Velocity form: the duty itself is the integrator state, so clamping it is the
        // anti-windup.
        let mut u = self.duty + KP_PCT_PER_W * (err - prev) + KI_PCT_PER_W_S * err * dt_s;
        u = u.min(self.duty + DUTY_RAMP_PCT_PER_S * dt_s);
        self.duty = u.clamp(f64::from(DUTY_MIN_PCT), f64::from(DUTY_MAX_PCT));
        self.prev_err = Some(err);
        self.running()
    }

    fn restart_ramp(&mut self) {
        self.duty = f64::from(DUTY_MIN_PCT);
        self.prev_err = None;
    }

    fn fire(&mut self, trip: Trip) -> Decision {
        self.trip = Some(trip);
        self.over_power_streak = 0;
        self.restart_ramp();
        Decision { duty_pct: DUTY_MIN_PCT, trip: Some(trip), fired: true }
    }

    fn holding(&self) -> Decision {
        Decision { duty_pct: DUTY_MIN_PCT, trip: self.trip, fired: false }
    }

    fn running(&self) -> Decision {
        Decision { duty_pct: self.duty_pct(), trip: None, fired: false }
    }
}

fn temps_allow_resume(s: &Sample) -> bool {
    s.temp_gpu_c <= GPU_TEMP_TRIP_C - TEMP_RESUME_MARGIN_C
        && s.temp_acpitz_c.is_none_or(|t| t <= ACPITZ_TRIP_C - TEMP_RESUME_MARGIN_C)
}
