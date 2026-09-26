//! `PoolSession`: one connection to one pool, kept deliberately dumb and observable.
//!
//! It connects with the configured `TlsMode`, sends the dialect handshake, parses jobs and
//! replies, tracks the current `job_id`, and submits only for the current job. It never
//! reconnects and never fails over: every state change is reported as a [`SessionEvent`] and
//! the `spm-pool` reducer decides what to do. When the connection ends the task ends; the
//! owner creates a new session to reconnect.
use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::codec::{self, CodecError, NdjsonReader};
use crate::encode::{self, ProofEncoding, ProofField, ProofFormatLearner};
use crate::tls::{ConnectError, Connector, PoolStream, TlsMode, Transport};
use crate::{authorize_lines, classify_reject, parse_notify_value, reply_of, reply_type, submit_line};
use crate::{Dialect, FrameOpts, Job, RejectKind, Reply, AGENT};

pub const DEFAULT_SUBMIT_ACK_TIMEOUT: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(100);
/// Events buffered before the session applies back-pressure to the socket.
const EVENT_BUFFER: usize = 256;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub dialect: Dialect,
    pub wallet: String,
    pub worker: String,
    /// Kryptex password (`x`, or `d=<N>` for a static difficulty).
    pub password: String,
    /// `None` ⇒ the dialect default ([`Dialect::default_jsonrpc`]). LuckyPool needs `Some(true)`.
    pub jsonrpc: Option<bool>,
    pub agent: String,
    /// Initial proof field; the session learns the working one (see [`ProofFormatLearner`]).
    pub proof_field: ProofField,
    pub submit_ack_timeout: Duration,
}

impl SessionConfig {
    pub fn new(host: impl Into<String>, port: u16, dialect: Dialect, wallet: impl Into<String>, worker: impl Into<String>) -> Self {
        SessionConfig {
            host: host.into(),
            port,
            tls: TlsMode::Auto,
            dialect,
            wallet: wallet.into(),
            worker: worker.into(),
            password: "x".into(),
            jsonrpc: None,
            agent: AGENT.into(),
            proof_field: ProofField::PlainProof,
            submit_ack_timeout: DEFAULT_SUBMIT_ACK_TIMEOUT,
        }
    }

    fn frame_opts(&self) -> FrameOpts<'_> {
        FrameOpts { jsonrpc: self.jsonrpc.unwrap_or(self.dialect.default_jsonrpc()), agent: &self.agent }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// DNS/TCP failure or timeout.
    ConnectFailed(String),
    /// Certificate rejected or pin mismatch (never downgraded to plain).
    Certificate(String),
    /// TLS handshake failed for a non-certificate reason (`On`/`Pinned`).
    TlsProtocol(String),
    AuthRejected(String),
    /// The pool closed the connection.
    Eof,
    Io(String),
    /// The pool sent a line over the 4 MiB cap.
    LineTooLong,
    /// The pool sent something that is not JSON.
    Protocol(String),
    /// Closed by the owner (shutdown or handle dropped).
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connected { transport: Transport, plain_fallback: bool },
    TlsOk { spki_sha256_b64: Option<String> },
    Authorized { proof_type: Option<String> },
    AuthRejected { reason: String },
    JobReceived(Job),
    ShareAccepted { submit_id: u64, job_id: String },
    ShareRejected { submit_id: u64, job_id: String, kind: RejectKind, reason: String },
    SubmitAckTimeout { submit_id: u64, job_id: String },
    /// The learner switched fields after repeated format rejects; persist it for this pool.
    ProofFieldSwitched { field: ProofField },
    Disconnected { reason: DisconnectReason },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("stale: job {job_id} is not the current job ({current:?})")]
    Stale { job_id: String, current: Option<String> },
    #[error("not authorized yet")]
    NotAuthorized,
    #[error("session closed")]
    Closed,
    #[error("encode: {0}")]
    Encode(String),
}

