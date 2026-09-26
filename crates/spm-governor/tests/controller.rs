//! Closed-loop tests of the PI duty controller against the plant model in `spm_governor::sim`.

use std::time::Duration;

use spm_governor::sim::{simulate, Plant, PlantParams, SimStep};
use spm_governor::{Governor, Profile, DUTY_MAX_PCT, DUTY_MIN_PCT, SAMPLE_PERIOD};

const TOLERANCE_W: f64 = 3.0;
const SETTLE: Duration = Duration::from_secs(20);
const THIRTY_MIN: Duration = Duration::from_secs(30 * 60);

fn settle_index() -> usize {
    (SETTLE.as_millis() / SAMPLE_PERIOD.as_millis()) as usize
}

/// Largest distance between the power reading (noise included) and the target after settling.
fn worst_error(steps: &[SimStep], target_w: f64) -> f64 {
    steps[settle_index()..].iter().map(|s| (s.sample.power_w - target_w).abs()).fold(0.0, f64::max)
}

fn no_trips(steps: &[SimStep]) -> bool {
    steps.iter().all(|s| s.decision.trip.is_none())
}

#[test]
fn every_profile_holds_its_target_within_3_w_for_30_minutes() {
    for (i, profile) in Profile::ALL.into_iter().enumerate() {
        let mut g = Governor::new(profile);
        let mut plant = Plant::new(PlantParams::default(), 0x5eed + i as u64);
        let steps = simulate(&mut g, &mut plant, Duration::ZERO, THIRTY_MIN, true);
        assert!(no_trips(&steps), "{profile}: unexpected trip");
        let worst = worst_error(&steps, profile.target_w());
        assert!(worst <= TOLERANCE_W, "{profile}: worst error {worst:.2} W");
        let peak = steps.iter().map(|s| s.true_power_w).fold(0.0, f64::max);
        assert!(peak < profile.hard_stop_w(), "{profile}: peak {peak:.1} W");
        // The mean sits on the target: the integrator removes the leakage drift.
        let tail = &steps[settle_index()..];
        let mean = tail.iter().map(|s| s.true_power_w).sum::<f64>() / tail.len() as f64;
        assert!((mean - profile.target_w()).abs() < 0.3, "{profile}: mean {mean:.2} W");
        let duty = steps.last().unwrap().decision.duty_pct;
        eprintln!(
            "{profile}: worst {worst:.2} W, mean {mean:.2} W, peak {peak:.1} W, final duty {duty} %, \
             die {:.1} °C",
            plant.temp_c()
        );
    }
}

#[test]
fn start_ramps_up_gradually_without_overshoot() {
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(PlantParams::default(), 7);
    let steps = simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(20), true);
    assert_eq!(steps[0].decision.duty_pct, DUTY_MIN_PCT);
    for w in steps.windows(2) {
        let up = i32::from(w[1].decision.duty_pct) - i32::from(w[0].decision.duty_pct);
        // 20 %/s at 10 Hz is 2 % per sample (+1 for rounding).
        assert!(up <= 3, "duty jumped by {up}");
    }
    let peak = steps.iter().map(|s| s.true_power_w).fold(0.0, f64::max);
    assert!(peak < Profile::Balanced.target_w() + TOLERANCE_W, "overshoot to {peak:.1} W");
    // Within 3 W of the target well inside the settling window.
    let first_in_band = steps
        .iter()
        .position(|s| (s.true_power_w - 75.0).abs() <= TOLERANCE_W)
        .expect("never reached the band");
    assert!(first_in_band < 120, "took {first_in_band} samples");
}

