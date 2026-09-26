//! Shared helpers: a daemon on temporary paths with the simulated worker, and a tiny HTTP client.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use spm::daemon::{DaemonHandle, DaemonOptions};
use spm::paths::Paths;
use spm_api::config::{Config, DialectSetting, PoolEntry, TlsSetting};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const WALLET: &str = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh";

pub fn temp_root(name: &str) -> PathBuf {
    // Short paths: Unix socket paths are limited to 108 bytes.
    let d = PathBuf::from(format!("/tmp/spm-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A mock pool slot: plain TCP on 127.0.0.1, object dialect.
pub fn mock_slot(name: &str, port: u16) -> PoolEntry {
    let mut p = PoolEntry::new(name, "127.0.0.1", port, TlsSetting::Off);
    p.dialect = DialectSetting::Object;
    p
}

/// A complete configuration for tests: simulated worker, short failback timers.
pub fn test_config(pools: Vec<PoolEntry>) -> Config {
    let mut c = Config::default();
    c.miner.wallet = WALLET.to_string();
    c.miner.worker = "rig-test".to_string();
    c.miner.disclosure_accepted = true;
    c.pools = pools;
    c.worker.simulate = true;
    c.worker.sim_interval_ms = 200;
    c.failover.connect_timeout_s = 3;
    c.failover.handshake_timeout_s = 3;
    c.failover.first_job_timeout_s = 5;
    c.failover.backoff_s = vec![1];
    c.failover.failback_probe_every_s = 2;
    c.failover.failback_stable_s = 1;
    c
}

/// Start a daemon on `root` with `cfg` written to its config file. The API listens on a free port.
pub async fn start_daemon(root: &std::path::Path, cfg: &Config) -> DaemonHandle {
    let paths = Paths::under(root);
    paths.ensure().unwrap();
    std::fs::write(paths.config_file(), cfg.to_toml()).unwrap();
    let mut opts = DaemonOptions::new(paths);
    opts.worker_exe = PathBuf::from(env!("CARGO_BIN_EXE_spark-pearl-miner"));
    opts.api_port_override = Some(0);
    opts.telemetry = false;
    spm::daemon::start(opts).await.expect("daemon starts")
}

/// One HTTP/1.1 request; returns (status, raw headers, body).
pub async fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut buf)).await.expect("response in time").unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// Log in with the token: returns (cookie header value, csrf).
pub async fn login(port: u16, token: &str) -> (String, String) {
    let (st, head, body) = http(port, "POST", "/api/v1/session", &[], Some(&format!("{{\"token\":\"{token}\"}}"))).await;
    assert_eq!(st, 200, "{head}\n{body}");
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: ").or_else(|| l.strip_prefix("Set-Cookie: ")))
        .and_then(|c| c.split(';').next())
        .expect("cookie")
        .to_string();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    (cookie, v["csrf"].as_str().unwrap().to_string())
}