enum Command {
    Submit { job_id: String, proof: Vec<u8>, reply: oneshot::Sender<Result<u64, SubmitError>> },
    Shutdown,
}

/// Handle to a running session. Dropping it closes the connection.
#[derive(Debug)]
pub struct PoolSession {
    cmd: mpsc::Sender<Command>,
    job: watch::Receiver<Option<Job>>,
    task: JoinHandle<()>,
}

impl PoolSession {
    /// Start connecting in the background. Events arrive on the returned receiver, ending
    /// with exactly one `Disconnected`.
    pub fn spawn(cfg: SessionConfig, connector: Connector) -> (PoolSession, mpsc::Receiver<SessionEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (ev_tx, ev_rx) = mpsc::channel(EVENT_BUFFER);
        let (job_tx, job_rx) = watch::channel(None);
        let task = tokio::spawn(async move {
            let reason = run(cfg, connector, cmd_rx, &ev_tx, &job_tx).await;
            let _ = ev_tx.send(SessionEvent::Disconnected { reason }).await;
        });
        (PoolSession { cmd: cmd_tx, job: job_rx, task }, ev_rx)
    }

    pub fn current_job(&self) -> Option<Job> {
        self.job.borrow().clone()
    }

    pub fn current_job_id(&self) -> Option<String> {
        self.job.borrow().as_ref().map(|j| j.job_id.clone())
    }

    /// Watch the current job (updated before the matching `JobReceived` event is sent).
    pub fn subscribe_jobs(&self) -> watch::Receiver<Option<Job>> {
        self.job.clone()
    }

    /// Submit `proof` (bincode of a verified PlainProof) for `job_id`. Refused without touching
    /// the wire unless `job_id` is the session's current job at the moment of sending.
    /// Returns the request id that later `ShareAccepted`/`ShareRejected`/`SubmitAckTimeout` carry.
    pub async fn submit(&self, job_id: &str, proof: Vec<u8>) -> Result<u64, SubmitError> {
        let current = self.current_job_id();
        if current.as_deref() != Some(job_id) {
            return Err(SubmitError::Stale { job_id: job_id.to_string(), current });
        }
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Submit { job_id: job_id.to_string(), proof, reply: tx })
            .await
            .map_err(|_| SubmitError::Closed)?;
        rx.await.map_err(|_| SubmitError::Closed)?
    }

    /// Close the connection and wait for the task to finish.
    pub async fn shutdown(self) {
        let _ = self.cmd.send(Command::Shutdown).await;
        let _ = self.task.await;
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

struct Pending {
    job_id: String,
    deadline: Instant,
}

struct State<'a> {
    cfg: &'a SessionConfig,
    ev: &'a mpsc::Sender<SessionEvent>,
    job: &'a watch::Sender<Option<Job>>,
    auth_id: u64,
    next_id: u64,
    authorized: bool,
    learner: ProofFormatLearner,
    /// Encoding of the `plain_proof` field, possibly changed by the authorize ack's `type`.
    plain_encoding: ProofEncoding,
    pending: HashMap<u64, Pending>,
}

