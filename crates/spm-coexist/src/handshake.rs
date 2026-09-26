//! Pause/resume handshake between a controller (the daemon, or `spark-modo` tooling) and the GPU
//! worker, over POSIX signals so that a shell script can drive it too.
//!
//! * **SIGUSR1 = pause.** The worker lets the GEMM chunk in flight finish (a *quiescent point*:
//!   nothing queued on its streams; ≤ 10 ms in v0), stops issuing GPU work, keeps its CUDA
//!   context and memory, and acknowledges. A worker with nothing in flight is quiescent at once.
//! * **SIGUSR2 = resume.** Immediate; acknowledged.
//! * **ACK.** One line `"<paused|running> <seq>\n"`, where `seq` grows on every ACK, written
//!   atomically (temporary file + rename) to `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack` and
//!   sent to the daemon over the worker IPC. Repeating a command re-sends the ACK, so a
//!   controller that missed one simply asks again.
//! * Signals carry no payload and coalesce. The worker's handler only records the *last* signal
//!   received; the worker loop feeds it to [`WorkerHandshake`] at its next poll, so the last
//!   command wins.
//! * **Escalation** ([`ControllerHandshake`]): no pause ACK within [`PAUSE_ACK_DEADLINE`] →
//!   SIGTERM (the worker exits at its next quiescent point and the context is freed, i.e. a
//!   Release); still alive [`TERM_GRACE`] later → SIGKILL. A resume that is not acknowledged
//!   within [`RESUME_ACK_DEADLINE`] is re-sent up to [`RESUME_RETRIES`] times, then reported.
//!
//! Both sides are pure state machines; time comes from the caller.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// Name of the ACK file inside `$XDG_RUNTIME_DIR/spark-pearl-miner/`.
pub const ACK_FILE: &str = "worker.ack";
/// A pause must be acknowledged within this long (10× the v0 target of 10 ms).
pub const PAUSE_ACK_DEADLINE: Duration = Duration::from_millis(100);
/// Time between SIGTERM and SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// A resume must be acknowledged within this long ...
pub const RESUME_ACK_DEADLINE: Duration = Duration::from_secs(1);
/// ... or it is re-sent, at most this many times.
pub const RESUME_RETRIES: u32 = 3;

/// A command the worker received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGUSR1: pause.
    Usr1,
    /// SIGUSR2: resume.
    Usr2,
}

/// Worker-side state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerState {
    #[default]
    Running,
    /// SIGUSR1 received; finishing the chunk in flight.
    PausePending,
    /// No GPU work in flight; context kept.
    Paused,
}

/// The state an ACK reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    Running,
    Paused,
}

/// One acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub state: AckState,
    pub seq: u64,
}

impl Ack {
    /// The ACK line.
    pub fn encode(&self) -> String {
        let s = match self.state {
            AckState::Running => "running",
            AckState::Paused => "paused",
        };
        format!("{s} {}\n", self.seq)
    }

    /// Parses an ACK line.
    pub fn parse(line: &str) -> Option<Ack> {
        let mut it = line.split_whitespace();
        let state = match it.next()? {
            "running" => AckState::Running,
            "paused" => AckState::Paused,
            _ => return None,
        };
        let seq = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Ack { state, seq })
    }
}

/// Writes an ACK atomically (temporary file in the same directory, then rename), so a reader
/// never sees half a line. The worker calls this after every acknowledged command.
pub fn write_ack_file(path: &Path, ack: &Ack) -> io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(ACK_FILE);
    let tmp = path.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    fs::write(&tmp, ack.encode())?;
    fs::rename(&tmp, path)
}

/// Reads the ACK file. `Ok(None)` when it does not exist or does not hold a valid ACK line.
pub fn read_ack_file(path: &Path) -> io::Result<Option<Ack>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().next().and_then(Ack::parse)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The worker side.
#[derive(Debug, Clone, Default)]
pub struct WorkerHandshake {
    state: WorkerState,
    seq: u64,
}

