//! The numbers the docs quote from `docs/_data/facts.toml` (`[coexist]`) must match the code.

use std::collections::HashMap;
use std::path::Path;

use spm_coexist::handshake::{PAUSE_ACK_DEADLINE, TERM_GRACE};
use spm_coexist::memguard::*;
use spm_coexist::*;

fn section(name: &str) -> HashMap<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/_data/facts.toml");
    let text = std::fs::read_to_string(path).expect("facts.toml");
    let mut current = String::new();
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or_default().trim();
        if let Some(s) = line.strip_prefix('[') {
            current = s.trim_end_matches(']').to_string();
        } else if current == name {
            if let Some((k, v)) = line.split_once('=') {
                out.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
        }
    }
    out
}

#[test]
fn coexist_facts_match_the_code() {
    let f = section("coexist");
    let get = |k: &str| f.get(k).unwrap_or_else(|| panic!("facts.toml [coexist] lacks {k}")).clone();
    let num = |k: &str| get(k).parse::<u64>().unwrap_or_else(|_| panic!("{k} is not an integer"));
    assert_eq!(get("metrics_url"), DEFAULT_METRICS_URL);
    assert_eq!(num("poll_ms_default"), DEFAULT_POLL.as_millis() as u64);
    assert_eq!(num("poll_ms_min"), POLL_MIN.as_millis() as u64);
    assert_eq!(num("poll_ms_max"), POLL_MAX.as_millis() as u64);
    assert_eq!(num("idle_s"), DEFAULT_IDLE.as_secs());
    assert_eq!(num("busy_sm_pct"), u64::from(DEFAULT_BUSY_SM_PCT));
    assert_eq!(num("pause_ack_deadline_ms"), PAUSE_ACK_DEADLINE.as_millis() as u64);
    assert_eq!(num("term_grace_s"), TERM_GRACE.as_secs());
    assert_eq!(num("worker_budget_gib") * GIB, WORKER_BUDGET_BYTES);
    assert_eq!(num("start_headroom_gib") * GIB, START_HEADROOM_BYTES);
    assert_eq!(num("exit_below_gib") * GIB, EXIT_BELOW_AVAILABLE_BYTES);
    assert_eq!(num("exit_psi_some_avg10_pct") as f64, EXIT_ABOVE_PSI_SOME_AVG10);
}
