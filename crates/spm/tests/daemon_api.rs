//! The daemon behind the real HTTP listener: SSE stats, config changes through the API (saved
//! atomically with a .bak, audited, TLS mode kept), hot reload of hand edits, fee keys refused,
//! and the CLI control socket.

mod common;

use std::time::Duration;

use common::*;
use spm::control::{self, Request};
use spm::paths::Paths;
use spm_mockpool::{MockConfig, MockPool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_config_sse_and_control_socket() {
    let root = temp_root("api");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let cfg = test_config(vec![mock_slot("mock", pool.port())]);
    let d = start_daemon(&root, &cfg).await;
    let port = d.api_port.unwrap();
    let paths = Paths::under(&root);

    // Token file is private.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(paths.token_file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let (cookie, csrf) = login(port, &d.token).await;

    // SSE: a stats event within a couple of seconds.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {cookie}\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "SSE stream closed");
            seen.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&seen).contains("event: stats") {
                return;
            }
        }
    })
    .await;
    assert!(got.is_ok(), "no stats event: {}", String::from_utf8_lossy(&seen));
    assert!(String::from_utf8_lossy(&seen).contains("text/event-stream"));

    // PUT a change: TLS off on pool 1, a second pool; saved, backed up, audited.
    let mut new = cfg.clone();
    new.pools[0].tls = spm_api::config::TlsSetting::Off;
    new.pools.push(mock_slot("second", pool.port()));
    let body = serde_json::to_string(&new).unwrap();
    let (code, _, out) = http(port, "PUT", "/api/v1/config", &[("Cookie", &cookie), ("X-SPM-CSRF", &csrf)], Some(&body)).await;
    assert_eq!(code, 200, "{out}");
    let text = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(text.contains("tls = \"off\"") && text.contains("second"), "{text}");
    assert!(paths.config_file().with_extension("toml.bak").exists());
    let audit = std::fs::read_to_string(paths.audit_file()).unwrap();
    assert!(audit.contains("\"source\":\"api\""), "{audit}");
    let (_, _, got) = http(port, "GET", "/api/v1/config", &[("Cookie", &cookie)], None).await;
    let v: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(v["pools"][0]["tls"], "off");
    assert_eq!(v["pools"].as_array().unwrap().len(), 2);

    // Fee keys and a 4th pool are refused and change nothing.
    let mut bad: serde_json::Value = serde_json::from_str(&body).unwrap();
    bad["miner"]["dev_wallet"] = serde_json::json!("prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n");
    let (code, _, out) = http(port, "PUT", "/api/v1/config", &[("Cookie", &cookie), ("X-SPM-CSRF", &csrf)], Some(&bad.to_string())).await;
    assert_eq!(code, 422, "{out}");
    let mut four = new.clone();
    four.pools.push(mock_slot("3", 1));
    four.pools.push(mock_slot("4", 2));
    let (code, _, _) = http(port, "PUT", "/api/v1/config", &[("Cookie", &cookie), ("X-SPM-CSRF", &csrf)], Some(&serde_json::to_string(&four).unwrap())).await;
    assert_eq!(code, 422);
    assert_eq!(std::fs::read_to_string(paths.config_file()).unwrap(), text, "refused changes leave the file alone");

    // A hand edit of config.toml is hot-reloaded and audited as a file change.
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(paths.config_file(), text.replace("worker = \"rig-test\"", "worker = \"rig-edited\"")).unwrap();
    let s = d.wait_for(Duration::from_secs(8), |s| s.config.miner.worker == "rig-edited").await;
    assert!(s.is_some(), "hot reload");
    let audit = std::fs::read_to_string(paths.audit_file()).unwrap();
    assert!(audit.contains("\"source\":\"file\""), "{audit}");

    // An invalid hand edit keeps the running configuration and raises an alert.
    std::fs::write(paths.config_file(), "schema_version = 1\nfee = 0\n").unwrap();
    let s = d.wait_for(Duration::from_secs(8), |s| s.status.alerts.iter().any(|a| a.msg.contains("invalid"))).await;
    assert!(s.is_some(), "invalid edit reported");
    assert_eq!(d.snapshot().config.miner.worker, "rig-edited");

    // The CLI control socket (same user only) answers status and controls.
    let v = control::request(&paths.control_sock(), &Request::Status).await.unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"]["state"], "stopped");
    let v = control::request(&paths.control_sock(), &Request::Control { op: spm_api::ControlOp::Pause }).await.unwrap();
    assert_eq!(v["ok"], false, "pause while stopped is refused: {v}");
    let mode = std::fs::metadata(paths.control_sock()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    d.shutdown().await;
    assert!(!paths.control_sock().exists());
    let state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(paths.state_file()).unwrap()).unwrap();
    assert_eq!(state["running"], false, "clean shutdown recorded");
    assert!(state["fee"].is_object());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mining_does_not_start_before_setup() {
    let root = temp_root("setup");
    let mut cfg = test_config(vec![mock_slot("mock", 9)]);
    cfg.miner.disclosure_accepted = false;
    let d = start_daemon(&root, &cfg).await;
    let s = d.snapshot();
    assert!(s.status.setup_required);
    assert_eq!(s.status.state, "setup_required");
    let e = d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap_err();
    assert!(e.contains("disclosure"), "{e}");
    // A second daemon for the same user refuses to start instead of taking the sockets over.
    let mut opts = spm::daemon::DaemonOptions::new(Paths::under(&root));
    opts.api_port_override = Some(0);
    opts.telemetry = false;
    let e = spm::daemon::start(opts).await.err().expect("second daemon refused");
    assert!(e.to_string().contains("already running"), "{e}");
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
