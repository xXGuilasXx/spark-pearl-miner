//! End to end on the GPU (only with `SPM_GPU_TESTS=1`): the real worker over libspm_cuda against
//! a fake daemon. Short bursts that fit next to a resident vLLM (≤ ~10 s, ≤ 2 GiB of device
//! memory).
//!
//! * `gpu_worker_mines_verified_proofs`: KAT, then 4096 × 4096 × 2048 with an easy share bound;
//!   at least three proofs, each accepted by the official verifier; canaries pass; a pause is
//!   acknowledged within 100 ms; NVML clock and power in `Stats`.
//! * `gpu_worker_binary_frees_the_gpu_on_release`: the `spark-pearl-gpu-worker` process mines a
//!   share, then exits with status 0 on `Release`, and `nvidia-smi` no longer lists it.
//! * `gpu_worker_default_shape` (`--ignored`): the production shape 131072² × 4096 for a few
//!   seconds: host and device job build times, attempt time, pause ACK latency and the cancel
//!   latency of a new work unit.

mod common;

use std::time::{Duration, Instant};

use common::*;
use spm_coexist::handshake::AckState;
use spm_cpuref::{verify_v3, IncompleteBlockHeader, PlainProof};
use spm_ipc::{ToDaemon, ToWorker};
use spm_work::Shape;
use spm_worker::gpu::GpuEngine;
use spm_worker::{Exit, Outcome};

fn gpu_tests() -> bool {
    if std::env::var("SPM_GPU_TESTS").ok().as_deref() == Some("1") {
        true
    } else {
        eprintln!("skipped (set SPM_GPU_TESTS=1)");
        false
    }
}

const T: Duration = Duration::from_secs(30);

fn report(out: &Outcome) {
    let t = &out.timings;
    eprintln!(
        "exit {:?}; attempts {}, proofs {}, canaries {}; host build {:?}, device build {:?}; \
         attempt wall {:?} (kernel {:?}, set_attempt {:?}), max chunk {:?}; cancels {:?}",
        out.exit,
        out.attempts,
        out.proofs,
        out.canaries,
        t.host_builds,
        t.device_builds,
        t.mean_attempt_wall(),
        t.mean_attempt_kernel(),
        t.mean_set_attempt(),
        t.max_chunk,
        t.cancels
    );
}

#[test]
fn gpu_worker_mines_verified_proofs() {
    if !gpu_tests() {
        return;
    }
    let d = FakeDaemon::new();
    let mut opts = d.options();
    opts.telemetry = true;
    opts.memory_guard = true;
    let (mut conn, worker) = d.spawn(opts, GpuEngine::new);
    let Some(ToDaemon::Ready { kat_ok, device }) =
        conn.wait_for(T, |m| matches!(m, ToDaemon::Ready { .. }))
    else {
        panic!("no Ready");
    };
    eprintln!("device: {device}");
    assert!(kat_ok, "known-answer test on the GPU");
    // 131072 tiles per attempt; share bound 2^241: ~4 shares per attempt.
    let shape = Shape {
        m: 4096,
        n: 4096,
        k: 2048,
        r: 128,
    };
    let wu = work_unit(shape, 223, 0x31, 5);
    let header = IncompleteBlockHeader::from_bytes(&wu.header).unwrap();
    conn.send(ToWorker::SetJob {
        wu: Box::new(wu.clone()),
    });
    conn.send(ToWorker::Resume);
    let start = Instant::now();
    let mut proofs = 0;
    while proofs < 3 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "only {proofs} proofs"
        );
        let Some(ToDaemon::Proof {
            wu_id,
            digest,
            t_rows,
            t_cols,
            proof_bincode,
            ..
        }) = conn.wait_for(T, |m| matches!(m, ToDaemon::Proof { .. }))
        else {
            panic!("no proof");
        };
        assert_eq!(wu_id, 5);
        let p: PlainProof = bincode::deserialize(&proof_bincode).unwrap();
        verify_v3(&header, &p, Some(wu.nbits_share))
            .expect("the official verifier accepts the GPU share");
        assert!(spm_cpuref::U256::from_little_endian(&digest) <= wu.share_bound());
        eprintln!(
            "proof {proofs}: tile ({t_rows}, {t_cols}), {} bytes",
            proof_bincode.len()
        );
        proofs += 1;
    }
    let stats = conn
        .wait_for(
            T,
            |m| matches!(m, ToDaemon::Stats { attempts, .. } if *attempts > 0),
        )
        .expect("stats");
    eprintln!("{stats:?}");
    if let ToDaemon::Stats {
        sm_clock_mhz,
        power_w,
        ..
    } = stats
    {
        assert!(sm_clock_mhz > 0 && power_w > 0.0, "NVML telemetry");
    }
    // Pause: acknowledged once nothing is queued on the device.
    conn.send(ToWorker::Pause);
    let (_, latency) = conn
        .wait_ack(T, |a| a.state == AckState::Paused)
        .expect("pause ACK");
    eprintln!("pause ACK after {latency:?}");
    assert!(latency < Duration::from_millis(100));
    conn.send(ToWorker::Resume);
    conn.wait_ack(T, |a| a.state == AckState::Running)
        .expect("resume ACK");
    std::thread::sleep(Duration::from_millis(200));
    conn.send(ToWorker::Release);
    let out = worker.join(T);
    report(&out);
    assert_eq!(out.exit, Exit::Release);
    assert!(out.canaries >= 1, "canary tiles checked");
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
}

