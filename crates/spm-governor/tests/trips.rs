//! Trips and fault signatures on synthetic telemetry.

use std::time::Duration;

use spm_governor::fault::{FaultDetector, FaultSignature};
use spm_governor::{
    CapStatus, ClockCapDetector, Decision, Governor, Profile, Sample, TripReason, DUTY_MIN_PCT,
};

fn ms(t: u64) -> Duration {
    Duration::from_millis(t)
}

/// A normal loaded sample at `t_ms`: 70 W, 60 °C, acpitz 50 °C, 2200 MHz.
fn s(t_ms: u64) -> Sample {
    Sample {
        ts: ms(t_ms),
        power_w: 70.0,
        temp_gpu_c: 60.0,
        temp_acpitz_c: Some(50.0),
        sm_mhz: 2200,
        worker_active: true,
    }
}

fn with_power(t_ms: u64, power_w: f64) -> Sample {
    Sample { power_w, ..s(t_ms) }
}

/// Feeds samples every 100 ms from `from_ms` (inclusive) to `to_ms` (exclusive).
fn feed(g: &mut Governor, from_ms: u64, to_ms: u64, f: impl Fn(u64) -> Sample) -> Vec<Decision> {
    (from_ms..to_ms).step_by(100).map(|t| g.step(&f(t))).collect()
}

#[test]
fn three_consecutive_samples_above_the_hard_stop_pause_for_60_s() {
    let mut g = Governor::new(Profile::Balanced);
    assert!(g.step(&with_power(0, 86.0)).should_mine());
    assert!(g.step(&with_power(100, 86.0)).should_mine());
    let d = g.step(&with_power(200, 86.0));
    assert!(d.fired && !d.should_mine());
    let trip = d.trip.unwrap();
    assert!(
        matches!(trip.reason, TripReason::OverPower { hard_stop_w, .. } if hard_stop_w == 85.0)
    );
    assert_eq!(trip.resume_not_before, Some(ms(60_200)));
    assert!(!trip.is_fault());

    // Paused for 60 s (worker idle), whatever the power does.
    let held =
        feed(&mut g, 300, 60_200, |t| Sample { worker_active: false, ..with_power(t, 16.0) });
    assert!(held.iter().all(|d| !d.should_mine() && !d.fired && d.duty_pct == DUTY_MIN_PCT));
    // Resumes at the minimum duty.
    let d = g.step(&with_power(60_200, 16.0));
    assert!(d.should_mine());
    assert_eq!(d.duty_pct, DUTY_MIN_PCT);
}

#[test]
fn over_power_needs_consecutive_samples_strictly_above_the_stop() {
    let mut g = Governor::new(Profile::Balanced);
    let powers = [86.0, 86.0, 84.0, 86.0, 86.0, 85.0, 85.0, 85.0, 85.0];
    for (i, p) in powers.into_iter().enumerate() {
        assert!(g.step(&with_power(i as u64 * 100, p)).should_mine(), "sample {i}");
    }
}

#[test]
fn the_hard_stop_follows_the_profile() {
    let mut max = Governor::new(Profile::Max);
    let mut balanced = Governor::new(Profile::Balanced);
    for t in (0..500).step_by(100) {
        assert!(max.step(&with_power(t, 90.0)).should_mine());
    }
    let d = feed(&mut balanced, 0, 500, |t| with_power(t, 90.0));
    assert!(d.iter().any(|d| d.fired));
    let d = feed(&mut max, 500, 800, |t| with_power(t, 93.0));
    assert!(d[2].fired);
}

#[test]
fn switching_down_keeps_the_old_stop_for_2_s_then_applies_the_new_one() {
    let mut g = Governor::new(Profile::Balanced);
    feed(&mut g, 0, 1_000, |t| with_power(t, 75.0));
    g.set_profile(Profile::Eco);
    // 72 W is above the Eco stop (70 W) but below the Balanced one: tolerated for 20 samples.
    let grace = feed(&mut g, 1_000, 3_000, |t| with_power(t, 72.0));
    assert!(grace.iter().all(|d| d.should_mine()));
    let after = feed(&mut g, 3_000, 3_300, |t| with_power(t, 72.0));
    assert!(after[2].fired);
    assert!(matches!(
        after[2].trip.unwrap().reason,
        TripReason::OverPower { hard_stop_w, .. } if hard_stop_w == 70.0
    ));
}

#[test]
fn over_power_from_another_process_still_trips() {
    // vLLM loading a model pushes the GPU above the stop while our worker is idle.
    let mut g = Governor::new(Profile::Balanced);
    let d = feed(&mut g, 0, 300, |t| Sample { worker_active: false, ..with_power(t, 88.0) });
    assert!(d[2].fired);
}

