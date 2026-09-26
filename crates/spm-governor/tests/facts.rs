//! The numbers the docs quote from `docs/_data/facts.toml` (`[power]`) must match the code.

use std::collections::HashMap;
use std::path::Path;

use spm_governor::fault::*;
use spm_governor::*;

fn section(name: &str) -> HashMap<String, f64> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/_data/facts.toml");
    let text = std::fs::read_to_string(path).expect("facts.toml");
    let mut current = String::new();
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some(s) = line.strip_prefix('[') {
            current = s.trim_end_matches(']').to_string();
        } else if current == name {
            if let Some((k, v)) = line.split_once('=') {
                if let Ok(v) = v.trim().parse::<f64>() {
                    out.insert(k.trim().to_string(), v);
                }
            }
        }
    }
    out
}

#[test]
fn power_facts_match_the_code() {
    let f = section("power");
    let get = |k: &str| *f.get(k).unwrap_or_else(|| panic!("facts.toml [power] lacks {k}"));
    for (p, prefix) in [(Profile::Eco, "eco"), (Profile::Balanced, "balanced"), (Profile::Max, "max")] {
        assert_eq!(get(&format!("{prefix}_target_w")), p.target_w(), "{p}");
        assert_eq!(get(&format!("{prefix}_stop_w")), p.hard_stop_w(), "{p}");
    }
    assert_eq!(get("eco_clock_cap_mhz"), f64::from(Profile::Eco.recommended_clock_cap_mhz()));
    assert_eq!(get("clock_cap_mhz"), f64::from(DEFAULT_CLOCK_CAP_MHZ));
    assert_eq!(get("clock_cap_mhz"), f64::from(Profile::Balanced.recommended_clock_cap_mhz()));
    assert_eq!(get("sample_hz"), 1.0 / SAMPLE_PERIOD.as_secs_f64());
    assert_eq!(get("duty_min_pct"), f64::from(DUTY_MIN_PCT));
    assert_eq!(get("ramp_pct_per_s"), DUTY_RAMP_PCT_PER_S);
    assert_eq!(get("over_power_samples"), f64::from(OVER_POWER_SAMPLES));
    assert_eq!(get("pause_s"), OVER_POWER_PAUSE.as_secs_f64());
    assert_eq!(get("pause_s"), OVER_TEMP_PAUSE.as_secs_f64());
    assert_eq!(get("gpu_temp_trip_c"), GPU_TEMP_TRIP_C);
    assert_eq!(get("acpitz_trip_c"), ACPITZ_TRIP_C);
    assert_eq!(get("temp_resume_margin_c"), TEMP_RESUME_MARGIN_C);
    assert_eq!(get("derate_start_c"), GPU_TEMP_DERATE_START_C);
    assert_eq!(get("derate_w_per_c"), DERATE_W_PER_C);
    assert_eq!(get("usb_pd_max_mhz"), f64::from(USB_PD_MAX_SM_MHZ));
    assert_eq!(get("usb_pd_hold_s"), USB_PD_HOLD.as_secs_f64());
    assert_eq!(get("safety_mode_w"), SAFETY_MODE_POWER_W);
    assert_eq!(get("safety_mode_hold_s"), SAFETY_MODE_HOLD.as_secs_f64());
    assert_eq!(get("thermal_cap_w"), THERMAL_CAP_POWER_W);
    assert_eq!(get("thermal_cap_hold_s"), THERMAL_CAP_HOLD.as_secs_f64());
}
