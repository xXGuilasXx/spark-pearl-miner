//! The SIGUSR1/SIGUSR2 pause/resume handshake, worker and controller sides.

use std::time::Duration;

use spm_coexist::handshake::{
    Ack, AckState, ControllerHandshake, CtlOutput, CtlSignal, Signal, WorkerHandshake, WorkerState,
    PAUSE_ACK_DEADLINE, RESUME_ACK_DEADLINE, RESUME_RETRIES, TERM_GRACE,
};

fn ms(t: u64) -> Duration {
    Duration::from_millis(t)
}

#[test]
fn pause_waits_for_the_quiescent_point_then_acks() {
    let mut w = WorkerHandshake::new();
    assert!(w.may_issue_gpu_work());
    assert_eq!(w.on_quiescent(), None);
    assert_eq!(w.on_signal(Signal::Usr1), None);
    assert_eq!(w.state(), WorkerState::PausePending);
    assert!(!w.may_issue_gpu_work());
    // A second SIGUSR1 before the chunk ends is coalesced.
    assert_eq!(w.on_signal(Signal::Usr1), None);
    assert_eq!(w.on_quiescent(), Some(Ack { state: AckState::Paused, seq: 1 }));
    assert_eq!(w.state(), WorkerState::Paused);
    assert_eq!(w.on_quiescent(), None);
}

#[test]
fn resume_is_immediate_and_repeats_re_ack() {
    let mut w = WorkerHandshake::new();
    w.on_signal(Signal::Usr1);
    w.on_quiescent();
    // Repeated pause while paused: the ACK is sent again with a new sequence number.
    assert_eq!(w.on_signal(Signal::Usr1), Some(Ack { state: AckState::Paused, seq: 2 }));
    assert_eq!(w.on_signal(Signal::Usr2), Some(Ack { state: AckState::Running, seq: 3 }));
    assert!(w.may_issue_gpu_work());
    assert_eq!(w.on_signal(Signal::Usr2), Some(Ack { state: AckState::Running, seq: 4 }));
}

#[test]
fn resume_before_the_quiescent_point_cancels_the_pause() {
    let mut w = WorkerHandshake::new();
    w.on_signal(Signal::Usr1);
    assert_eq!(w.on_signal(Signal::Usr2), Some(Ack { state: AckState::Running, seq: 1 }));
    assert_eq!(w.on_quiescent(), None);
    assert_eq!(w.state(), WorkerState::Running);
}

