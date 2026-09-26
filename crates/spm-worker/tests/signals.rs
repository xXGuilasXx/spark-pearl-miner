//! SIGUSR1 / SIGUSR2 drive the same pause/resume state machine as the IPC frames (the last
//! command wins, every command is acknowledged in `worker.ack`), and SIGTERM makes the worker
//! exit at its next quiescent point. Its own test binary: the handlers are process-wide.

mod common;

use std::process::Command;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use spm_coexist::handshake::AckState;
use spm_ipc::{ToDaemon, ToWorker};
use spm_work::Shape;
use spm_worker::cpu::{CpuEngine, CpuOptions};
use spm_worker::Exit;

const T: Duration = Duration::from_secs(30);

fn kill(sig: &str) {
    let pid = std::process::id().to_string();
    let st = Command::new("kill").args([sig, &pid]).status().unwrap();
    assert!(st.success());
}

#[test]
fn signals_pause_resume_and_terminate_the_worker() {
    let d = FakeDaemon::new();
    let mut opts = d.options();
    opts.signals = true;
    let engine = CpuEngine::new(CpuOptions {
        chunk_tiles: 8,
        chunk_delay: Duration::from_millis(5),
        ..CpuOptions::default()
    });
    let counters = engine.counters();
    let (mut conn, worker) = d.spawn(opts, move || Ok(engine));
    assert!(matches!(
        conn.wait_for(T, |m| matches!(m, ToDaemon::Ready { .. })),
        Some(ToDaemon::Ready { kat_ok: true, .. })
    ));
    conn.send(ToWorker::SetJob {
        wu: Box::new(work_unit(
            Shape {
                m: 128,
                n: 128,
                k: 2048,
                r: 128,
            },
            200,
            9,
            1,
        )),
    });
    conn.send(ToWorker::Resume);
    conn.wait_ack(T, |a| a.state == AckState::Running && a.seq == 1)
        .expect("resume ACK");
    let progress = |from: u64| {
        let start = Instant::now();
        while counters.chunks_run.load(Ordering::SeqCst) < from + 3 {
            assert!(start.elapsed() < T, "no progress");
            thread::sleep(Duration::from_millis(2));
        }
    };
    progress(counters.chunks_run.load(Ordering::SeqCst));

    kill("-USR1");
    let (_, latency) = conn
        .wait_ack(T, |a| a.state == AckState::Paused && a.seq == 2)
        .expect("SIGUSR1 ACK");
    assert!(latency < Duration::from_millis(100), "{latency:?}");
    let frozen = counters.chunks_run.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(counters.chunks_run.load(Ordering::SeqCst), frozen);

    kill("-USR2");
    conn.wait_ack(T, |a| a.state == AckState::Running && a.seq == 3)
        .expect("SIGUSR2 ACK");
    progress(frozen);

    // IPC pause, then a signal resume: the last command wins.
    conn.send(ToWorker::Pause);
    conn.wait_ack(T, |a| a.state == AckState::Paused && a.seq == 4)
        .expect("Pause ACK");
    kill("-USR2");
    conn.wait_ack(T, |a| a.state == AckState::Running && a.seq == 5)
        .expect("SIGUSR2 ACK");
    progress(counters.chunks_run.load(Ordering::SeqCst));

    kill("-TERM");
    let out = worker.join(T);
    assert_eq!(out.exit, Exit::Signal("SIGTERM"));
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
}