#[test]
fn gpu_over_temperature_pauses_until_cool_and_60_s_elapsed() {
    let mut g = Governor::new(Profile::Balanced);
    assert!(g.step(&Sample { temp_gpu_c: 83.0, ..s(0) }).should_mine());
    let d = g.step(&Sample { temp_gpu_c: 83.4, ..s(100) });
    assert!(d.fired);
    assert!(matches!(d.trip.unwrap().reason, TripReason::GpuOverTemp { .. }));
    // After 60 s but still at 80 °C: stays paused.
    let d = g.step(&Sample { temp_gpu_c: 80.0, worker_active: false, ..s(60_200) });
    assert!(!d.should_mine() && !d.fired);
    // Cooled to 78 °C (trip minus 5): resumes.
    let d = g.step(&Sample { temp_gpu_c: 78.0, ..s(60_300) });
    assert!(d.should_mine());
}

#[test]
fn acpitz_over_temperature_pauses_and_a_missing_acpitz_is_ignored() {
    let mut g = Governor::new(Profile::Balanced);
    assert!(g.step(&Sample { temp_acpitz_c: None, ..s(0) }).should_mine());
    assert!(g.step(&Sample { temp_acpitz_c: Some(95.0), ..s(100) }).should_mine());
    let d = g.step(&Sample { temp_acpitz_c: Some(95.2), ..s(200) });
    assert!(d.fired);
    assert!(matches!(d.trip.unwrap().reason, TripReason::AcpitzOverTemp { .. }));
    // 60 s later with acpitz at 91 °C (above 95 − 5): still paused; at 90 °C it resumes.
    assert!(!g.step(&Sample { temp_acpitz_c: Some(91.0), ..s(60_200) }).should_mine());
    assert!(g.step(&Sample { temp_acpitz_c: Some(90.0), ..s(60_300) }).should_mine());
}

#[test]
fn hot_gpu_lowers_the_power_target_before_the_trip() {
    let g = Governor::new(Profile::Balanced);
    assert_eq!(g.effective_target_w(&Sample { temp_gpu_c: 70.0, ..s(0) }), 75.0);
    assert_eq!(g.effective_target_w(&Sample { temp_gpu_c: 80.0, ..s(0) }), 69.0);
    assert_eq!(g.effective_target_w(&Sample { temp_gpu_c: 83.0, ..s(0) }), 60.0);
}

// ---------------------------------------------------------------------------------------------
// Fault signatures
// ---------------------------------------------------------------------------------------------

fn usb_pd(t_ms: u64) -> Sample {
    Sample { power_w: 10.0, sm_mhz: 600, ..s(t_ms) }
}

#[test]
fn usb_pd_signature_needs_more_than_10_s_under_load() {
    let mut det = FaultDetector::new();
    for t in (0..=10_000).step_by(100) {
        assert_eq!(det.observe(&usb_pd(t), 100.0), None, "t={t}");
    }
    assert_eq!(det.observe(&usb_pd(10_100), 100.0), Some(FaultSignature::UsbPd));
}

#[test]
fn usb_pd_signature_ignores_idle_low_duty_and_normal_clocks() {
    let mut idle = FaultDetector::new();
    let mut low_duty = FaultDetector::new();
    let mut clocks_ok = FaultDetector::new();
    for t in (0..30_000).step_by(100) {
        assert_eq!(idle.observe(&Sample { worker_active: false, ..usb_pd(t) }, 100.0), None);
        assert_eq!(low_duty.observe(&usb_pd(t), 20.0), None);
        assert_eq!(clocks_ok.observe(&Sample { sm_mhz: 900, ..usb_pd(t) }, 100.0), None);
    }
}

#[test]
fn a_telemetry_gap_restarts_the_fault_windows() {
    let mut det = FaultDetector::new();
    for t in (0..6_000).step_by(100) {
        assert_eq!(det.observe(&usb_pd(t), 100.0), None);
    }
    // 3 s without samples, then 6 s more: never 10 s of continuous evidence.
    for t in (9_000..15_000).step_by(100) {
        assert_eq!(det.observe(&usb_pd(t), 100.0), None);
    }
}

#[test]
fn usb_pd_through_the_governor_latches_a_stop() {
    let mut g = Governor::new(Profile::Balanced);
    // The PI pushes the duty up (10 W is far below target); once it is past 50 % the 10 s
    // window starts.
    let decisions = feed(&mut g, 0, 20_000, usb_pd);
    let fired: Vec<_> = decisions.iter().enumerate().filter(|(_, d)| d.fired).collect();
    assert_eq!(fired.len(), 1);
    let (i, d) = fired[0];
    assert_eq!(d.trip.unwrap().reason, TripReason::Fault(FaultSignature::UsbPd));
    assert!(d.trip.unwrap().resume_not_before.is_none());
    assert!((100..140).contains(&i), "fired at sample {i}");
    // Latched: normal telemetry does not resume mining.
    let later = feed(&mut g, 20_000, 200_000, s);
    assert!(later.iter().all(|d| !d.should_mine() && !d.fired));
    assert!(g.clear_fault());
    assert!(g.step(&s(200_000)).should_mine());
    assert!(!g.clear_fault());
}

