//! WorkerSupervisor: owns `worker.sock` and the GPU worker process.
//!
//! * The daemon listens on `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` (0600); the worker
//!   attaches to it. Launch mode `spawn`: the supervisor starts
//!   `spark-pearl-miner gpu-worker --attach <sock>` itself. Launch mode `external`: something else
//!   starts it (on a DGX Spark with spark-modo, the `spark-miner.service` system unit under the
//!   exclusive GPU lease) and the supervisor only waits for it.
//! * The worker sends a heartbeat every 500 ms; 5 s without any frame is a failure (the process is
//!   killed in spawn mode, the connection dropped in external mode).
//! * Failures back off 5 s, 30 s, then 2 min. Three failures within 10 min raise the
//!   "possible hardware fault" alert and stop respawning until the user starts mining again.
//! * `Release` makes the worker exit, freeing its CUDA context; a worker kept paused for a minute
//!   is released too, unless the pause keeps the context on purpose (a coexistence yield or a
//!   power-governor trip).
//! * Pause/resume handshake (`spm_coexist::handshake`): every `Pause`/`Resume` frame starts a
//!   request that the worker acknowledges in `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack`.
//!   A pause without an ACK within 100 ms is escalated to a `Release` (the worker exits at its
//!   next quiescent point), then to a kill 3 s later; a resume is re-sent up to 3 times.
//! * `SetDuty` (power governor) is remembered and re-sent to every new worker before it resumes.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use spm_api::config::LaunchMode;
use spm_api::WorkerView;
use spm_coexist::handshake::{read_ack_file, ControllerHandshake, CtlOutput, CtlSignal, ACK_FILE};
use spm_ipc::{read_frame_async, write_frame_async, FaultKind, IpcError, ToDaemon, ToWorker, WorkUnit, IPC_VERSION};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixListener;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

pub const HEARTBEAT_WATCHDOG: Duration = Duration::from_secs(5);
pub const BACKOFF: [Duration; 3] = [Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(120)];
pub const FAULT_WINDOW: Duration = Duration::from_secs(600);
pub const FAULT_THRESHOLD: usize = 3;
/// A worker healthy this long clears the consecutive-failure count.
pub const HEALTHY_RESET: Duration = Duration::from_secs(120);
/// A paused worker is released (process exits, context freed) after this long.
pub const IDLE_RELEASE: Duration = Duration::from_secs(60);
/// A spawned worker must attach within this long.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the ACK file is read while a pause/resume request is pending.
pub const ACK_POLL: Duration = Duration::from_millis(10);

/// Commands from the daemon.
#[derive(Debug)]
pub enum SupCmd {
    /// What the worker should do: mine `job` (if any) when `run`, else stay paused.
    /// `keep_context`: the pause is a hold (coexistence yield, power trip) that keeps the worker
    /// and its CUDA context; the one-minute idle release does not apply.
    Desire { job: Option<Box<WorkUnit>>, run: bool, keep_context: bool },
    /// Duty cycle from the power governor, 1–100 %.
    SetDuty { pct: u8 },
    /// Make the worker exit now (Stop): frees the CUDA context.
    Release,
    Configure { launch: LaunchMode, simulate: bool, sim_interval_ms: u64 },
    /// The user started mining again after a hardware-fault stop.
    ResetFaults,
    Shutdown,
}

/// Events for the daemon.
#[derive(Debug)]
pub enum WorkerEvent {
    Ready { device: String },
    Stats { credited_macs: u64, attempts: u64, sm_clock_mhz: u32, power_w: f32 },
    Proof { wu_id: u64, session_id: u64, job_id: String, proof: Vec<u8> },
    Fault { kind: FaultKind, msg: String },
    Lost { reason: String, failure: bool },
    /// Three failures within ten minutes: the daemon stops mining and alerts.
    HardwareFault { failures: usize },
}

/// What the pause/resume handshake has seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandshakeInfo {
    /// Last ACK line read (`paused 3`).
    pub last_ack: Option<String>,
    pub acks: u64,
    pub pause_latency_ms: Option<u64>,
    pub resume_latency_ms: Option<u64>,
    /// Pauses escalated to a release or a kill for want of an ACK.
    pub escalations: u32,
}

