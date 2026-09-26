//! The worker's IPC state machine against a fake daemon, with the CPU engine (no GPU): Ready
//! after the known-answer test, heartbeats and stats, verified proofs, the pause/resume
//! handshake with its ACK file, the epoch rule (a new work unit cancels the attempt within a
//! chunk), duty cycle, Release / Shutdown / disconnect, and the fault paths (KAT, canary).

mod common;

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use spm_coexist::handshake::AckState;
use spm_cpuref::{verify_v3, IncompleteBlockHeader, PlainProof};
use spm_ipc::{FaultKind, ToDaemon, ToWorker};
use spm_work::Shape;
use spm_worker::cpu::{CpuEngine, CpuOptions};
use spm_worker::Exit;

const SMALL: Shape = Shape {
    m: 128,
    n: 128,
    k: 2048,
    r: 128,
};
const T: Duration = Duration::from_secs(30);

fn ready(conn: &mut common::Conn) -> (bool, String) {
    match conn.wait_for(T, |m| matches!(m, ToDaemon::Ready { .. })) {
        Some(ToDaemon::Ready { kat_ok, device }) => (kat_ok, device),
        other => panic!("no Ready: {other:?}"),
    }
}

#[test]
fn mines_verified_proofs_with_heartbeats_and_stats_then_releases() {
    let d = FakeDaemon::new();
    let (mut conn, worker) = d.spawn(d.options(), || Ok(CpuEngine::new(CpuOptions::default())));
    let (kat_ok, device) = ready(&mut conn);
    assert!(kat_ok);
    assert!(device.contains("cpu"));
    // Share bound 2^251: about 1 tile in 32 is a share (4 per 128-tile attempt).
    let wu = work_unit(SMALL, 233, 1, 11);
    conn.send(ToWorker::SetJob {
        wu: Box::new(wu.clone()),
    });
    conn.send(ToWorker::Resume);
    let proof = conn
        .wait_for(T, |m| matches!(m, ToDaemon::Proof { .. }))
        .expect("a proof");
    let ToDaemon::Proof {
        wu_id,
        session_id,
        job_id,
        is_block,
        digest,
        t_rows,
        t_cols,
        proof_bincode,
    } = proof
    else {
        unreachable!()
    };
    assert_eq!(
        (wu_id, session_id, job_id.as_str()),
        (11, 3, wu.job_id.as_str())
    );
    assert!(!is_block);
    assert!(spm_cpuref::U256::from_little_endian(&digest) <= wu.share_bound());
    let p: PlainProof = bincode::deserialize(&proof_bincode).unwrap();
    assert_eq!(p.a.row_indices[0], t_rows as usize);
    assert_eq!(p.bt.row_indices[0], t_cols as usize);
    let header = IncompleteBlockHeader::from_bytes(&wu.header).unwrap();
    verify_v3(&header, &p, Some(wu.nbits_share)).expect("the official verifier accepts it");
    // Stats with credited work, and heartbeats at the configured period.
    let stats = conn
        .wait_for(
            T,
            |m| matches!(m, ToDaemon::Stats { attempts, .. } if *attempts > 0),
        )
        .expect("stats with attempts");
    let ToDaemon::Stats {
        credited_macs,
        tiles,
        attempts,
        ..
    } = stats
    else {
        unreachable!()
    };
    assert_eq!(credited_macs, attempts * 128 * 128 * 2048);
    assert_eq!(tiles, attempts * 128);
    let window = conn.collect(Duration::from_millis(600));
    let beats = window
        .iter()
        .filter(|m| matches!(m, ToDaemon::Heartbeat { .. }))
        .count();
    assert!(
        (4..=8).contains(&beats),
        "{beats} heartbeats in 600 ms at 100 ms"
    );
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
    // The resume was acknowledged.
    assert_eq!(conn.ack().map(|a| a.state), Some(AckState::Running));
    conn.send(ToWorker::Release);
    let out = worker.join(T);
    assert_eq!(out.exit, Exit::Release);
    assert!(out.kat_ok && out.attempts >= 1 && out.proofs >= 1);
    assert!(out.canaries >= 1, "one canary per attempt with hits");
}

