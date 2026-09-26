//! End-to-end on the CPU (CI-safe: localhost only, no real pool, no GPU):
//! mock pool at trivial difficulty → `PoolSession` → `WorkUnit` → the official reference miner
//! (`try_mine_one`, m = n = 256, k = 2048, our 8x16 pattern) → local verify → submit → the mock
//! verifies with zk-pow and accepts. Then a proof for a superseded job is refused by the client
//! before it reaches the wire.
use std::time::Duration;

use rand::rngs::StdRng;
use rand::SeedableRng;
use spm_mockpool::{MockConfig, MockPool, TRIVIAL_NBITS};
use spm_pow::{check_cert_version_eligible, mining_config, SeedDerivation};
use spm_proto::client::{PoolSession, SessionConfig, SessionEvent, SubmitError};
use spm_proto::tls::{Connector, TlsMode, Transport};
use spm_proto::{Dialect, Job, ProofField};
use spm_work::{HashClass, Shape, WorkUnit};
use tokio::sync::mpsc::Receiver;
use zk_pow::api::verify::verify_plain_proof;
use zk_pow::ffi::mine::try_mine_one;

const SMALL: Shape = Shape { m: 256, n: 256, k: 2048, r: 128 };
const WAIT: Duration = Duration::from_secs(10);

async fn next_event(ev: &mut Receiver<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(WAIT, ev.recv()).await.expect("event in time").expect("session alive")
}

async fn wait_job(ev: &mut Receiver<SessionEvent>) -> Job {
    loop {
        match next_event(ev).await {
            SessionEvent::JobReceived(j) => return j,
            SessionEvent::Disconnected { reason } => panic!("disconnected: {reason:?}"),
            _ => {}
        }
    }
}

/// CPU "miner": the official reference miner on the work unit's header and config.
fn mine_cpu(wu: &WorkUnit) -> Vec<u8> {
    let header = wu.block_header().unwrap();
    let cfg = wu.mining_config().unwrap();
    assert_eq!(cfg.to_bytes(), mining_config(SMALL.k).unwrap().to_bytes());
    let (m, n, k) = (wu.shape.m as usize, wu.shape.n as usize, wu.shape.k as usize);
    for seed in 0..64u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        if let Some(proof) = try_mine_one(&mut rng, m, n, k, header, cfg, None, false, SeedDerivation::Salted).unwrap() {
            // Verify locally before submitting, exactly as the daemon will.
            check_cert_version_eligible(wu.cert_version, &proof).unwrap();
            verify_plain_proof(&header, &proof, Some(wu.nbits_share), SeedDerivation::Salted).expect("local verify");
            return bincode::serialize(&proof).unwrap();
        }
    }
    panic!("no share at trivial difficulty after 64 attempts");
}