fn safety(t_ms: u64, power_w: f64) -> Sample {
    Sample { power_w, sm_mhz: 1000, ..s(t_ms) }
}

#[test]
fn safety_mode_signature_is_a_pinned_30_w_with_low_clocks_for_30_s() {
    let mut det = FaultDetector::new();
    let mut fired_at = None;
    for t in (0..40_000).step_by(100) {
        let p = if (t / 100) % 2 == 0 { 29.0 } else { 31.0 };
        if let Some(sig) = det.observe(&safety(t, p), 100.0) {
            fired_at = Some((t, sig));
            break;
        }
    }
    assert_eq!(fired_at, Some((30_100, FaultSignature::SafetyMode)));
}

#[test]
fn safety_mode_needs_a_pinned_power_and_low_clocks() {
    let mut wobbly = FaultDetector::new();
    let mut fast = FaultDetector::new();
    for t in (0..60_000).step_by(100) {
        // 27/33 W: inside the ±3 W band but a 6 W spread is not "pinned".
        let p = if (t / 100) % 2 == 0 { 27.0 } else { 33.0 };
        assert_eq!(wobbly.observe(&safety(t, p), 100.0), None);
        // 30 W at the cap clock is a light kernel, not safety mode.
        assert_eq!(fast.observe(&Sample { sm_mhz: 2200, ..safety(t, 30.0) }, 100.0), None);
    }
}

#[test]
fn thermal_cap_signature_escalates_an_over_power_pause_to_a_stop() {
    let mut g = Governor::new(Profile::Balanced);
    // Pinned at ~100 W: the over-power trip fires first, and the power stays there although our
    // worker is paused.
    let decisions = feed(&mut g, 0, 12_000, |t| Sample {
        worker_active: t < 300,
        power_w: if (t / 100) % 2 == 0 { 99.5 } else { 100.5 },
        ..s(t)
    });
    let fired: Vec<_> =
        decisions.iter().filter(|d| d.fired).map(|d| d.trip.unwrap().reason).collect();
    assert_eq!(fired.len(), 2);
    assert!(matches!(fired[0], TripReason::OverPower { .. }));
    assert_eq!(fired[1], TripReason::Fault(FaultSignature::ThermalCap100W));
    assert!(g.trip().unwrap().is_fault());
}

#[test]
fn fault_alerts_are_actionable() {
    for sig in [FaultSignature::UsbPd, FaultSignature::SafetyMode, FaultSignature::ThermalCap100W] {
        assert!(sig.alert().contains("Mining stopped"), "{sig}");
    }
}

// ---------------------------------------------------------------------------------------------
// Clock cap detection
// ---------------------------------------------------------------------------------------------

#[test]
fn clock_cap_is_confirmed_after_30_s_of_load_at_or_below_the_cap() {
    let mut det = ClockCapDetector::new(2200);
    for t in (0..29_900).step_by(100) {
        det.observe(&Sample { sm_mhz: 2190, ..s(t) }, 100);
    }
    assert_eq!(det.status(), CapStatus::Unknown);
    det.observe(&Sample { sm_mhz: 2200, ..s(30_000) }, 100);
    assert_eq!(det.status(), CapStatus::Capped { max_seen_mhz: 2200 });
}

#[test]
fn clock_above_the_cap_means_uncapped_even_at_idle() {
    let mut det = ClockCapDetector::new(2200);
    // A short run can be the idle boost caught right after a restart; 3 s without a break is not.
    det.observe(&Sample { sm_mhz: 2424, worker_active: false, ..s(0) }, DUTY_MIN_PCT);
    det.observe(&Sample { sm_mhz: 2424, worker_active: false, ..s(2_000) }, DUTY_MIN_PCT);
    assert_ne!(det.status(), CapStatus::Uncapped { max_seen_mhz: 2424 });
    det.observe(&Sample { sm_mhz: 2424, worker_active: false, ..s(3_000) }, DUTY_MIN_PCT);
    assert_eq!(det.status(), CapStatus::Uncapped { max_seen_mhz: 2424 });
}

#[test]
fn light_load_does_not_confirm_the_cap() {
    let mut det = ClockCapDetector::new(2200);
    for t in (0..120_000).step_by(100) {
        det.observe(&Sample { sm_mhz: 2200, ..s(t) }, 40);
    }
    assert_eq!(det.status(), CapStatus::Unknown);
}