impl State<'_> {
    async fn emit(&self, e: SessionEvent) {
        let _ = self.ev.send(e).await;
    }

    fn encoding(&self) -> ProofEncoding {
        match self.learner.field() {
            ProofField::PlainProof => self.plain_encoding,
            ProofField::PlainProofZst => ProofEncoding::Zstd,
        }
    }

    /// Handle one server line. `Err` ends the session.
    async fn on_line(&mut self, line: &str) -> Result<(), DisconnectReason> {
        let v: Value = serde_json::from_str(line).map_err(|e| DisconnectReason::Protocol(format!("not JSON: {e}")))?;
        match parse_notify_value(&v) {
            Ok(Some(job)) => {
                tracing::debug!(job_id = %job.job_id, height = ?job.height, "job");
                self.job.send_replace(Some(job.clone()));
                self.emit(SessionEvent::JobReceived(job)).await;
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                // A malformed notify is the pool's problem; keep the previous job.
                tracing::warn!(error = %e, "ignoring malformed mining.notify");
                return Ok(());
            }
        }
        let Some((id, outcome)) = reply_of(&v) else {
            if let Some(m) = v.get("method").and_then(Value::as_str) {
                tracing::debug!(method = m, "ignoring pool request");
            }
            return Ok(());
        };
        if id == self.auth_id && !self.authorized {
            return match outcome {
                Reply::Accepted => {
                    self.authorized = true;
                    let proof_type = reply_type(&v).map(str::to_string);
                    match proof_type.as_deref() {
                        Some("v2") => self.plain_encoding = ProofEncoding::Gzip,
                        Some("plain") => self.plain_encoding = ProofEncoding::Plain,
                        _ => {}
                    }
                    self.emit(SessionEvent::Authorized { proof_type }).await;
                    Ok(())
                }
                Reply::Rejected(reason) => {
                    self.emit(SessionEvent::AuthRejected { reason: reason.clone() }).await;
                    Err(DisconnectReason::AuthRejected(reason))
                }
                Reply::Unrelated => Ok(()),
            };
        }
        let Some(p) = self.pending.remove(&id) else {
            return Ok(());
        };
        match outcome {
            Reply::Accepted => {
                self.learner.on_accept();
                self.emit(SessionEvent::ShareAccepted { submit_id: id, job_id: p.job_id }).await;
            }
            Reply::Rejected(reason) => {
                let kind = classify_reject(&reason);
                tracing::info!(submit_id = id, job_id = %p.job_id, ?kind, %reason, "share rejected");
                self.emit(SessionEvent::ShareRejected { submit_id: id, job_id: p.job_id, kind, reason }).await;
                if kind == RejectKind::Format {
                    if let Some(field) = self.learner.on_format_reject() {
                        tracing::warn!(field = field.key(), "switching proof field after repeated format rejects");
                        self.emit(SessionEvent::ProofFieldSwitched { field }).await;
                    }
                }
            }
            Reply::Unrelated => {}
        }
        Ok(())
    }

    /// Build the submit line for the current job, or refuse.
    fn prepare_submit(&mut self, job_id: &str, proof: &[u8]) -> Result<(u64, String), SubmitError> {
        let current = self.job.borrow().as_ref().map(|j| j.job_id.clone());
        if current.as_deref() != Some(job_id) {
            return Err(SubmitError::Stale { job_id: job_id.to_string(), current });
        }
        if !self.authorized {
            return Err(SubmitError::NotAuthorized);
        }
        let enc = self.encoding();
        let b64 = encode::encode_proof(proof, enc).map_err(|e| SubmitError::Encode(e.to_string()))?;
        let id = self.next_id;
        let field = self.learner.field().key();
        let line = submit_line(self.cfg.dialect, id, &self.cfg.wallet, &self.cfg.worker, job_id, field, &b64, &self.cfg.frame_opts())
            .map_err(|e| SubmitError::Encode(e.to_string()))?;
        tracing::info!(submit_id = id, job_id, field, encoding = ?enc, proof_bytes = proof.len(), wire_bytes = line.len(), "submitting share");
        self.next_id += 1;
        Ok((id, line))
    }
}

fn connect_reason(e: ConnectError) -> DisconnectReason {
    match e {
        ConnectError::Certificate { .. } | ConnectError::PinMismatch { .. } => DisconnectReason::Certificate(e.to_string()),
        ConnectError::TlsProtocol { .. } => DisconnectReason::TlsProtocol(e.to_string()),
        ConnectError::Tcp { .. } | ConnectError::Timeout { .. } | ConnectError::ServerName(_) | ConnectError::Config(_) => {
            DisconnectReason::ConnectFailed(e.to_string())
        }
    }
}

