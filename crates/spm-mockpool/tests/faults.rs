//! Every mock-pool fault knob, observed through `PoolSession` events (what the failover reducer
//! will consume). No mining here: proofs are junk bytes unless the fault is about verification.
use std::time::Duration;

use spm_mockpool::{Faults, MockConfig, MockPool};
use spm_proto::client::{DisconnectReason, PoolSession, SessionConfig, SessionEvent, SubmitError};
use spm_proto::tls::{Connector, TlsMode};
use spm_proto::{Dialect, Job, ProofField, RejectKind};
use tokio::sync::mpsc::Receiver;

const WAIT: Duration = Duration::from_secs(10);

async fn pool_with(faults: Faults) -> MockPool {
    let mut cfg = MockConfig::trivial();
    cfg.faults = faults;
    MockPool::start(cfg).await.unwrap()
}

fn session_cfg(pool: &MockPool, dialect: Dialect) -> SessionConfig {
    let mut cfg = SessionConfig::new("127.0.0.1", pool.port(), dialect, "prl1qtestwallet", "rig0");
    cfg.tls = TlsMode::Off;
    cfg
}

fn start(pool: &MockPool) -> (PoolSession, Receiver<SessionEvent>) {
    PoolSession::spawn(session_cfg(pool, Dialect::Object), Connector::default())
}

async fn next(ev: &mut Receiver<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(WAIT, ev.recv()).await.expect("event in time").expect("channel open")
}

/// Skip events until `pick` returns Some.
async fn until<T>(ev: &mut Receiver<SessionEvent>, mut pick: impl FnMut(&SessionEvent) -> Option<T>) -> T {
    loop {
        let e = next(ev).await;
        if let Some(t) = pick(&e) {
            return t;
        }
    }
}

async fn job(ev: &mut Receiver<SessionEvent>) -> Job {
    until(ev, |e| match e {
        SessionEvent::JobReceived(j) => Some(j.clone()),
        SessionEvent::Disconnected { reason } => panic!("disconnected: {reason:?}"),
        _ => None,
    })
    .await
}

async fn disconnected(ev: &mut Receiver<SessionEvent>) -> DisconnectReason {
    until(ev, |e| match e {
        SessionEvent::Disconnected { reason } => Some(reason.clone()),
        _ => None,
    })
    .await
}

/// Nothing but `allowed` events for `d`.
async fn quiet_for(ev: &mut Receiver<SessionEvent>, d: Duration, allowed: impl Fn(&SessionEvent) -> bool) {
    let end = tokio::time::Instant::now() + d;
    while let Ok(e) = tokio::time::timeout_at(end, ev.recv()).await {
        let e = e.expect("channel open");
        assert!(allowed(&e), "unexpected event {e:?}");
    }
}

#[tokio::test]
async fn refuse_connections_then_recover() {
    let pool = pool_with(Faults { refuse_connections: true, ..Faults::default() }).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_s, mut ev) = start(&pool);
    assert!(matches!(next(&mut ev).await, SessionEvent::Disconnected { reason: DisconnectReason::ConnectFailed(_) }));
    pool.set_faults(Faults::default());
    // The listener comes back on the same port.
    let mut ok = false;
    for _ in 0..50 {
        let (_s, mut ev) = start(&pool);
        if let SessionEvent::Connected { .. } = next(&mut ev).await {
            job(&mut ev).await;
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "pool accepts again after the fault is cleared");
}

#[tokio::test]
async fn blackhole_connects_but_never_authorizes() {
    let pool = pool_with(Faults { blackhole: true, ..Faults::default() }).await;
    let (s, mut ev) = start(&pool);
    assert!(matches!(next(&mut ev).await, SessionEvent::Connected { .. }));
    quiet_for(&mut ev, Duration::from_millis(600), |_| false).await;
    assert_eq!(pool.stats().authorizes, 0);
    assert_eq!(s.submit("anything", vec![1]).await, Err(SubmitError::Stale { job_id: "anything".into(), current: None }));
    s.shutdown().await;
}