/// Snapshot for status views and fee accounting.
#[derive(Debug, Clone, Default)]
pub struct WorkerStatus {
    pub view: WorkerView,
    /// Ready, resumed, with a job, and heard from recently.
    pub hashing: bool,
    /// `wu_id` of the job the worker was last given.
    pub job_wu: Option<u64>,
    /// A worker process exists (attached, or spawned and starting): its memory is allocated.
    pub present: bool,
    /// Process ids of our worker (spawned child and/or the peer of `worker.sock`), excluded from
    /// the "other GPU users" of the coexistence fallback.
    pub pids: Vec<u32>,
    pub handshake: HandshakeInfo,
}

struct Conn {
    id: u64,
    w: OwnedWriteHalf,
    last_frame: Instant,
    ready: bool,
    ready_since: Option<Instant>,
    sent_wu: Option<u64>,
    sent_run: Option<bool>,
    sent_duty: Option<u8>,
    releasing: bool,
    /// Process id of the peer (`SO_PEERCRED`).
    pid: Option<u32>,
}

pub struct Supervisor {
    sock: PathBuf,
    ack_path: PathBuf,
    exe: PathBuf,
    launch: LaunchMode,
    simulate: bool,
    sim_interval_ms: u64,
    desired_job: Option<Box<WorkUnit>>,
    desired_run: bool,
    desired_duty: Option<u8>,
    keep_context: bool,
    paused_since: Option<Instant>,
    conn: Option<Conn>,
    next_conn: u64,
    child: Option<Child>,
    child_started: Option<Instant>,
    failures: VecDeque<Instant>,
    consecutive: u32,
    backoff_until: Option<Instant>,
    faulted: bool,
    last_fault: Option<String>,
    device: Option<String>,
    hs: ControllerHandshake,
    hs_origin: Instant,
    hs_info: HandshakeInfo,
    events: mpsc::UnboundedSender<WorkerEvent>,
    status: watch::Sender<WorkerStatus>,
}

enum Frame {
    Msg(u64, ToDaemon),
    Closed(u64, String),
}

impl Supervisor {
    /// Bind `worker.sock` (0600) and run until `Shutdown`. `exe` is the binary to spawn.
    pub async fn start(
        sock: PathBuf,
        exe: PathBuf,
        launch: LaunchMode,
        simulate: bool,
        sim_interval_ms: u64,
    ) -> std::io::Result<(mpsc::UnboundedSender<SupCmd>, mpsc::UnboundedReceiver<WorkerEvent>, watch::Receiver<WorkerStatus>)> {
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
        let ack_path = sock.with_file_name(ACK_FILE);
        let _ = std::fs::remove_file(&ack_path);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (st_tx, st_rx) = watch::channel(WorkerStatus::default());
        let sup = Supervisor {
            sock,
            ack_path,
            exe,
            launch,
            simulate,
            sim_interval_ms,
            desired_job: None,
            desired_run: false,
            desired_duty: None,
            keep_context: false,
            paused_since: None,
            conn: None,
            next_conn: 1,
            child: None,
            child_started: None,
            failures: VecDeque::new(),
            consecutive: 0,
            backoff_until: None,
            faulted: false,
            last_fault: None,
            device: None,
            hs: ControllerHandshake::new(),
            hs_origin: Instant::now(),
            hs_info: HandshakeInfo::default(),
            events: ev_tx,
            status: st_tx,
        };
        tokio::spawn(sup.run(listener, cmd_rx));
        Ok((cmd_tx, ev_rx, st_rx))
    }

    /// Spawn mode is always available now that the real GPU worker exists (`spark-pearl-miner
    /// gpu-worker`); `simulate` only selects the CPU simulation instead of the GPU.
    fn unavailable(&self) -> bool {
        false
    }