#[test]
#[ignore = "production shape: 1.06 GiB of device memory for ~6 s; run with --ignored"]
fn gpu_worker_default_shape() {
    if !gpu_tests() {
        return;
    }
    let d = FakeDaemon::new();
    let mut opts = d.options();
    opts.telemetry = true;
    opts.memory_guard = true;
    let (mut conn, worker) = d.spawn(opts, GpuEngine::new);
    let Some(ToDaemon::Ready { kat_ok, .. }) =
        conn.wait_for(T, |m| matches!(m, ToDaemon::Ready { .. }))
    else {
        panic!("no Ready");
    };
    assert!(kat_ok);
    // Share bound 2^230 over 2^27 tiles: ~2 shares per attempt, so full-size PlainProofs (A and
    // Bᵀ openings in 2^19-leaf trees) go through the official verifier too.
    let wu = work_unit(Shape::MINING, 211, 0x42, 1);
    conn.send(ToWorker::SetJob {
        wu: Box::new(wu.clone()),
    });
    conn.send(ToWorker::Resume);
    let stats = conn
        .wait_for(
            T,
            |m| matches!(m, ToDaemon::Stats { attempts, .. } if *attempts > 0),
        )
        .expect("a completed attempt");
    eprintln!("{stats:?}");
    std::thread::sleep(Duration::from_millis(2500));
    conn.send(ToWorker::Pause);
    let (_, latency) = conn
        .wait_ack(T, |a| a.state == AckState::Paused)
        .expect("pause ACK");
    eprintln!("pause ACK after {latency:?} (mid-attempt)");
    assert!(latency < Duration::from_millis(100));
    conn.send(ToWorker::Resume);
    conn.wait_ack(T, |a| a.state == AckState::Running)
        .expect("resume ACK");
    std::thread::sleep(Duration::from_millis(500));
    // A new work unit for the same job (new share target): the attempt is cancelled, the host
    // and device jobs are kept.
    let mut wu2 = work_unit(Shape::MINING, 210, 0x42, 2);
    wu2.job_id = "same-header".into();
    assert_eq!(wu2.job_key, wu.job_key);
    conn.send(ToWorker::SetJob { wu: Box::new(wu2) });
    std::thread::sleep(Duration::from_millis(1500));
    conn.send(ToWorker::Release);
    let out = worker.join(T);
    report(&out);
    let t = &out.timings;
    assert_eq!(out.exit, Exit::Release);
    assert_eq!(t.host_builds.len(), 1, "same job key: no rebuild");
    assert_eq!(t.device_builds.len(), 1);
    assert!(!t.cancels.is_empty() && t.cancels[0] < Duration::from_millis(50));
    assert!(out.canaries >= 1);
    assert!(out.proofs >= 1, "a verified full-size proof");
    let header = IncompleteBlockHeader::from_bytes(&wu.header).unwrap();
    let mut checked = 0;
    for m in &conn.seen {
        if let ToDaemon::Proof {
            wu_id: 1,
            proof_bincode,
            ..
        } = m
        {
            let p: PlainProof = bincode::deserialize(proof_bincode).unwrap();
            verify_v3(&header, &p, Some(wu.nbits_share)).expect("full-size proof verifies");
            checked += 1;
        }
    }
    eprintln!("{checked} full-size proofs re-verified by the test");
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
}

fn compute_pids() -> Vec<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid", "--format=csv,noheader"])
        .output()
        .expect("nvidia-smi");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
fn gpu_worker_binary_frees_the_gpu_on_release() {
    if !gpu_tests() {
        return;
    }
    let d = FakeDaemon::new();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spark-pearl-gpu-worker"))
        .arg("--attach")
        .arg(&d.sock)
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn the worker binary");
    let pid = child.id();
    let mut conn = d.accept();
    let Some(ToDaemon::Ready { kat_ok, .. }) =
        conn.wait_for(T, |m| matches!(m, ToDaemon::Ready { .. }))
    else {
        panic!("no Ready");
    };
    assert!(kat_ok);
    assert!(
        compute_pids().contains(&pid),
        "the worker holds a CUDA context"
    );
    let shape = Shape {
        m: 2048,
        n: 2048,
        k: 2048,
        r: 128,
    };
    // 32768 tiles, share bound 2^245: ~16 shares per attempt.
    let wu = work_unit(shape, 227, 0x77, 9);
    conn.send(ToWorker::SetJob {
        wu: Box::new(wu.clone()),
    });
    conn.send(ToWorker::Resume);
    let Some(ToDaemon::Proof { proof_bincode, .. }) =
        conn.wait_for(T, |m| matches!(m, ToDaemon::Proof { .. }))
    else {
        panic!("no proof");
    };
    let p: PlainProof = bincode::deserialize(&proof_bincode).unwrap();
    let header = IncompleteBlockHeader::from_bytes(&wu.header).unwrap();
    verify_v3(&header, &p, Some(wu.nbits_share)).unwrap();
    let released = Instant::now();
    conn.send(ToWorker::Release);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(
            released.elapsed() < Duration::from_secs(10),
            "the worker did not exit"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    eprintln!(
        "worker exited {status} {:?} after Release",
        released.elapsed()
    );
    assert!(status.success());
    assert!(!compute_pids().contains(&pid), "the CUDA context is gone");
    assert!(!conn
        .seen
        .iter()
        .any(|m| matches!(m, ToDaemon::Fault { .. })));
}