async fn send_line(w: &mut WriteHalf<Box<dyn PoolStream>>, line: &str) -> Result<(), DisconnectReason> {
    codec::write_line(w, line).await.map_err(|e| match e {
        CodecError::Io(e) => DisconnectReason::Io(e.to_string()),
        other => DisconnectReason::Protocol(other.to_string()),
    })
}

async fn run(
    cfg: SessionConfig,
    connector: Connector,
    mut cmd: mpsc::Receiver<Command>,
    ev: &mpsc::Sender<SessionEvent>,
    job: &watch::Sender<Option<Job>>,
) -> DisconnectReason {
    let connected = tokio::select! {
        c = connector.connect(&cfg.host, cfg.port, &cfg.tls) => c,
        _ = wait_shutdown(&mut cmd) => return DisconnectReason::Shutdown,
    };
    let connected = match connected {
        Ok(c) => c,
        Err(e) => return connect_reason(e),
    };
    let _ = ev.send(SessionEvent::Connected { transport: connected.transport, plain_fallback: connected.plain_fallback }).await;
    if connected.transport == Transport::Tls {
        let _ = ev.send(SessionEvent::TlsOk { spki_sha256_b64: connected.peer_spki_sha256_b64.clone() }).await;
    }
    let (rd, mut wr) = tokio::io::split(connected.stream);
    let mut reader = NdjsonReader::new(rd);

    let opts = cfg.frame_opts();
    let lines = match authorize_lines(cfg.dialect, 1, &cfg.wallet, &cfg.worker, &cfg.password, &opts) {
        Ok(l) => l,
        Err(e) => return DisconnectReason::Protocol(e.to_string()),
    };
    let n = lines.len() as u64;
    let mut st = State {
        cfg: &cfg,
        ev,
        job,
        auth_id: n,
        next_id: n + 1,
        authorized: false,
        learner: ProofFormatLearner::new(cfg.proof_field),
        plain_encoding: cfg.dialect.plain_field_encoding(),
        pending: HashMap::new(),
    };
    for l in &lines {
        if let Err(r) = send_line(&mut wr, l).await {
            return r;
        }
    }

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(l)) => if let Err(r) = st.on_line(&l).await { break r },
                Ok(None) => break DisconnectReason::Eof,
                Err(CodecError::ReadTooLong { .. }) => break DisconnectReason::LineTooLong,
                Err(CodecError::Io(e)) => break DisconnectReason::Io(e.to_string()),
                Err(e) => break DisconnectReason::Protocol(e.to_string()),
            },
            c = cmd.recv() => match c {
                Some(Command::Submit { job_id, proof, reply }) => match st.prepare_submit(&job_id, &proof) {
                    Ok((id, line)) => {
                        if let Err(r) = send_line(&mut wr, &line).await {
                            let _ = reply.send(Err(SubmitError::Closed));
                            break r;
                        }
                        st.pending.insert(id, Pending { job_id, deadline: Instant::now() + cfg.submit_ack_timeout });
                        let _ = reply.send(Ok(id));
                    }
                    Err(e) => { let _ = reply.send(Err(e)); }
                },
                Some(Command::Shutdown) | None => break DisconnectReason::Shutdown,
            },
            _ = tick.tick() => {
                let now = Instant::now();
                let mut expired: Vec<u64> = st.pending.iter().filter(|(_, p)| p.deadline <= now).map(|(id, _)| *id).collect();
                expired.sort_unstable();
                for id in expired {
                    if let Some(p) = st.pending.remove(&id) {
                        st.emit(SessionEvent::SubmitAckTimeout { submit_id: id, job_id: p.job_id }).await;
                    }
                }
            }
        }
    };
    let _ = wr.shutdown().await;
    reason
}

async fn wait_shutdown(cmd: &mut mpsc::Receiver<Command>) {
    loop {
        match cmd.recv().await {
            Some(Command::Shutdown) | None => return,
            Some(Command::Submit { reply, .. }) => {
                let _ = reply.send(Err(SubmitError::NotAuthorized));
            }
        }
    }
}