    fn publish(&self) {
        let now = Instant::now();
        let state = if self.faulted {
            "faulted"
        } else if let Some(c) = &self.conn {
            if !c.ready {
                "starting"
            } else if c.sent_run == Some(true) && c.sent_wu.is_some() {
                "hashing"
            } else {
                "paused"
            }
        } else if self.child.is_some() {
            "starting"
        } else if self.unavailable() && self.desired_run {
            "unavailable"
        } else if self.backoff_until.is_some_and(|t| t > now) {
            "backoff"
        } else if self.launch == LaunchMode::External && self.desired_run {
            "waiting_external"
        } else {
            "absent"
        };
        let hashing = state == "hashing" && self.conn.as_ref().is_some_and(|c| now.duration_since(c.last_frame) < HEARTBEAT_WATCHDOG);
        let failures_10min = self.failures.iter().filter(|t| now.duration_since(**t) < FAULT_WINDOW).count() as u32;
        let child_pid = self.child.as_ref().and_then(Child::id);
        let view = WorkerView {
            state: state.to_string(),
            device: self.device.clone(),
            simulated: self.simulate,
            launch: match self.launch {
                LaunchMode::Spawn => "spawn".into(),
                LaunchMode::External => "external".into(),
            },
            pid: child_pid.or_else(|| self.conn.as_ref().and_then(|c| c.pid)),
            failures_10min,
            last_fault: self.last_fault.clone(),
            next_retry_s: self.backoff_until.filter(|t| *t > now).map(|t| t.duration_since(now).as_secs()),
        };
        let job_wu = self.conn.as_ref().and_then(|c| c.sent_wu);
        let present = self.conn.is_some() || self.child.is_some();
        let mut pids: Vec<u32> = child_pid.into_iter().chain(self.conn.as_ref().and_then(|c| c.pid)).collect();
        pids.dedup();
        let handshake = self.hs_info.clone();
        self.status.send_if_modified(|s| {
            let changed = s.view.state != view.state
                || s.hashing != hashing
                || s.job_wu != job_wu
                || s.view.failures_10min != view.failures_10min
                || s.view.pid != view.pid
                || s.view.device != view.device
                || s.view.last_fault != view.last_fault
                || s.view.next_retry_s != view.next_retry_s
                || s.view.launch != view.launch
                || s.view.simulated != view.simulated
                || s.present != present
                || s.pids != pids
                || s.handshake != handshake;
            *s = WorkerStatus { view: view.clone(), hashing, job_wu, present, pids: pids.clone(), handshake: handshake.clone() };
            changed
        });
    }