#[test]
fn pause_is_acknowledged_fast_and_stops_gpu_work_until_resume() {
    let d = FakeDaemon::new();
    let engine = CpuEngine::new(CpuOptions {
        chunk_tiles: 8,
        chunk_delay: Duration::from_millis(5),
        ..CpuOptions::default()
    });
    let counters = engine.counters();
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    assert!(ready(&mut conn).0);
    // No work before the first Resume (the chunks so far are the known-answer test's).
    let kat_chunks = counters.chunks_run.load(Ordering::SeqCst);
    conn.send(ToWorker::SetJob {
        wu: Box::new(work_unit(SMALL, 200, 2, 1)),
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(counters.chunks_run.load(Ordering::SeqCst), kat_chunks);
    conn.send(ToWorker::Resume);
    let (a, _) = conn
        .wait_ack(T, |a| a.state == AckState::Running)
        .expect("resume ACK");
    assert_eq!(a.seq, 1);
    let start = Instant::now();
    while counters.chunks_run.load(Ordering::SeqCst) < kat_chunks + 5 {
        assert!(start.elapsed() < T);
        thread::sleep(Duration::from_millis(2));
    }
    let mut seq = 1;
    for round in 0..3 {
        conn.send(ToWorker::Pause);
        seq += 1;
        let (_, latency) = conn
            .wait_ack(T, |a| a.state == AckState::Paused && a.seq == seq)
            .expect("pause ACK");
        assert!(
            latency < Duration::from_millis(100),
            "pause ACK after {latency:?}"
        );
        let frozen = counters.chunks_run.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            counters.chunks_run.load(Ordering::SeqCst),
            frozen,
            "no chunk may run while paused"
        );
        if round == 1 {
            // A repeated pause is acknowledged again.
            conn.send(ToWorker::Pause);
            seq += 1;
            conn.wait_ack(T, |a| a.state == AckState::Paused && a.seq == seq)
                .expect("repeated pause ACK");
        }
        conn.send(ToWorker::Resume);
        seq += 1;
        conn.wait_ack(T, |a| a.state == AckState::Running && a.seq == seq)
            .expect("resume ACK");
        let start = Instant::now();
        while counters.chunks_run.load(Ordering::SeqCst) < frozen + 3 {
            assert!(start.elapsed() < T, "no progress after resume");
            thread::sleep(Duration::from_millis(2));
        }
    }
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
    conn.send(ToWorker::Shutdown);
    assert_eq!(worker.join(T).exit, Exit::Shutdown);
}

#[test]
fn a_new_work_unit_cancels_the_attempt_within_one_chunk() {
    let d = FakeDaemon::new();
    let engine = CpuEngine::new(CpuOptions {
        chunk_tiles: 4,
        chunk_delay: Duration::from_millis(20),
        ..CpuOptions::default()
    });
    let counters = engine.counters();
    let created = engine.created.clone();
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    assert!(ready(&mut conn).0);
    let first = work_unit(SMALL, 200, 3, 1);
    conn.send(ToWorker::SetJob {
        wu: Box::new(first.clone()),
    });
    conn.send(ToWorker::Resume);
    let start = Instant::now();
    while counters.chunks_run.load(Ordering::SeqCst) < 3 {
        assert!(start.elapsed() < T);
        thread::sleep(Duration::from_millis(1));
    }
    // The same work unit again changes nothing.
    conn.send(ToWorker::SetJob {
        wu: Box::new(first),
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        counters.attempts_set.load(Ordering::SeqCst),
        2,
        "KAT + the first attempt"
    );
    assert_eq!(counters.chunks_aborted.load(Ordering::SeqCst), 0);
    // A new work unit with another shape and header.
    let second = work_unit(
        Shape {
            m: 64,
            n: 192,
            k: 2048,
            r: 128,
        },
        200,
        4,
        2,
    );
    let before = counters.chunks_run.load(Ordering::SeqCst);
    conn.send(ToWorker::SetJob {
        wu: Box::new(second),
    });
    let start = Instant::now();
    while created.lock().unwrap().len() < 3 {
        assert!(start.elapsed() < T, "the new job was never created");
        thread::sleep(Duration::from_millis(1));
    }
    let switched = start.elapsed();
    let ran = counters.chunks_run.load(Ordering::SeqCst) - before;
    assert!(ran <= 1, "{ran} chunks of the old attempt ran after SetJob");
    assert!(counters.chunks_aborted.load(Ordering::SeqCst) >= 1);
    assert!(
        switched < Duration::from_millis(500),
        "job switch took {switched:?}"
    );
    assert_eq!(created.lock().unwrap()[2], (64, 192, 2048));
    conn.send(ToWorker::Release);
    assert_eq!(worker.join(T).exit, Exit::Release);
}

#[test]
fn duty_cycle_spaces_the_chunks() {
    let d = FakeDaemon::new();
    let engine = CpuEngine::new(CpuOptions {
        chunk_tiles: 1,
        chunk_delay: Duration::from_millis(10),
        ..CpuOptions::default()
    });
    let counters = engine.counters();
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    assert!(ready(&mut conn).0);
    conn.send(ToWorker::SetJob {
        wu: Box::new(work_unit(SMALL, 200, 5, 1)),
    });
    conn.send(ToWorker::Resume);
    let rate = |ms: u64| {
        let c0 = counters.chunks_run.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(ms));
        (counters.chunks_run.load(Ordering::SeqCst) - c0) as f64 / ms as f64
    };
    thread::sleep(Duration::from_millis(200));
    let full = rate(600);
    conn.send(ToWorker::SetDuty { pct: 25 });
    thread::sleep(Duration::from_millis(100));
    let quarter = rate(600);
    assert!(full > 0.0);
    assert!(
        quarter < 0.5 * full,
        "duty 25 %: {quarter:.3} chunks/ms vs {full:.3} at 100 %"
    );
    conn.send(ToWorker::Release);
    assert_eq!(worker.join(T).exit, Exit::Release);
}

#[test]
fn shutdown_release_and_disconnect_end_the_worker() {
    // Shutdown while idle.
    let d = FakeDaemon::new();
    let (mut conn, worker) = d.spawn(d.options(), || Ok(CpuEngine::new(CpuOptions::default())));
    assert!(ready(&mut conn).0);
    conn.send(ToWorker::Shutdown);
    assert_eq!(worker.join(T).exit, Exit::Shutdown);
    // The daemon goes away while mining.
    let d = FakeDaemon::new();
    let (mut conn, worker) = d.spawn(d.options(), || Ok(CpuEngine::new(CpuOptions::default())));
    assert!(ready(&mut conn).0);
    conn.send(ToWorker::SetJob {
        wu: Box::new(work_unit(SMALL, 200, 6, 1)),
    });
    conn.send(ToWorker::Resume);
    thread::sleep(Duration::from_millis(200));
    conn.close();
    assert!(matches!(worker.join(T).exit, Exit::Disconnected(_)));
    // Release while paused.
    let d = FakeDaemon::new();
    let (mut conn, worker) = d.spawn(d.options(), || Ok(CpuEngine::new(CpuOptions::default())));
    assert!(ready(&mut conn).0);
    conn.send(ToWorker::Pause);
    conn.wait_ack(T, |a| a.state == AckState::Paused)
        .expect("pause ACK");
    conn.send(ToWorker::Release);
    assert_eq!(worker.join(T).exit, Exit::Release);
}

#[test]
fn a_failed_known_answer_test_reports_and_never_mines() {
    let d = FakeDaemon::new();
    let engine = CpuEngine::new(CpuOptions {
        corrupt_dump: true,
        ..CpuOptions::default()
    });
    let counters = engine.counters();
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    assert!(!ready(&mut conn).0, "kat_ok must be false");
    let fault = conn.wait_for(T, |m| matches!(m, ToDaemon::Fault { .. }));
    assert!(
        matches!(
            fault,
            Some(ToDaemon::Fault {
                kind: FaultKind::KatFailed,
                ..
            })
        ),
        "{fault:?}"
    );
    let out = worker.join(T);
    assert!(matches!(out.exit, Exit::Fault(FaultKind::KatFailed, _)));
    assert!(!out.kat_ok);
    assert_eq!(out.attempts, 0);
    // Only the KAT's own job ever existed.
    assert_eq!(counters.jobs_created.load(Ordering::SeqCst), 1);
}

#[test]
fn corrupted_hits_fail_the_known_answer_test() {
    let d = FakeDaemon::new();
    // The KAT runs in dump mode (records intact); mining reports corrupted digests.
    let engine = CpuEngine::new(CpuOptions {
        corrupt_digests: true,
        ..CpuOptions::default()
    });
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    // The KAT also compares the hit ring, so it catches the fault before any mining.
    assert!(!ready(&mut conn).0);
    let out = worker.join(T);
    assert!(matches!(out.exit, Exit::Fault(FaultKind::KatFailed, _)));
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Proof { .. })));
}

