//! The P0 demo, end to end on localhost (CI-safe: mock pools only, simulated CPU worker, no GPU):
//! pool 1 refuses connections → pool 2 is ACTIVE within 15 s of real time and accepts real,
//! verified shares; pool 1 comes back → mining returns to pool 1 after the (shortened) probe
//! cadence. The GUI's view (`GET /api/v1/pools`, SSE) shows the same thing.

mod common;

use std::time::{Duration, Instant};

use common::*;
use spm_api::ControlOp;
use spm_mockpool::{Faults, MockConfig, MockPool};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool1_down_fails_over_to_pool2_and_returns() {
    let root = temp_root("failover");
    let mut refusing = MockConfig::trivial();
    refusing.faults = Faults { refuse_connections: true, ..Faults::default() };
    let pool1 = MockPool::start(refusing).await.unwrap();
    let pool2 = MockPool::start(MockConfig::trivial()).await.unwrap();
    let cfg = test_config(vec![mock_slot("mock 1", pool1.port()), mock_slot("mock 2", pool2.port())]);
    let d = start_daemon(&root, &cfg).await;
    let port = d.api_port.unwrap();

    // Before Start nothing connects.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pool2.stats().connections, 0);
    assert_eq!(d.snapshot().status.state, "stopped");

    let t0 = Instant::now();
    d.bridge.control_op(ControlOp::Start).await.unwrap();
    let s = d
        .wait_for(Duration::from_secs(15), |s| s.status.active_pool == Some(2))
        .await
        .expect("pool 2 becomes ACTIVE within 15 s");
    let failover = t0.elapsed();
    assert!(failover < Duration::from_secs(15), "{failover:?}");
    assert_eq!(s.pools.slots[1].state, "active");
    assert_eq!(s.pools.slots[0].last_error.as_ref().map(|e| e.code.as_str()), Some("connect_refused"));
    eprintln!("failover to pool 2 after {failover:?}");

    // The simulated worker mines real shares that the mock verifies with zk-pow and accepts.
    let st = pool2.wait_stats(|s| s.accepted >= 1, Duration::from_secs(30)).await.expect("a share accepted on pool 2");
    assert_eq!(st.rejected, 0);
    let s = d.wait_for(Duration::from_secs(10), |s| s.status.shares.accepted >= 1).await.expect("share counted");
    assert_eq!(s.status.mining_target, "user");
    assert!(s.status.worker.simulated);

    // The GUI sees the same: pools view over HTTP with a session, and SSE stats.
    let (cookie, _csrf) = login(port, &d.token).await;
    let (code, _, body) = http(port, "GET", "/api/v1/pools", &[("Cookie", &cookie)], None).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["slots"][1]["state"], "active");
    assert!(v["timeline"].as_array().unwrap().iter().any(|e| e["msg"].as_str().unwrap_or("").contains("failing over from pool 1 to pool 2")));

    // Pool 1 is restored: after the backoff and the probe cadence (2 s + 1 s stable here; 300 s +
    // 60 s by default) mining returns to it.
    pool1.set_faults(Faults::default());
    let t1 = Instant::now();
    d.wait_for(Duration::from_secs(25), |s| s.status.active_pool == Some(1)).await.expect("mining returns to pool 1");
    eprintln!("failback to pool 1 after {:?}", t1.elapsed());
    pool1.wait_stats(|s| s.accepted >= 1, Duration::from_secs(30)).await.expect("shares on pool 1 after failback");
    let s = d.snapshot();
    assert!(s.pools.timeline.iter().any(|e| e.msg.contains("switching from pool 2 to pool 1")), "{:#?}", s.pools.timeline);

    // Stop releases the worker process (frees the GPU context in the real worker).
    d.bridge.control_op(ControlOp::Stop).await.unwrap();
    d.wait_for(Duration::from_secs(10), |s| s.status.worker.state == "absent" && s.status.state == "stopped")
        .await
        .expect("worker released after Stop");
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
