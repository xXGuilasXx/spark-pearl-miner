//! WorkerSupervisor with fake workers on worker.sock (external launch mode, no process spawned):
//! heartbeat watchdog, and the "possible hardware fault" alert after 3 failures in 10 minutes.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spm::supervisor::{SupCmd, Supervisor, WorkerEvent};
use spm_api::config::LaunchMode;
use spm_ipc::{read_frame, write_frame, ToDaemon, ToWorker};
use spm_proto::Job;
use spm_work::WorkUnit;
use tokio::sync::mpsc::UnboundedReceiver;

fn sock(name: &str) -> PathBuf {
    let d = PathBuf::from(format!("/tmp/spm-sup-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("worker.sock")
}

fn wu() -> Box<WorkUnit> {
    let job = Job {
        job_id: "j_1".into(),
        header: spm_mockpool::trivial_header(),
        target: spm_pow::target_from_compact(spm_mockpool::TRIVIAL_NBITS),
        height: Some(1),
        diff: Some(1),
        cert_version: Some(3),
    };
    Box::new(WorkUnit::build(&job, spm::worker_sim::SIM_SHAPE, 1, 1).unwrap())
}

/// A fake worker: attach, answer Hello with Ready, and hand back the stream.
fn attach(path: &PathBuf) -> UnixStream {
    let mut s = UnixStream::connect(path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert!(matches!(read_frame::<_, ToWorker>(&mut s).unwrap(), ToWorker::Hello { .. }));
    write_frame(&mut s, &ToDaemon::Ready { kat_ok: true, device: "fake".into() }).unwrap();
    s
}

async fn next_event(rx: &mut UnboundedReceiver<WorkerEvent>, timeout: Duration) -> WorkerEvent {
    tokio::time::timeout(timeout, rx.recv()).await.expect("worker event in time").expect("supervisor alive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_worker_trips_the_watchdog() {
    let path = sock("wd");
    let (cmd, mut ev, _st) = Supervisor::start(path.clone(), PathBuf::from("/bin/false"), LaunchMode::External, false, 1000).await.unwrap();
    cmd.send(SupCmd::Desire { job: Some(wu()), run: true }).unwrap();
    let p = path.clone();
    let mut s = tokio::task::spawn_blocking(move || attach(&p)).await.unwrap();
    assert!(matches!(next_event(&mut ev, Duration::from_secs(5)).await, WorkerEvent::Ready { .. }));
    // The supervisor pushes the job and resumes.
    let (a, b) = tokio::task::spawn_blocking(move || {
        let a = read_frame::<_, ToWorker>(&mut s).unwrap();
        let b = read_frame::<_, ToWorker>(&mut s).unwrap();
        std::mem::forget(s); // stay connected but silent
        (a, b)
    })
    .await
    .unwrap();
    assert!(matches!(a, ToWorker::SetJob { .. }), "{a:?}");
    assert_eq!(b, ToWorker::Resume);
    let t0 = Instant::now();
    match next_event(&mut ev, Duration::from_secs(8)).await {
        WorkerEvent::Lost { reason, failure } => {
            assert!(failure);
            assert!(reason.contains("heartbeat"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_secs(4) && waited < Duration::from_secs(7), "{waited:?}");
    cmd.send(SupCmd::Shutdown).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_failures_in_ten_minutes_raise_the_hardware_fault_alert() {
    let path = sock("hw");
    let (cmd, mut ev, st) = Supervisor::start(path.clone(), PathBuf::from("/bin/false"), LaunchMode::External, false, 1000).await.unwrap();
    cmd.send(SupCmd::Desire { job: Some(wu()), run: true }).unwrap();
    let mut failures = 0;
    for _ in 0..3 {
        let p = path.clone();
        // Attach, get ready, then die.
        tokio::task::spawn_blocking(move || drop(attach(&p))).await.unwrap();
        loop {
            match next_event(&mut ev, Duration::from_secs(5)).await {
                WorkerEvent::Ready { .. } => {}
                WorkerEvent::Lost { failure: true, .. } => {
                    failures += 1;
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
    }
    assert_eq!(failures, 3);
    match next_event(&mut ev, Duration::from_secs(2)).await {
        WorkerEvent::HardwareFault { failures } => assert_eq!(failures, 3),
        other => panic!("{other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let view = st.borrow().view.clone();
    assert_eq!(view.state, "faulted");
    assert_eq!(view.failures_10min, 3);
    // Start again (ResetFaults) clears it.
    cmd.send(SupCmd::ResetFaults).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(st.borrow().view.failures_10min, 0);
    assert_eq!(st.borrow().view.state, "waiting_external");
    cmd.send(SupCmd::Shutdown).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn backoff_schedule_matches_the_architecture() {
    use spm::supervisor::{BACKOFF, FAULT_THRESHOLD, FAULT_WINDOW, HEARTBEAT_WATCHDOG};
    assert_eq!(BACKOFF.map(|d| d.as_secs()), [5, 30, 120]);
    assert_eq!(HEARTBEAT_WATCHDOG, Duration::from_secs(5));
    assert_eq!((FAULT_THRESHOLD, FAULT_WINDOW), (3, Duration::from_secs(600)));
}
