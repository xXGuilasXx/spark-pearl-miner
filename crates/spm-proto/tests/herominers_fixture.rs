//! Replays the redacted live capture of HeroMiners BR (2026-09-26): the handshake we send must be
//! byte-for-byte what was accepted, and every recorded notify must parse.
use primitive_types::U256;
use serde_json::Value;
use spm_proto::*;

const FIXTURE: &str = include_str!("../../../tests/fixtures/capture-herominers-br-authorize.jsonl");

#[test]
fn replay_authorize_and_jobs() {
    let mut sent: Option<Value> = None;
    let mut accepted = false;
    let mut jobs = 0;
    for line in FIXTURE.lines().filter(|l| !l.trim().is_empty()) {
        let ev: Value = serde_json::from_str(line).unwrap();
        match ev["ev"].as_str().unwrap() {
            "send" => sent = Some(ev["msg"].clone()),
            "recv" if ev["msg"]["id"] == 1 => {
                accepted = parse_reply(&ev["msg"].to_string(), 1).unwrap() == Reply::Accepted;
            }
            "job" => {
                let job = parse_notify(&ev["msg"].to_string()).unwrap().expect("notify");
                assert_eq!(job.header.len(), 76);
                assert_eq!(job.cert_version, Some(3));
                assert!(!job.requires_update());
                assert_eq!(job.diff, Some(2_097_152));
                // The pool's target equals the pdiff convention for its difficulty.
                assert_eq!(job.target, Job::target_for_diff(2_097_152));
                assert_eq!(job.target, U256::from(0x7fff8u64) << 184);
                assert!(job.job_id.ends_with("_2097152"));
                jobs += 1;
            }
            _ => {}
        }
    }
    assert!(accepted, "fixture must contain an accepted authorize");
    assert!(jobs >= 1, "fixture must contain at least one job");
    // Our generated handshake matches the accepted one (modulo the wallet placeholder and agent string).
    let ours = &authorize_msg(Dialect::Object, 1, "<WALLET>", "spm-probe", "x")[0];
    let recorded = sent.expect("a send event");
    assert_eq!(ours["method"], recorded["method"]);
    assert_eq!(ours["params"]["wallet"], recorded["params"]["wallet"]);
    assert_eq!(ours["params"]["worker"], recorded["params"]["worker"]);
    assert!(recorded["params"]["agent"].is_string());
}

#[test]
fn kryptex_handshake_shape() {
    let m = authorize_msg(Dialect::Kryptex, 1, "prl1abc", "rig", "x");
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["method"], "mining.subscribe");
    assert_eq!(m[1]["params"][0], "prl1abc.rig");
    let s = submit_msg(Dialect::Kryptex, 7, "prl1abc", "rig", "deadbeef_2097152", "plain_proof", "AAAA");
    assert_eq!(s["params"]["worker"], "prl1abc.rig");
    assert_eq!(s["params"]["plain_proof"], "AAAA");
}