async fn end_to_end(dialect: Dialect, field: ProofField) {
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let mut cfg = SessionConfig::new("127.0.0.1", pool.port(), dialect, "prl1qtestwallet", "rig0");
    cfg.tls = TlsMode::Off;
    cfg.proof_field = field;
    let (session, mut ev) = PoolSession::spawn(cfg, Connector::default());

    assert_eq!(next_event(&mut ev).await, SessionEvent::Connected { transport: Transport::Plain, plain_fallback: false });
    let mut job = None;
    let mut authorized = false;
    while job.is_none() || !authorized {
        match next_event(&mut ev).await {
            SessionEvent::Authorized { .. } => authorized = true,
            SessionEvent::JobReceived(j) => job = Some(j),
            other => panic!("unexpected {other:?}"),
        }
    }
    let job = job.unwrap();
    assert_eq!(session.current_job_id().as_deref(), Some(job.job_id.as_str()));

    // Job → work unit on the small shape.
    let wu = WorkUnit::build(&job, SMALL, 1, 1).unwrap();
    assert_eq!(wu.nbits_share, TRIVIAL_NBITS);
    assert_eq!(wu.nbits_share, pool.nbits_share());
    assert_eq!(wu.block_nbits, TRIVIAL_NBITS);
    assert_eq!(wu.classify(&[0u8; 32]), HashClass::Block, "share and block bounds coincide on the mock");

    let proof = tokio::task::spawn_blocking({
        let wu = wu.clone();
        move || mine_cpu(&wu)
    })
    .await
    .unwrap();

    let submit_id = session.submit(&job.job_id, proof.clone()).await.unwrap();
    match next_event(&mut ev).await {
        SessionEvent::ShareAccepted { submit_id: id, job_id } => {
            assert_eq!((id, job_id.as_str()), (submit_id, job.job_id.as_str()));
        }
        SessionEvent::ShareRejected { reason, .. } => panic!("rejected: {reason}"),
        other => panic!("unexpected {other:?}"),
    }
    let st = pool.stats();
    assert_eq!((st.submits, st.verified_ok, st.accepted, st.rejected), (1, 1, 1, 0));
    assert_eq!(st.last_submit_field.as_deref(), Some(field.key()));

    // A new job supersedes the old one: a proof for the old job is refused before sending.
    let new_id = pool.new_job().unwrap();
    let j2 = wait_job(&mut ev).await;
    assert_eq!(j2.job_id, new_id);
    let e = session.submit(&job.job_id, proof).await.unwrap_err();
    assert_eq!(e, SubmitError::Stale { job_id: job.job_id.clone(), current: Some(new_id) });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pool.stats().submits, 1, "the stale proof never reached the pool");

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_share_accepted_object_dialect_plain_proof() {
    end_to_end(Dialect::Object, ProofField::PlainProof).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_share_accepted_kryptex_dialect_zstd_proof() {
    end_to_end(Dialect::Kryptex, ProofField::PlainProofZst).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_share_accepted_kryptex_v2_gzip_proof() {
    end_to_end(Dialect::KryptexV2, ProofField::PlainProof).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_proof_is_rejected_by_the_verifier() {
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let mut cfg = SessionConfig::new("127.0.0.1", pool.port(), Dialect::Object, "prl1qtestwallet", "rig0");
    cfg.tls = TlsMode::Off;
    let (session, mut ev) = PoolSession::spawn(cfg, Connector::default());
    let job = wait_job(&mut ev).await;
    let wu = WorkUnit::build(&job, SMALL, 1, 1).unwrap();
    let proof = tokio::task::spawn_blocking(move || mine_cpu(&wu)).await.unwrap();
    // Corrupt the proof: flip the noise rank (first fields are m, n, k, noise_rank as u64 LE).
    let mut bad: zk_pow::ffi::plain_proof::PlainProof = bincode::deserialize(&proof).unwrap();
    bad.noise_rank = 256;
    let bad = bincode::serialize(&bad).unwrap();
    let id = session.submit(&job.job_id, bad).await.unwrap();
    loop {
        match next_event(&mut ev).await {
            SessionEvent::ShareRejected { submit_id, kind, reason, .. } => {
                assert_eq!(submit_id, id);
                assert!(kind.is_invalid(), "{kind:?}: {reason}");
                break;
            }
            SessionEvent::Authorized { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    // The valid proof is still accepted afterwards (not a duplicate of the bad one).
    let id = session.submit(&job.job_id, proof.clone()).await.unwrap();
    assert!(matches!(next_event(&mut ev).await, SessionEvent::ShareAccepted { submit_id, .. } if submit_id == id));
    // Re-submitting the same proof is a duplicate.
    let id = session.submit(&job.job_id, proof).await.unwrap();
    match next_event(&mut ev).await {
        SessionEvent::ShareRejected { submit_id, kind, .. } => {
            assert_eq!(submit_id, id);
            assert_eq!(kind, spm_proto::RejectKind::Duplicate);
        }
        other => panic!("unexpected {other:?}"),
    }
    session.shutdown().await;
}