impl WorkerHandshake {
    /// A running worker that has sent no ACK yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    pub fn state(&self) -> WorkerState {
        self.state
    }

    /// Whether the worker may start another GPU chunk.
    pub fn may_issue_gpu_work(&self) -> bool {
        self.state == WorkerState::Running
    }

    /// A signal arrived. Returns the ACK to publish, if any.
    pub fn on_signal(&mut self, sig: Signal) -> Option<Ack> {
        match (self.state, sig) {
            (WorkerState::Running, Signal::Usr1) => {
                self.state = WorkerState::PausePending;
                None
            }
            (WorkerState::PausePending, Signal::Usr1) => None,
            (WorkerState::Paused, Signal::Usr1) => Some(self.ack(AckState::Paused)),
            (_, Signal::Usr2) => {
                self.state = WorkerState::Running;
                Some(self.ack(AckState::Running))
            }
        }
    }

    /// The worker reached a quiescent point (chunk boundary, or nothing in flight).
    pub fn on_quiescent(&mut self) -> Option<Ack> {
        if self.state == WorkerState::PausePending {
            self.state = WorkerState::Paused;
            Some(self.ack(AckState::Paused))
        } else {
            None
        }
    }

    fn ack(&mut self, state: AckState) -> Ack {
        self.seq += 1;
        Ack { state, seq: self.seq }
    }
}

/// A signal the controller must send to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtlSignal {
    Usr1,
    Usr2,
    Term,
    Kill,
}

/// What the controller should do or report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtlOutput {
    /// Send this signal.
    Send(CtlSignal),
    /// The pause was acknowledged after `latency`.
    Paused { latency: Duration },
    /// The resume was acknowledged after `latency`.
    Resumed { latency: Duration },
    /// The worker exited while a pause was pending: the GPU is free (released).
    Released,
    /// The resume was never acknowledged.
    ResumeFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Signalled,
    Terminating,
    Killed,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    want: AckState,
    requested_at: Duration,
    after_seq: u64,
    stage: Stage,
    stage_since: Duration,
    retries: u32,
}

/// The controller side.
#[derive(Debug, Clone, Default)]
pub struct ControllerHandshake {
    last_seq: u64,
    pending: Option<Pending>,
}

impl ControllerHandshake {
    /// A controller that has seen no ACK yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a request is waiting for its ACK.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Asks the worker to pause.
    pub fn request_pause(&mut self, now: Duration) -> CtlOutput {
        self.start(AckState::Paused, now);
        CtlOutput::Send(CtlSignal::Usr1)
    }

    /// Asks the worker to resume.
    pub fn request_resume(&mut self, now: Duration) -> CtlOutput {
        self.start(AckState::Running, now);
        CtlOutput::Send(CtlSignal::Usr2)
    }

    fn start(&mut self, want: AckState, now: Duration) {
        self.pending = Some(Pending {
            want,
            requested_at: now,
            after_seq: self.last_seq,
            stage: Stage::Signalled,
            stage_since: now,
            retries: 0,
        });
    }

    /// An ACK was read. Returns the completion it causes, if any. Stale ACKs (sequence not newer
    /// than the request) and ACKs of the other state are ignored.
    pub fn on_ack(&mut self, ack: Ack, now: Duration) -> Option<CtlOutput> {
        self.last_seq = self.last_seq.max(ack.seq);
        let p = self.pending?;
        if ack.seq <= p.after_seq || ack.state != p.want {
            return None;
        }
        self.pending = None;
        let latency = now.saturating_sub(p.requested_at);
        Some(match ack.state {
            AckState::Paused => CtlOutput::Paused { latency },
            AckState::Running => CtlOutput::Resumed { latency },
        })
    }

    /// The worker process exited.
    pub fn on_worker_exit(&mut self) -> Option<CtlOutput> {
        let p = self.pending.take()?;
        Some(match p.want {
            AckState::Paused => CtlOutput::Released,
            AckState::Running => CtlOutput::ResumeFailed,
        })
    }

    /// Timer tick: escalation and retries.
    pub fn poll(&mut self, now: Duration) -> Option<CtlOutput> {
        let p = self.pending.as_mut()?;
        let waited = now.saturating_sub(p.stage_since);
        match (p.want, p.stage) {
            (AckState::Paused, Stage::Signalled) if waited >= PAUSE_ACK_DEADLINE => {
                p.stage = Stage::Terminating;
                p.stage_since = now;
                Some(CtlOutput::Send(CtlSignal::Term))
            }
            (AckState::Paused, Stage::Terminating) if waited >= TERM_GRACE => {
                p.stage = Stage::Killed;
                p.stage_since = now;
                Some(CtlOutput::Send(CtlSignal::Kill))
            }
            (AckState::Running, _) if waited >= RESUME_ACK_DEADLINE => {
                if p.retries < RESUME_RETRIES {
                    p.retries += 1;
                    p.stage_since = now;
                    Some(CtlOutput::Send(CtlSignal::Usr2))
                } else {
                    self.pending = None;
                    Some(CtlOutput::ResumeFailed)
                }
            }
            _ => None,
        }
    }
}