#[test]
fn a_slower_or_noisier_power_reading_is_still_held_in_the_band() {
    // NVML's averaging window on GB10 is not documented: check 4x slower and 2x noisier.
    for (tau, noise) in [(2.0, 0.8), (0.5, 1.6)] {
        for profile in Profile::ALL {
            let params = PlantParams { tau_power_s: tau, noise_w: noise, ..PlantParams::default() };
            let mut g = Governor::new(profile);
            let mut plant = Plant::new(params, 29);
            let steps =
                simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(600), true);
            assert!(no_trips(&steps), "{profile} tau {tau}");
            let tail = &steps[2 * settle_index()..];
            let worst = tail
                .iter()
                .map(|s| (s.true_power_w - profile.target_w()).abs())
                .fold(0.0, f64::max);
            assert!(worst <= TOLERANCE_W, "{profile} tau {tau} noise {noise}: worst {worst:.2} W");
        }
    }
}

#[test]
fn hotter_ambient_is_absorbed_by_the_integrator() {
    let params = PlantParams { ambient_c: 38.0, ..PlantParams::default() };
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(params, 11);
    let steps = simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(600), true);
    assert!(no_trips(&steps));
    assert!(worst_error(&steps, 75.0) <= TOLERANCE_W);
}

#[test]
fn heavier_job_mid_run_recovers_within_seconds_and_never_hits_the_hard_stop() {
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(PlantParams::default(), 13);
    let first = simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(300), true);
    // A job whose kernel draws 12 % more at the same duty.
    plant.params.load_w_at_50c *= 1.12;
    let second =
        simulate(&mut g, &mut plant, Duration::from_secs(300), Duration::from_secs(300), true);
    assert!(no_trips(&first) && no_trips(&second));
    let peak = second.iter().map(|s| s.true_power_w).fold(0.0, f64::max);
    assert!(peak < 85.0, "peak {peak:.1} W");
    // Back inside the band within 10 s and for good.
    let back = &second[100..];
    let worst = back.iter().map(|s| (s.sample.power_w - 75.0).abs()).fold(0.0, f64::max);
    assert!(worst <= TOLERANCE_W, "worst {worst:.2} W after the disturbance");
}

#[test]
fn weak_plant_saturates_at_full_duty_without_tripping() {
    // A kernel that cannot reach the target (a lower clock cap, a lighter job): full duty and
    // no trip.
    let params = PlantParams { load_w_at_50c: 70.0, ..PlantParams::default() };
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(params, 17);
    let steps = simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(120), true);
    assert!(no_trips(&steps));
    assert_eq!(steps.last().unwrap().decision.duty_pct, DUTY_MAX_PCT);
}

#[test]
fn profile_change_on_the_fly_moves_to_the_new_target() {
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(PlantParams::default(), 19);
    simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(120), true);
    g.set_profile(Profile::Eco);
    let steps =
        simulate(&mut g, &mut plant, Duration::from_secs(120), Duration::from_secs(120), true);
    assert!(no_trips(&steps));
    assert!(worst_error(&steps, Profile::Eco.target_w()) <= TOLERANCE_W);
}

#[test]
fn yielding_holds_the_duty_and_a_long_idle_restarts_the_ramp() {
    let mut g = Governor::new(Profile::Balanced);
    let mut plant = Plant::new(PlantParams::default(), 23);
    let run = simulate(&mut g, &mut plant, Duration::ZERO, Duration::from_secs(120), true);
    let settled_duty = run.last().unwrap().decision.duty_pct;
    assert!(settled_duty > 50);

    // A short yield (vLLM busy for 5 s): the duty is held while the worker is idle.
    let idle =
        simulate(&mut g, &mut plant, Duration::from_secs(120), Duration::from_secs(5), false);
    assert!(idle.iter().all(|s| s.decision.duty_pct == settled_duty && s.decision.trip.is_none()));
    let back =
        simulate(&mut g, &mut plant, Duration::from_secs(125), Duration::from_millis(200), true);
    assert!(back[0].decision.duty_pct >= settled_duty - 5);

    // A long yield (> 30 s): the next start ramps from the minimum again.
    simulate(&mut g, &mut plant, Duration::from_secs(126), Duration::from_secs(40), false);
    let restart =
        simulate(&mut g, &mut plant, Duration::from_secs(166), Duration::from_millis(200), true);
    assert!(restart[0].decision.duty_pct <= DUTY_MIN_PCT + 2);
}
