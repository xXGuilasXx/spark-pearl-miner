//! Replays every redacted live capture in tests/fixtures: the handshake we generate must match
//! what each pool accepted, and every recorded `mining.notify` must parse into a `Job`.
use primitive_types::U256;
use serde_json::Value;
use spm_proto::*;

struct Capture { name: &'static str, dialect: Dialect, text: &'static str, expect_diff: Option<u64>, jsonrpc: bool }

const CAPTURES: &[Capture] = &[
    Capture { name: "herominers-br", dialect: Dialect::Object, expect_diff: Some(2_097_152), jsonrpc: false,
              text: include_str!("../../../tests/fixtures/capture-herominers-br-authorize.jsonl") },
    Capture { name: "luckypool-br", dialect: Dialect::Object, expect_diff: None, jsonrpc: true, // vardiff
              text: include_str!("../../../tests/fixtures/capture-luckypool-br-authorize.jsonl") },
    Capture { name: "kryptex-8048", dialect: Dialect::Kryptex, expect_diff: Some(2_097_152), jsonrpc: true,
              text: include_str!("../../../tests/fixtures/capture-kryptex-8048-authorize.jsonl") },
];

fn events(text: &str) -> Vec<Value> {
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn every_capture_replays() {
    for c in CAPTURES {
        let ev = events(c.text);
        let sent: Vec<&Value> = ev.iter().filter(|e| e["ev"] == "send").map(|e| &e["msg"]).collect();
        let ours = authorize_msg(c.dialect, 1, "<WALLET>", "spm-probe", "x");
        // Same method sequence and same login shape as the accepted handshake.
        let sent_methods: Vec<&str> = sent.iter().map(|m| m["method"].as_str().unwrap()).collect();
        let our_methods: Vec<&str> = ours.iter().map(|m| m["method"].as_str().unwrap()).collect();
        assert_eq!(sent_methods, our_methods, "{}: method sequence", c.name);
        let last_sent = sent.last().unwrap();
        let last_ours = ours.last().unwrap();
        match c.dialect {
            Dialect::Object => {
                assert_eq!(last_ours["params"]["wallet"], last_sent["params"]["wallet"], "{}: wallet", c.name);
                assert_eq!(last_ours["params"]["worker"], last_sent["params"]["worker"], "{}: worker", c.name);
            }
            Dialect::Kryptex | Dialect::KryptexV2 => assert_eq!(last_ours["params"][0], last_sent["params"][0], "{}: login", c.name),
        }
        // The authorize was accepted.
        let auth_id = last_sent["id"].as_u64().unwrap();
        let accepted = ev.iter().filter(|e| e["ev"] == "recv").any(|e| parse_reply(&e["msg"].to_string(), auth_id).unwrap() == Reply::Accepted);
        assert!(accepted, "{}: authorize accepted", c.name);
        // Every job parses and matches expectations.
        let mut jobs = 0;
        for e in ev.iter().filter(|e| e["ev"] == "job") {
            let job = parse_notify(&e["msg"].to_string()).unwrap().expect("notify");
            assert_eq!(job.cert_version.unwrap_or(3), 3, "{}: cert_version", c.name);
            assert!(!job.requires_update());
            let diff = job.diff.expect("diff from field or job_id");
            if let Some(d) = c.expect_diff { assert_eq!(diff, d, "{}: diff", c.name); }
            match c.dialect {
                // HeroMiners/LuckyPool: Bitcoin pdiff, target = floor(0xFFFF * 2^208 / diff).
                Dialect::Object => assert_eq!(job.target, Job::target_for_diff(diff), "{}: pdiff target", c.name),
                // Kryptex: target = 2^224 / diff - 1 (ratio 65535/65536 vs pdiff). Always use notify.target as sent.
                Dialect::Kryptex | Dialect::KryptexV2 => assert_eq!(job.target, (U256::one() << 224) / U256::from(diff) - U256::one(), "{}: 2^224/diff-1 target", c.name),
            }
            assert_eq!(job.header[0..4], 0x2000_0000u32.to_le_bytes(), "{}: header version", c.name);
            jobs += 1;
        }
        assert!(jobs >= 1, "{}: at least one job", c.name);
        eprintln!("{}: {} jobs replayed", c.name, jobs);
    }
}

/// Python's `json.dumps` default separators (", " and ": "), inserted outside strings. The probe
/// sent exactly `json.dumps(msg)`, so this turns our compact frame into the captured wire bytes.
fn python_separators(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + 32);
    let (mut in_str, mut esc) = (false, false);
    for c in compact.chars() {
        out.push(c);
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == ',' || c == ':' {
            out.push(' ');
        }
    }
    out
}

/// The raw `msg` text of a capture line (the probe logged it with the same `json.dumps`).
fn raw_msg(line: &str) -> &str {
    let key = "\"msg\": ";
    let start = line.find(key).expect("msg") + key.len();
    let end = line.rfind(", \"t\": ").expect("t");
    &line[start..end]
}

#[test]
fn handshake_frames_match_captures_byte_for_byte() {
    const PROBE_AGENT: &str = "spark-pearl-miner-probe/0.0.1";
    for c in CAPTURES {
        let sent: Vec<&str> = c.text.lines().filter(|l| l.contains("\"ev\": \"send\"")).map(raw_msg).collect();
        let opts = FrameOpts { jsonrpc: c.jsonrpc, agent: PROBE_AGENT };
        let ours: Vec<String> = authorize_lines(c.dialect, 1, "<WALLET>", "spm-probe", "x", &opts)
            .unwrap()
            .iter()
            .map(|l| python_separators(l))
            .collect();
        assert_eq!(ours, sent, "{}: handshake bytes", c.name);
        // The dialect default matches what the pool accepted, except LuckyPool (object + jsonrpc,
        // set by its preset).
        if c.name != "luckypool-br" {
            assert_eq!(c.dialect.default_jsonrpc(), c.jsonrpc, "{}: default jsonrpc", c.name);
        }
    }
}