    async fn run(mut self, listener: UnixListener, mut cmd_rx: mpsc::UnboundedReceiver<SupCmd>) {
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Frame>();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ack_tick = tokio::time::interval(ACK_POLL);
        ack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let pending = self.hs.is_pending();
            tokio::select! {
                c = cmd_rx.recv() => match c {
                    Some(SupCmd::Shutdown) | None => break,
                    Some(c) => self.on_cmd(c).await,
                },
                a = listener.accept() => if let Ok((stream, _)) = a {
                    self.on_accept(stream, &frame_tx).await;
                },
                Some(f) = frame_rx.recv() => self.on_frame(f).await,
                _ = tick.tick() => self.on_tick().await,
                _ = ack_tick.tick(), if pending => self.on_ack_tick().await,
            }
            self.publish();
        }
        self.shutdown().await;
    }

    async fn on_cmd(&mut self, c: SupCmd) {
        match c {
            SupCmd::Desire { job, run, keep_context } => {
                if run {
                    self.paused_since = None;
                } else if self.desired_run || self.paused_since.is_none() {
                    self.paused_since = Some(Instant::now());
                }
                self.desired_job = job;
                self.desired_run = run;
                self.keep_context = !run && keep_context;
                self.push_desired().await;
            }
            SupCmd::SetDuty { pct } => {
                self.desired_duty = Some(pct.clamp(1, 100));
                self.push_desired().await;
            }
            SupCmd::Release => self.release().await,
            SupCmd::Configure { launch, simulate, sim_interval_ms } => {
                let changed = launch != self.launch || simulate != self.simulate || sim_interval_ms != self.sim_interval_ms;
                self.launch = launch;
                self.simulate = simulate;
                self.sim_interval_ms = sim_interval_ms;
                if changed && self.child.is_some() {
                    // Restart the spawned worker with the new settings.
                    self.release().await;
                }
            }
            SupCmd::ResetFaults => {
                self.faulted = false;
                self.failures.clear();
                self.consecutive = 0;
                self.backoff_until = None;
            }
            SupCmd::Shutdown => {}
        }
    }

    async fn on_accept(&mut self, stream: tokio::net::UnixStream, frame_tx: &mpsc::UnboundedSender<Frame>) {
        // Only processes of this user can reach the 0600 socket in a 0700 directory; check anyway.
        let cred = stream.peer_cred().ok();
        if let (Some(cred), Ok(meta)) = (cred, std::fs::metadata(&self.sock)) {
            use std::os::unix::fs::MetadataExt;
            if cred.uid() != meta.uid() {
                tracing::warn!(peer_uid = cred.uid(), "worker socket: refused a process of another user");
                return;
            }
        }
        if self.conn.is_some() {
            tracing::warn!("worker socket: a second worker tried to attach; refused");
            return;
        }
        let id = self.next_conn;
        self.next_conn += 1;
        let (mut r, mut w) = stream.into_split();
        // A new worker numbers its ACKs from 1 again: forget the previous one's.
        self.hs = ControllerHandshake::new();
        let _ = std::fs::remove_file(&self.ack_path);
        if let Err(e) = write_frame_async(&mut w, &ToWorker::Hello { version: IPC_VERSION }).await {
            tracing::warn!(error = %e, "worker socket: hello failed");
            return;
        }
        let tx = frame_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_frame_async::<_, ToDaemon>(&mut r).await {
                    Ok(m) => {
                        if tx.send(Frame::Msg(id, m)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let reason = match e {
                            IpcError::Closed => "worker closed the connection".to_string(),
                            other => other.to_string(),
                        };
                        let _ = tx.send(Frame::Closed(id, reason));
                        return;
                    }
                }
            }
        });
        let pid = cred.and_then(|c| c.pid()).and_then(|p| u32::try_from(p).ok()).filter(|p| *p > 0);
        tracing::info!(pid, "GPU worker attached");
        self.conn = Some(Conn {
            id,
            w,
            last_frame: Instant::now(),
            ready: false,
            ready_since: None,
            sent_wu: None,
            sent_run: None,
            sent_duty: None,
            releasing: false,
            pid,
        });
    }

    async fn on_frame(&mut self, f: Frame) {
        match f {
            Frame::Msg(id, m) => {
                let Some(c) = self.conn.as_mut().filter(|c| c.id == id) else { return };
                c.last_frame = Instant::now();
                match m {
                    ToDaemon::Ready { kat_ok, device } => {
                        if !kat_ok {
                            self.last_fault = Some("known-answer test failed".into());
                            self.fail("the worker's known-answer test failed").await;
                            return;
                        }
                        c.ready = true;
                        c.ready_since = Some(Instant::now());
                        self.device = Some(device.clone());
                        tracing::info!(%device, "GPU worker ready");
                        let _ = self.events.send(WorkerEvent::Ready { device });
                        self.push_desired().await;
                    }
                    ToDaemon::Heartbeat { .. } => {
                        if c.ready_since.is_some_and(|t| t.elapsed() >= HEALTHY_RESET) {
                            self.consecutive = 0;
                        }
                    }
                    ToDaemon::Stats { credited_macs, attempts, sm_clock_mhz, power_w, .. } => {
                        let _ = self.events.send(WorkerEvent::Stats { credited_macs, attempts, sm_clock_mhz, power_w });
                    }
                    ToDaemon::Proof { wu_id, session_id, job_id, proof_bincode, .. } => {
                        let _ = self.events.send(WorkerEvent::Proof { wu_id, session_id, job_id, proof: proof_bincode });
                    }
                    ToDaemon::Fault { kind, msg } => {
                        tracing::warn!(?kind, %msg, "GPU worker fault");
                        self.last_fault = Some(format!("{kind:?}: {msg}"));
                        let _ = self.events.send(WorkerEvent::Fault { kind, msg: msg.clone() });
                        if matches!(kind, FaultKind::KatFailed | FaultKind::CanaryMismatch | FaultKind::Cuda | FaultKind::OutOfMemory) {
                            self.fail(&format!("worker fault {kind:?}: {msg}")).await;
                        }
                    }
                }
            }
            Frame::Closed(id, reason) => {
                let Some(c) = self.conn.as_ref().filter(|c| c.id == id) else { return };
                let expected = c.releasing;
                self.conn = None;
                self.on_worker_gone();
                if expected {
                    tracing::info!("GPU worker released");
                    let _ = self.events.send(WorkerEvent::Lost { reason, failure: false });
                } else {
                    self.fail(&format!("worker disconnected: {reason}")).await;
                }
            }
        }
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        // Child exit.
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.child = None;
                self.child_started = None;
                let expected = self.conn.as_ref().is_none_or(|c| c.releasing) && status.success();
                if self.conn.is_none() {
                    self.on_worker_gone();
                }
                if !expected && self.conn.is_none() {
                    self.fail(&format!("worker process exited ({status})")).await;
                }
            }
        }
        // Watchdog.
        if let Some(c) = &self.conn {
            if now.duration_since(c.last_frame) >= HEARTBEAT_WATCHDOG {
                self.fail("no heartbeat for 5 s").await;
            }
        } else if let (Some(_), Some(t0)) = (&self.child, self.child_started) {
            if now.duration_since(t0) >= ATTACH_TIMEOUT {
                self.fail("the spawned worker never attached").await;
            }
        }
        // Idle release: a paused worker gives the GPU back after a minute (not during a hold that
        // keeps the context on purpose).
        if !self.keep_context
            && self.paused_since.is_some_and(|t| now.duration_since(t) >= IDLE_RELEASE)
            && self.conn.as_ref().is_some_and(|c| !c.releasing)
        {
            tracing::info!("GPU idle for a minute: releasing the worker");
            self.release().await;
        }
        // Spawn when needed.
        let want = self.desired_run && self.desired_job.is_some();
        let can = self.launch == LaunchMode::Spawn
            && !self.faulted
            && self.conn.is_none()
            && self.child.is_none()
            && self.backoff_until.is_none_or(|t| now >= t);
        if want && can {
            self.spawn();
        }
    }

    fn hs_now(&self) -> Duration {
        self.hs_origin.elapsed()
    }

    /// The worker is gone (connection closed or process exited): settle a pending request.
    fn on_worker_gone(&mut self) {
        match self.hs.on_worker_exit() {
            Some(CtlOutput::Released) => tracing::info!("the worker exited while a pause was pending: the GPU is free"),
            Some(CtlOutput::ResumeFailed) => tracing::info!("the worker exited before acknowledging a resume"),
            _ => {}
        }
    }

    /// Reads the ACK file and runs the handshake timers while a request is pending.
    async fn on_ack_tick(&mut self) {
        let now = self.hs_now();
        match read_ack_file(&self.ack_path) {
            Ok(Some(ack)) => {
                if let Some(out) = self.hs.on_ack(ack, now) {
                    self.hs_info.last_ack = Some(ack.encode().trim_end().to_string());
                    self.hs_info.acks += 1;
                    self.on_hs_output(out).await;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, "worker.ack unreadable"),
        }
        if let Some(out) = self.hs.poll(now) {
            self.on_hs_output(out).await;
        }
    }

    async fn on_hs_output(&mut self, out: CtlOutput) {
        match out {
            CtlOutput::Paused { latency } => {
                tracing::debug!(latency_ms = latency.as_millis() as u64, "worker acknowledged the pause");
                self.hs_info.pause_latency_ms = Some(latency.as_millis() as u64);
            }
            CtlOutput::Resumed { latency } => {
                tracing::debug!(latency_ms = latency.as_millis() as u64, "worker acknowledged the resume");
                self.hs_info.resume_latency_ms = Some(latency.as_millis() as u64);
            }
            CtlOutput::Send(CtlSignal::Usr1) => {
                if let Some(c) = self.conn.as_mut() {
                    let _ = write_frame_async(&mut c.w, &ToWorker::Pause).await;
                }
            }
            CtlOutput::Send(CtlSignal::Usr2) => {
                tracing::info!("resume not acknowledged within 1 s: sending it again");
                if let Some(c) = self.conn.as_mut() {
                    let _ = write_frame_async(&mut c.w, &ToWorker::Resume).await;
                }
            }
            CtlOutput::Send(CtlSignal::Term) => {
                tracing::warn!("the worker did not acknowledge the pause within 100 ms: releasing it");
                self.hs_info.escalations += 1;
                if let Some(c) = self.conn.as_mut() {
                    if !c.releasing {
                        c.releasing = true;
                        let _ = write_frame_async(&mut c.w, &ToWorker::Release).await;
                    }
                }
            }
            CtlOutput::Send(CtlSignal::Kill) => {
                // Killed in spawn mode; an external worker's connection is dropped and its
                // launcher restarts it. Either way it counts as a worker failure.
                self.hs_info.escalations += 1;
                self.fail("the worker acknowledged neither the pause nor the release within 3 s").await;
            }
            CtlOutput::Released => tracing::info!("the worker exited while a pause was pending"),
            CtlOutput::ResumeFailed => tracing::warn!("the worker never acknowledged the resume"),
        }
    }

    fn spawn(&mut self) {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("gpu-worker").arg("--attach").arg(&self.sock);
        if self.simulate {
            cmd.arg("--sim").arg("--sim-interval-ms").arg(self.sim_interval_ms.to_string());
        }
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        match cmd.spawn() {
            Ok(child) => {
                tracing::info!(pid = child.id(), "spawned the GPU worker");
                self.child = Some(child);
                self.child_started = Some(Instant::now());
            }
            Err(e) => {
                tracing::warn!(error = %e, exe = %self.exe.display(), "could not spawn the GPU worker");
                self.last_fault = Some(format!("spawn failed: {e}"));
                self.backoff_until = Some(Instant::now() + BACKOFF[2]);
            }
        }
    }

    async fn push_desired(&mut self) {
        let now = self.hs_now();
        let Some(c) = self.conn.as_mut() else { return };
        if !c.ready || c.releasing {
            return;
        }
        // The duty goes first, so a resumed worker starts at the governor's duty.
        if let Some(pct) = self.desired_duty {
            if c.sent_duty != Some(pct) {
                if write_frame_async(&mut c.w, &ToWorker::SetDuty { pct }).await.is_err() {
                    return;
                }
                c.sent_duty = Some(pct);
            }
        }
        let wu_id = self.desired_job.as_ref().map(|j| j.wu_id);
        if let Some(job) = &self.desired_job {
            if c.sent_wu != wu_id {
                if write_frame_async(&mut c.w, &ToWorker::SetJob { wu: job.clone() }).await.is_err() {
                    return;
                }
                c.sent_wu = wu_id;
            }
        } else if c.sent_wu.is_some() && c.sent_run != Some(false) {
            // No job any more: pause (the worker keeps the stale job but does not hash it).
            if write_frame_async(&mut c.w, &ToWorker::Pause).await.is_err() {
                return;
            }
            c.sent_run = Some(false);
            self.hs.request_pause(now);
        }
        let run = self.desired_run && self.desired_job.is_some();
        if c.sent_run != Some(run) {
            let msg = if run { ToWorker::Resume } else { ToWorker::Pause };
            if write_frame_async(&mut c.w, &msg).await.is_err() {
                return;
            }
            c.sent_run = Some(run);
            if run {
                self.hs.request_resume(now);
            } else {
                self.hs.request_pause(now);
            }
        }
    }

    async fn release(&mut self) {
        if let Some(c) = self.conn.as_mut() {
            if !c.releasing {
                c.releasing = true;
                let _ = write_frame_async(&mut c.w, &ToWorker::Release).await;
            }
        }
        self.paused_since = None;
        if let Some(mut child) = self.child.take() {
            // Give it a moment to exit on its own, then make sure.
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    let _ = child.kill().await;
                }
            }
            self.child_started = None;
        }
    }

    async fn fail(&mut self, why: &str) {
        let now = Instant::now();
        tracing::warn!(reason = why, "GPU worker failure");
        if let Some(mut c) = self.conn.take() {
            let _ = write_frame_async(&mut c.w, &ToWorker::Shutdown).await;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
        self.on_worker_gone();
        self.child_started = None;
        self.failures.push_back(now);
        while self.failures.front().is_some_and(|t| now.duration_since(*t) >= FAULT_WINDOW) {
            self.failures.pop_front();
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.launch == LaunchMode::Spawn {
            // External workers are restarted by whoever launches them (spark-modo/systemd).
            let step = (self.consecutive as usize).saturating_sub(1).min(BACKOFF.len() - 1);
            self.backoff_until = Some(now + BACKOFF[step]);
        }
        self.last_fault = Some(why.to_string());
        let _ = self.events.send(WorkerEvent::Lost { reason: why.to_string(), failure: true });
        if self.failures.len() >= FAULT_THRESHOLD && !self.faulted {
            self.faulted = true;
            let _ = self.events.send(WorkerEvent::HardwareFault { failures: self.failures.len() });
        }
    }

    async fn shutdown(&mut self) {
        if let Some(mut c) = self.conn.take() {
            let _ = write_frame_async(&mut c.w, &ToWorker::Shutdown).await;
        }
        if let Some(mut child) = self.child.take() {
            if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_file(&self.ack_path);
    }
}