#[test]
fn a_fault_after_ready_stops_mining_with_a_canary_fault() {
    let d = FakeDaemon::new();
    let engine = FlakyEngine {
        inner: CpuEngine::new(CpuOptions::default()),
        jobs: 0,
    };
    let (mut conn, worker) = d.spawn(d.options(), move || Ok(engine));
    assert!(ready(&mut conn).0);
    // Share bound 2^251 so there are shares to (not) submit.
    conn.send(ToWorker::SetJob {
        wu: Box::new(work_unit(SMALL, 233, 8, 1)),
    });
    conn.send(ToWorker::Resume);
    let fault = conn.wait_for(T, |m| matches!(m, ToDaemon::Fault { .. }));
    assert!(
        matches!(
            fault,
            Some(ToDaemon::Fault {
                kind: FaultKind::CanaryMismatch,
                ..
            })
        ),
        "{fault:?}"
    );
    let out = worker.join(T);
    assert!(matches!(
        out.exit,
        Exit::Fault(FaultKind::CanaryMismatch, _)
    ));
    assert_eq!(out.proofs, 0);
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Proof { .. })));
}

/// Passes the KAT, then flips a digest bit of every hit it reports.
struct FlakyEngine {
    inner: CpuEngine,
    jobs: u32,
}

impl spm_worker::Engine for FlakyEngine {
    fn device(&self) -> String {
        self.inner.device()
    }
    fn abort_flag(&self) -> std::sync::Arc<dyn spm_worker::AbortFlag> {
        self.inner.abort_flag()
    }
    fn create_job(&mut self, spec: &spm_worker::JobSpec) -> Result<(), spm_worker::EngineError> {
        self.jobs += 1;
        self.inner.create_job(spec)
    }
    fn destroy_job(&mut self) {
        self.inner.destroy_job()
    }
    fn set_attempt(
        &mut self,
        a: &[u8; 32],
        b: &[u8; 32],
        p: &[i8],
    ) -> Result<(), spm_worker::EngineError> {
        self.inner.set_attempt(a, b, p)
    }
    fn run_chunk(&mut self) -> Result<spm_worker::ChunkReport, spm_worker::EngineError> {
        self.inner.run_chunk()
    }
    fn hits(&mut self) -> Result<(Vec<spm_worker::Hit>, u32), spm_worker::EngineError> {
        let (mut hits, total) = self.inner.hits()?;
        if self.jobs > 1 {
            for h in &mut hits {
                h.digest[31] ^= 0x01;
            }
        }
        Ok((hits, total))
    }
    fn dump(&mut self) -> Result<Vec<spm_cpuref::TileResult>, spm_worker::EngineError> {
        self.inner.dump()
    }
}
