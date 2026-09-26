//! Coexistence wired into the daemon: a fake vLLM metrics server drives `yield` (pause within
//! 300 ms, resume after `idle_s`) and `yield-release` (release, then a new worker); the memory
//! guard refuses a start on a fixture and releases under pressure; `spark-modo` never spawns;
//! an unacknowledged pause is escalated to a release. No GPU is touched.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use spm::paths::Paths;
use spm_api::config::{CoexistenceMode, Config, LaunchMode};
use spm_ipc::ToWorker;
use spm_mockpool::{MockConfig, MockPool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A vLLM `/metrics` stand-in: `running` requests in flight; `down` refuses to answer.
struct FakeVllm {
    port: u16,
    running: Arc<AtomicU32>,
    down: Arc<AtomicBool>,
}

impl FakeVllm {
    async fn start() -> FakeVllm {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let running = Arc::new(AtomicU32::new(0));
        let down = Arc::new(AtomicBool::new(false));
        let (r, dn) = (running.clone(), down.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let (r, dn) = (r.clone(), dn.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut got = Vec::new();
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    if dn.load(Ordering::Relaxed) {
                        let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    let body = format!(
                        "# HELP vllm:num_requests_running Number of requests in model execution batches.\n\
                         # TYPE vllm:num_requests_running gauge\n\
                         vllm:num_requests_running{{engine=\"0\",model_name=\"test\"}} {}.0\n\
                         # TYPE vllm:num_requests_waiting gauge\n\
                         vllm:num_requests_waiting{{engine=\"0\",model_name=\"test\"}} 0.0\n",
                        r.load(Ordering::Relaxed)
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        FakeVllm { port, running, down }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/metrics", self.port)
    }

    fn busy(&self, n: u32) {
        self.running.store(n, Ordering::Relaxed);
    }
}

fn yield_config(mut c: Config, mode: CoexistenceMode, vllm: &FakeVllm) -> Config {
    c.coexistence.mode = mode;
    c.coexistence.metrics_url = vllm.url();
    c.coexistence.poll_ms = 100;
    c.coexistence.idle_s = 1;
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn yield_pauses_within_300_ms_and_resumes_after_idle() {
    let root = temp_root("yield");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let vllm = FakeVllm::start().await;
    let cfg = yield_config(external_config(vec![mock_slot("mock", pool.port())]), CoexistenceMode::Yield, &vllm);
    let d = start_daemon(&root, &cfg).await;
    let started = Instant::now();
    let msg = d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    assert!(msg.contains("waiting for 1 s of LLM-server idle"), "{msg}");
    let sock = Paths::under(&root).worker_sock();
    let mut w = tokio::task::spawn_blocking(move || FakeWorker::attach(&sock, true)).await.unwrap();

    // Nothing runs before 1 s of continuous idle.
    let (t_resume, _) = w.next(Duration::from_secs(5), |m| *m == ToWorker::Resume).await.expect("resumed after idle");
    assert!(t_resume.duration_since(started) >= Duration::from_millis(900), "{:?}", t_resume.duration_since(started));
    assert!(d.wait_for(Duration::from_secs(3), |s| s.status.state == "mining" && s.status.coexist.gate == "mine").await.is_some());

    // A request arrives: paused (context kept, no release) within 300 ms.
    let acks = d.snapshot().status.coexist.handshake.acks;
    let t_busy = Instant::now();
    vllm.busy(1);
    let (t_pause, _) = w.next(Duration::from_secs(2), |m| *m == ToWorker::Pause).await.expect("paused");
    let lag = t_pause.duration_since(t_busy);
    assert!(lag < Duration::from_millis(300), "pause took {lag:?}");
    let s = d
        .wait_for(Duration::from_secs(2), |s| {
            let h = &s.status.coexist.handshake;
            s.status.pause_reason.as_deref() == Some("yield")
                && h.acks > acks
                && h.last_ack.as_deref().is_some_and(|a| a.starts_with("paused"))
        })
        .await
        .expect("paused by the gate, ACK read");
    let c = &s.status.coexist;
    assert_eq!((c.gate.as_str(), c.signal.as_str(), c.llm_running), ("pause", "vllm", Some(1.0)));
    assert!(c.handshake.pause_latency_ms.unwrap() < 100, "{:?}", c.handshake);
    assert!(c.handshake.last_ack.as_deref().is_some_and(|a| a.starts_with("paused")), "{:?}", c.handshake);
    assert!(!w.pending().contains(&ToWorker::Release));

    // Idle again: resumed after idle_s, not before.
    let t_idle = Instant::now();
    vllm.busy(0);
    let (t_resume, _) = w.next(Duration::from_secs(4), |m| *m == ToWorker::Resume).await.expect("resumed");
    let waited = t_resume.duration_since(t_idle);
    assert!(waited >= Duration::from_millis(900) && waited < Duration::from_millis(2500), "{waited:?}");

    // Metrics gone and no per-process fallback (no telemetry here): the GPU counts as busy.
    vllm.down.store(true, Ordering::Relaxed);
    w.next(Duration::from_secs(2), |m| *m == ToWorker::Pause).await.expect("paused without a signal");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.coexist.signal == "unavailable").await.expect("unavailable");
    assert!(s.status.coexist.metrics_error.as_deref().unwrap_or_default().contains("503"), "{:?}", s.status.coexist);
    assert!(s.status.coexist.transitions >= 4);
    assert!(!w.closed());
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn yield_release_releases_the_worker_then_spawns_a_new_one() {
    let root = temp_root("yrel");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let vllm = FakeVllm::start().await;
    let cfg = yield_config(test_config(vec![mock_slot("mock", pool.port())]), CoexistenceMode::YieldRelease, &vllm);
    let d = start_daemon(&root, &cfg).await;
    d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    let s = d
        .wait_for(Duration::from_secs(15), |s| s.status.worker.state == "hashing" && s.status.worker.pid.is_some())
        .await
        .expect("first worker hashing");
    let first = s.status.worker.pid.unwrap();

    vllm.busy(2);
    let s = d
        .wait_for(Duration::from_secs(8), |s| s.status.coexist.gate == "release" && s.status.worker.pid.is_none())
        .await
        .expect("worker released");
    assert_eq!(s.status.pause_reason.as_deref(), Some("yield"));
    assert!(matches!(s.status.worker.state.as_str(), "absent" | "backoff"), "{}", s.status.worker.state);
    assert_eq!(s.status.worker.failures_10min, 0, "a release is not a failure");
    // Stays released while busy.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(d.snapshot().status.worker.pid.is_none());

    vllm.busy(0);
    let s = d
        .wait_for(Duration::from_secs(15), |s| s.status.worker.state == "hashing" && s.status.worker.pid.is_some_and(|p| p != first))
        .await
        .expect("a new worker after idle");
    assert_eq!(s.status.coexist.gate, "mine");
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_guard_refuses_to_start_below_the_headroom_and_releases_under_pressure() {
    let root = temp_root("memg");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let cfg = test_config(vec![mock_slot("mock", pool.port())]);
    // 21 GiB available − 2 GiB budget < 20 GiB of headroom.
    let d = start_daemon_with(&root, &cfg, |o| o.memory = Some(mem_fixture(&root, 21, 0.0))).await;
    let msg = d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    assert!(msg.contains("memory guard"), "{msg}");
    let s = d.wait_for(Duration::from_secs(3), |s| s.status.coexist.memory.state == "refused").await.expect("refused");
    assert_eq!(s.status.pause_reason.as_deref(), Some("memory"));
    assert!(s.status.alerts.iter().any(|a| a.msg.contains("not starting")), "{:?}", s.status.alerts);
    assert_eq!(s.status.coexist.memory.available_gib, Some(21.0));
    // Pools connect and jobs arrive, but no worker is spawned.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = d.snapshot();
    assert_eq!((s.status.worker.pid, s.status.worker.state.as_str()), (None, "absent"));

    // Enough memory again: the worker starts.
    mem_fixture(&root, 64, 0.0);
    let s = d
        .wait_for(Duration::from_secs(15), |s| s.status.worker.state == "hashing" && s.status.coexist.memory.state == "ok")
        .await
        .expect("started once the headroom is back");
    assert!(s.status.worker.pid.is_some());

    // Memory pressure: released, and not restarted while it lasts.
    mem_fixture(&root, 64, 25.0);
    let s = d
        .wait_for(Duration::from_secs(8), |s| s.status.coexist.memory.state == "exit_pressure" && s.status.worker.pid.is_none())
        .await
        .expect("released under pressure");
    assert_eq!(s.status.coexist.memory.psi_some_avg10, Some(25.0));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(d.snapshot().status.worker.pid.is_none());
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spark_modo_mode_never_spawns_the_worker() {
    let root = temp_root("smodo");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let mut cfg = test_config(vec![mock_slot("mock", pool.port())]);
    cfg.coexistence.mode = CoexistenceMode::SparkModo;
    assert_eq!(cfg.worker.launch, LaunchMode::Spawn, "spawn in the file; spark-modo wins");
    let d = start_daemon(&root, &cfg).await;
    let msg = d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    assert!(msg.contains("controlled by spark-modo"), "{msg}");
    let s = d
        .wait_for(Duration::from_secs(5), |s| s.status.worker.state == "waiting_external")
        .await
        .expect("waits for the miner runtime");
    assert_eq!(s.status.worker.launch, "external");
    assert_eq!((s.status.coexist.gate.as_str(), s.status.coexist.controlled_by.as_deref()), ("external", Some("spark-modo")));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(d.snapshot().status.worker.pid, None, "never spawned");
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unacknowledged_pause_is_escalated_to_a_release() {
    let root = temp_root("noack");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let cfg = external_config(vec![mock_slot("mock", pool.port())]);
    let d = start_daemon(&root, &cfg).await;
    d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    let sock = Paths::under(&root).worker_sock();
    let mut w = tokio::task::spawn_blocking(move || FakeWorker::attach(&sock, false)).await.unwrap();
    w.next(Duration::from_secs(10), |m| *m == ToWorker::Resume).await.expect("Resume");
    d.bridge.control_op(spm_api::ControlOp::Pause).await.unwrap();
    let (t_pause, _) = w.next(Duration::from_secs(2), |m| *m == ToWorker::Pause).await.expect("Pause");
    let (t_rel, _) = w.next(Duration::from_secs(2), |m| *m == ToWorker::Release).await.expect("escalated to Release");
    let gap = t_rel.duration_since(t_pause);
    assert!(gap >= Duration::from_millis(90) && gap < Duration::from_millis(500), "{gap:?}");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.coexist.handshake.escalations == 1).await.expect("counted");
    assert_eq!(s.status.worker.failures_10min, 0, "the worker exited as asked: not a failure");
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