#[tokio::test]
async fn auth_reject_is_reported_and_ends_the_session() {
    let pool = pool_with(Faults { auth_reject: true, ..Faults::default() }).await;
    let (_s, mut ev) = start(&pool);
    let reason = until(&mut ev, |e| match e {
        SessionEvent::AuthRejected { reason } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert!(reason.contains("Unauthorized"));
    assert!(matches!(disconnected(&mut ev).await, DisconnectReason::AuthRejected(_)));
}

#[tokio::test]
async fn no_job_authorizes_but_stays_silent() {
    let pool = pool_with(Faults { no_job: true, ..Faults::default() }).await;
    let (s, mut ev) = start(&pool);
    until(&mut ev, |e| matches!(e, SessionEvent::Authorized { .. }).then_some(())).await;
    quiet_for(&mut ev, Duration::from_millis(500), |_| false).await;
    pool.new_job();
    quiet_for(&mut ev, Duration::from_millis(200), |_| false).await;
    assert_eq!(s.current_job(), None);
    assert_eq!(pool.stats().jobs_sent, 0);
}

#[tokio::test]
async fn reject_storm_rejects_every_share() {
    let pool = pool_with(Faults { reject_storm: true, ..Faults::default() }).await;
    let (s, mut ev) = start(&pool);
    let j = job(&mut ev).await;
    for _ in 0..5 {
        let id = s.submit(&j.job_id, vec![0u8; 100]).await.unwrap();
        let (sid, kind) = until(&mut ev, |e| match e {
            SessionEvent::ShareRejected { submit_id, kind, .. } => Some((*submit_id, *kind)),
            _ => None,
        })
        .await;
        assert_eq!((sid, kind), (id, RejectKind::InvalidProof));
    }
    assert_eq!(pool.stats().rejected, 5);
}

#[tokio::test]
async fn mute_submits_give_ack_timeouts() {
    let pool = pool_with(Faults { mute_submits: true, ..Faults::default() }).await;
    let mut cfg = session_cfg(&pool, Dialect::Object);
    cfg.submit_ack_timeout = Duration::from_millis(300);
    let (s, mut ev) = PoolSession::spawn(cfg, Connector::default());
    let j = job(&mut ev).await;
    let id = s.submit(&j.job_id, vec![1, 2, 3]).await.unwrap();
    let t0 = tokio::time::Instant::now();
    let got = until(&mut ev, |e| match e {
        SessionEvent::SubmitAckTimeout { submit_id, job_id } => Some((*submit_id, job_id.clone())),
        _ => None,
    })
    .await;
    assert_eq!(got, (id, j.job_id));
    assert!(t0.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn stall_holds_back_jobs_for_the_configured_time() {
    let mut cfg = MockConfig::trivial();
    cfg.job_interval = Some(Duration::from_millis(100));
    cfg.faults.stall = Some(Duration::from_millis(1200));
    let pool = MockPool::start(cfg).await.unwrap();
    let (_s, mut ev) = start(&pool);
    job(&mut ev).await;
    let t0 = tokio::time::Instant::now();
    job(&mut ev).await;
    let gap = t0.elapsed();
    assert!(gap >= Duration::from_millis(1000), "second job after {gap:?}");
    // After the stall, jobs flow at the normal interval.
    let t1 = tokio::time::Instant::now();
    job(&mut ev).await;
    assert!(t1.elapsed() < Duration::from_millis(600));
}

#[tokio::test]
async fn eof_mid_submit_disconnects() {
    let pool = pool_with(Faults { eof_mid_submit: true, ..Faults::default() }).await;
    let (s, mut ev) = start(&pool);
    let j = job(&mut ev).await;
    let _ = s.submit(&j.job_id, vec![9; 32]).await;
    assert_eq!(disconnected(&mut ev).await, DisconnectReason::Eof);
    assert_eq!(pool.stats().submits, 1);
    assert!(matches!(s.submit(&j.job_id, vec![9; 32]).await, Err(SubmitError::Closed)));
}

#[tokio::test]
async fn oversized_line_trips_the_4mib_cap() {
    let pool = pool_with(Faults { oversized_line: true, ..Faults::default() }).await;
    let (_s, mut ev) = start(&pool);
    until(&mut ev, |e| matches!(e, SessionEvent::Authorized { .. }).then_some(())).await;
    assert_eq!(disconnected(&mut ev).await, DisconnectReason::LineTooLong);
}

#[tokio::test]
async fn proof_field_is_learned_after_three_format_rejects() {
    let mut cfg = MockConfig::trivial();
    cfg.accepted_fields = vec!["plain_proof_zst".into()];
    let pool = MockPool::start(cfg).await.unwrap();
    let (s, mut ev) = start(&pool); // starts with plain_proof
    let j = job(&mut ev).await;
    for _ in 0..3 {
        s.submit(&j.job_id, vec![7u8; 64]).await.unwrap();
        let kind = until(&mut ev, |e| match e {
            SessionEvent::ShareRejected { kind, .. } => Some(*kind),
            _ => None,
        })
        .await;
        assert_eq!(kind, RejectKind::Format);
    }
    let f = until(&mut ev, |e| match e {
        SessionEvent::ProofFieldSwitched { field } => Some(*field),
        _ => None,
    })
    .await;
    assert_eq!(f, ProofField::PlainProofZst);
    // The next submit uses the new field (junk bytes, so now it fails deserialization instead).
    s.submit(&j.job_id, vec![7u8; 64]).await.unwrap();
    until(&mut ev, |e| matches!(e, SessionEvent::ShareRejected { .. }).then_some(())).await;
    assert_eq!(pool.stats().last_submit_field.as_deref(), Some("plain_proof_zst"));
}

#[tokio::test]
async fn handshake_bytes_on_the_wire_follow_the_dialect() {
    let mut mcfg = MockConfig::trivial();
    mcfg.ack_type = Some("plain".into());
    let pool = MockPool::start(mcfg).await.unwrap();
    // LuckyPool-style: object + jsonrpc; the ack's `type` is surfaced.
    let mut cfg = session_cfg(&pool, Dialect::Object);
    cfg.jsonrpc = Some(true);
    cfg.agent = "a/1".into();
    let (_s, mut ev) = PoolSession::spawn(cfg, Connector::default());
    let t = until(&mut ev, |e| match e {
        SessionEvent::Authorized { proof_type } => Some(proof_type.clone()),
        _ => None,
    })
    .await;
    assert_eq!(t.as_deref(), Some("plain"));
    assert_eq!(
        pool.stats().last_authorize.as_deref(),
        Some(r#"{"jsonrpc":"2.0","id":1,"method":"mining.authorize","params":{"wallet":"prl1qtestwallet","worker":"rig0","agent":"a/1"}}"#)
    );
    // Kryptex: silent subscribe, then the array authorize with id 2.
    let mut cfg = session_cfg(&pool, Dialect::Kryptex);
    cfg.agent = "a/1".into();
    let (_s2, mut ev2) = PoolSession::spawn(cfg, Connector::default());
    job(&mut ev2).await;
    let st = pool.stats();
    assert_eq!(st.subscribes, 1);
    assert_eq!(
        st.last_authorize.as_deref(),
        Some(r#"{"jsonrpc":"2.0","id":2,"method":"mining.authorize","params":["prl1qtestwallet.rig0","x"]}"#)
    );
}
