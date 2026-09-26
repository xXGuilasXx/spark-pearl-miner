//! Shared helpers: a daemon on temporary paths with the simulated worker, a fake GPU worker on
//! `worker.sock`, memory-guard fixtures and a tiny HTTP client.
#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use spm::coexist::MemorySource;
use spm::daemon::{DaemonHandle, DaemonOptions};
use spm::paths::Paths;
use spm::power::TelemetryChoice;
use spm_api::config::{Config, DialectSetting, PoolEntry, TlsSetting};
use spm_coexist::handshake::{write_ack_file, Signal, WorkerHandshake, ACK_FILE};
use spm_ipc::{read_frame, write_frame, ToDaemon, ToWorker};
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

/// Start a daemon on `root` with `cfg` written to its config file. The API listens on a free port;
/// no GPU telemetry (governor off) and a memory fixture with plenty of headroom.
pub async fn start_daemon(root: &Path, cfg: &Config) -> DaemonHandle {
    start_daemon_with(root, cfg, |_| {}).await
}

/// Same as [`start_daemon`], with a hook to adjust the options.
pub async fn start_daemon_with(root: &Path, cfg: &Config, adjust: impl FnOnce(&mut DaemonOptions)) -> DaemonHandle {
    let paths = Paths::under(root);
    paths.ensure().unwrap();
    std::fs::write(paths.config_file(), cfg.to_toml()).unwrap();
    let mut opts = DaemonOptions::new(paths);
    opts.worker_exe = PathBuf::from(env!("CARGO_BIN_EXE_spark-pearl-miner"));
    opts.api_port_override = Some(0);
    opts.telemetry = false;
    opts.governor = TelemetryChoice::Off;
    opts.memory = Some(mem_fixture(root, 100, 0.0));
    adjust(&mut opts);
    spm::daemon::start(opts).await.expect("daemon starts")
}

/// `/proc/meminfo` and `/proc/pressure/memory` stand-ins under `root` (rewritten on every call).
pub fn mem_fixture(root: &Path, available_gib: u64, psi_some_avg10: f64) -> MemorySource {
    let meminfo = root.join("meminfo");
    let psi = root.join("pressure-memory");
    let kb = available_gib * 1024 * 1024;
    std::fs::write(&meminfo, format!("MemTotal:       127535412 kB\nMemFree:        {kb} kB\nMemAvailable:   {kb} kB\n")).unwrap();
    std::fs::write(
        &psi,
        format!("some avg10={psi_some_avg10:.2} avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=1\n"),
    )
    .unwrap();
    MemorySource { meminfo, psi }
}

/// A fake GPU worker attached to `worker.sock` (launch = external): Hello → Ready, a heartbeat
/// every 200 ms, the pause/resume ACKs in `worker.ack` like the real worker (unless `ack` is
/// false), and every frame it receives recorded with its arrival time. It exits on
/// Release/Shutdown.
pub struct FakeWorker {
    frames: Arc<Mutex<Vec<(Instant, ToWorker)>>>,
    closed: Arc<AtomicBool>,
    pub cursor: usize,
}

impl FakeWorker {
    pub fn attach(sock: &Path, ack: bool) -> FakeWorker {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut s = loop {
            match UnixStream::connect(sock) {
                Ok(s) => break s,
                Err(e) if Instant::now() >= deadline => panic!("attach: {e}"),
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        };
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        assert!(matches!(read_frame::<_, ToWorker>(&mut s).unwrap(), ToWorker::Hello { .. }));
        write_frame(&mut s, &ToDaemon::Ready { kat_ok: true, device: "fake".into() }).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let ack_path = sock.with_file_name(ACK_FILE);
        let writer = Arc::new(Mutex::new(s.try_clone().unwrap()));
        {
            let (w, closed) = (writer.clone(), closed.clone());
            thread::spawn(move || {
                while !closed.load(Ordering::Relaxed) {
                    let beat = w.lock().map(|mut s| write_frame(&mut *s, &ToDaemon::Heartbeat { ts: 0 }).is_ok()).unwrap_or(false);
                    if !beat {
                        closed.store(true, Ordering::Relaxed);
                        return;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            });
        }
        {
            let (frames, closed) = (frames.clone(), closed.clone());
            thread::spawn(move || {
                let mut hs = WorkerHandshake::new();
                while let Ok(m) = read_frame::<_, ToWorker>(&mut s) {
                    let now = Instant::now();
                    let ackd = match &m {
                        ToWorker::Pause => {
                            let a = hs.on_signal(Signal::Usr1);
                            a.or_else(|| hs.on_quiescent())
                        }
                        ToWorker::Resume => hs.on_signal(Signal::Usr2),
                        _ => None,
                    };
                    if let (true, Some(a)) = (ack, ackd) {
                        write_ack_file(&ack_path, &a).unwrap();
                    }
                    let end = matches!(m, ToWorker::Release | ToWorker::Shutdown);
                    frames.lock().unwrap().push((now, m));
                    if end {
                        break;
                    }
                }
                closed.store(true, Ordering::Relaxed);
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
        }
        FakeWorker { frames, closed, cursor: 0 }
    }

    pub fn frames(&self) -> Vec<(Instant, ToWorker)> {
        self.frames.lock().unwrap().clone()
    }

    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// The next frame (after the cursor) matching `pred`, waiting up to `timeout`. Frames before
    /// it are skipped; the cursor moves past it.
    pub async fn next(&mut self, timeout: Duration, pred: impl Fn(&ToWorker) -> bool) -> Option<(Instant, ToWorker)> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let f = self.frames.lock().unwrap();
                if let Some(i) = (self.cursor..f.len()).find(|&i| pred(&f[i].1)) {
                    self.cursor = i + 1;
                    return Some(f[i].clone());
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Frames received after the cursor, without moving it.
    pub fn pending(&self) -> Vec<ToWorker> {
        self.frames.lock().unwrap()[self.cursor..].iter().map(|(_, m)| m.clone()).collect()
    }
}

/// A configuration for a fake external worker: `launch = external`, simulated shapes.
pub fn external_config(pools: Vec<PoolEntry>) -> Config {
    let mut c = test_config(pools);
    c.worker.launch = spm_api::config::LaunchMode::External;
    c
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