#[test]
fn ack_line_round_trip() {
    for ack in [Ack { state: AckState::Paused, seq: 7 }, Ack { state: AckState::Running, seq: 0 }] {
        assert_eq!(Ack::parse(&ack.encode()), Some(ack));
    }
    assert_eq!(Ack::parse("paused 7\n"), Some(Ack { state: AckState::Paused, seq: 7 }));
    for bad in ["", "paused", "stopped 1", "paused x", "paused 1 2"] {
        assert_eq!(Ack::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn controller_and_worker_complete_a_pause_and_a_resume() {
    let mut c = ControllerHandshake::new();
    let mut w = WorkerHandshake::new();
    assert_eq!(c.request_pause(ms(1000)), CtlOutput::Send(CtlSignal::Usr1));
    w.on_signal(Signal::Usr1);
    assert_eq!(c.poll(ms(1005)), None);
    let ack = w.on_quiescent().unwrap(); // chunk ended 8 ms later
    assert_eq!(c.on_ack(ack, ms(1008)), Some(CtlOutput::Paused { latency: ms(8) }));
    assert!(!c.is_pending());

    assert_eq!(c.request_resume(ms(5000)), CtlOutput::Send(CtlSignal::Usr2));
    let ack = w.on_signal(Signal::Usr2).unwrap();
    assert_eq!(c.on_ack(ack, ms(5001)), Some(CtlOutput::Resumed { latency: ms(1) }));
}

#[test]
fn stale_or_mismatched_acks_do_not_complete_a_request() {
    let mut c = ControllerHandshake::new();
    // Seen before the request.
    assert_eq!(c.on_ack(Ack { state: AckState::Paused, seq: 5 }, ms(0)), None);
    c.request_pause(ms(10));
    // The same (old) paused ACK read again: not an answer to this request.
    assert_eq!(c.on_ack(Ack { state: AckState::Paused, seq: 5 }, ms(12)), None);
    // A newer ACK of the wrong state.
    assert_eq!(c.on_ack(Ack { state: AckState::Running, seq: 6 }, ms(13)), None);
    assert!(c.is_pending());
    assert_eq!(
        c.on_ack(Ack { state: AckState::Paused, seq: 7 }, ms(15)),
        Some(CtlOutput::Paused { latency: ms(5) })
    );
}

#[test]
fn unanswered_pause_escalates_to_term_then_kill() {
    let mut c = ControllerHandshake::new();
    c.request_pause(ms(0));
    let deadline = PAUSE_ACK_DEADLINE.as_millis() as u64;
    assert_eq!(c.poll(ms(deadline - 1)), None);
    assert_eq!(c.poll(ms(deadline)), Some(CtlOutput::Send(CtlSignal::Term)));
    assert_eq!(c.poll(ms(deadline + 1)), None);
    let kill_at = deadline + TERM_GRACE.as_millis() as u64;
    assert_eq!(c.poll(ms(kill_at - 1)), None);
    assert_eq!(c.poll(ms(kill_at)), Some(CtlOutput::Send(CtlSignal::Kill)));
    assert_eq!(c.poll(ms(kill_at + 10_000)), None);
    // The exit frees the GPU: the pause goal is met by a release.
    assert_eq!(c.on_worker_exit(), Some(CtlOutput::Released));
    assert!(!c.is_pending());
}

#[test]
fn late_ack_after_term_still_completes() {
    let mut c = ControllerHandshake::new();
    c.request_pause(ms(0));
    assert_eq!(c.poll(ms(150)), Some(CtlOutput::Send(CtlSignal::Term)));
    assert_eq!(
        c.on_ack(Ack { state: AckState::Paused, seq: 1 }, ms(160)),
        Some(CtlOutput::Paused { latency: ms(160) })
    );
    assert_eq!(c.poll(ms(10_000)), None);
}

#[test]
fn unanswered_resume_is_retried_then_reported() {
    let mut c = ControllerHandshake::new();
    c.request_resume(ms(0));
    let step = RESUME_ACK_DEADLINE.as_millis() as u64;
    for i in 1..=u64::from(RESUME_RETRIES) {
        assert_eq!(c.poll(ms(i * step - 1)), None);
        assert_eq!(c.poll(ms(i * step)), Some(CtlOutput::Send(CtlSignal::Usr2)));
    }
    let last = (u64::from(RESUME_RETRIES) + 1) * step;
    assert_eq!(c.poll(ms(last)), Some(CtlOutput::ResumeFailed));
    assert!(!c.is_pending());
}

#[test]
fn last_command_wins_when_signals_coalesce() {
    // The handler keeps only the last signal between two polls of the worker loop.
    let mut w = WorkerHandshake::new();
    let mut c = ControllerHandshake::new();
    c.request_pause(ms(0));
    c.request_resume(ms(1));
    // Only SIGUSR2 is seen by the worker loop.
    let ack = w.on_signal(Signal::Usr2).unwrap();
    assert_eq!(c.on_ack(ack, ms(3)), Some(CtlOutput::Resumed { latency: ms(2) }));
    assert!(w.may_issue_gpu_work());
}

#[test]
fn ack_file_round_trip() {
    use spm_coexist::handshake::{read_ack_file, write_ack_file, ACK_FILE};
    let dir = std::env::temp_dir().join(format!("spm-coexist-ack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(ACK_FILE);
    assert_eq!(read_ack_file(&path).unwrap(), None);
    let mut w = WorkerHandshake::new();
    w.on_signal(Signal::Usr1);
    let ack = w.on_quiescent().unwrap();
    write_ack_file(&path, &ack).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "paused 1\n");
    assert_eq!(read_ack_file(&path).unwrap(), Some(ack));
    let mut c = ControllerHandshake::new();
    c.request_pause(ms(0));
    let read = read_ack_file(&path).unwrap().unwrap();
    assert_eq!(c.on_ack(read, ms(4)), Some(CtlOutput::Paused { latency: ms(4) }));
    std::fs::write(&path, "garbage\n").unwrap();
    assert_eq!(read_ack_file(&path).unwrap(), None);
    // Only the ACK itself is left behind: no temporary files.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}
