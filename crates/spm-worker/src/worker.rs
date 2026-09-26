//! The worker process: IPC with the daemon, the pause/resume handshake, and the attempt loop.
//!
//! Threads:
//! * **mining** (the caller of [`run_with`]): owns the engine. Job switch → host job and device
//!   job; per attempt: nonce patch, `set_attempt`, chunks, hits to the verifier.
//! * **ipc reader**: `SetJob` / `Pause` / `Resume` / `SetDuty` / `Release` / `Shutdown`. Anything
//!   that must stop GPU work sets the abort word at once, so the chunk in flight stops within a
//!   tile (tens of µs on the GB10) instead of running to its end.
//! * **heartbeat**: `Heartbeat` every 500 ms, `Stats` every second (NVML clock and power), the
//!   running memory guard every 2 s.
//! * **verifier**: per finished attempt, the canary tile and the proof of every share
//!   (`verify_v3` before anything is sent).
//! * **signals**: SIGUSR1 / SIGUSR2 (pause / resume, same state machine as the IPC frames; the
//!   last command wins) and SIGTERM / SIGINT (exit at the next quiescent point).
//!
//! Every acknowledgement of the handshake (`spm_coexist::handshake`) is written to `worker.ack`
//! next to the socket: a pause is acknowledged once nothing is queued on the device.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use spm_coexist::handshake::{write_ack_file, Ack, Signal, WorkerHandshake, ACK_FILE};
use spm_coexist::memguard::{self, MemVerdict, WORKER_BUDGET_BYTES};
use spm_cpuref::{verify_v3, U256};
use spm_ipc::{
    read_frame, write_frame, FaultKind, IpcError, ToDaemon, ToWorker, WorkUnit, IPC_VERSION,
};
use spm_work::le_bytes;

use crate::engine::{AbortFlag, ChunkStatus, Engine, EngineError, Hit};
use crate::host::{AttemptHost, JobHost, JobKey};
use crate::kat;
use crate::telemetry::Telemetry;

/// Consecutive attempts without a single loose-bound hit that count as a compute fault (with the
/// default 8 expected per attempt, 4 empty attempts in a row happen once in ~10¹⁴).
pub const NO_HIT_LIMIT: u32 = 4;
/// How long to wait for the daemon's Hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Finished attempts waiting for the verifier.
const VERIFY_QUEUE: usize = 64;
/// Longest duty-cycle sleep after one chunk.
const MAX_DUTY_SLEEP: Duration = Duration::from_secs(1);

/// Worker settings.
#[derive(Debug, Clone)]
pub struct Options {
    /// The daemon's `worker.sock`; the ACK file is `worker.ack` in the same directory.
    pub sock: PathBuf,
    /// How long to keep retrying the connection.
    pub connect_timeout: Duration,
    pub heartbeat: Duration,
    pub stats_every: Duration,
    /// Hits per attempt wanted at the loosened device bound: the canary is drawn from them.
    pub canary_hits: u32,
    /// SIGUSR1/SIGUSR2/SIGTERM/SIGINT handlers (process-wide; off in in-process tests).
    pub signals: bool,
    /// Memory guard: refuse to start without 20 GiB of headroom after the 2 GiB budget, exit
    /// below 16 GiB available or above 10 % memory pressure.
    pub memory_guard: bool,
    /// NVML clock and power in `Stats`.
    pub telemetry: bool,
    /// First nonce of every job; `None` draws a random one (so a restart never redoes work).
    pub nonce_start: Option<u64>,
}

impl Options {
    pub fn new(sock: PathBuf) -> Self {
        Self {
            sock,
            connect_timeout: Duration::from_secs(15),
            heartbeat: Duration::from_millis(500),
            stats_every: Duration::from_secs(1),
            canary_hits: 8,
            signals: true,
            memory_guard: true,
            telemetry: true,
            nonce_start: None,
        }
    }
}

/// Why the worker stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Release,
    Shutdown,
    /// SIGTERM or SIGINT.
    Signal(&'static str),
    /// The daemon went away (or the socket failed).
    Disconnected(String),
    /// A fault reported to the daemon (KAT, canary, verify, CUDA, memory).
    Fault(FaultKind, String),
}

impl Exit {
    /// Whether the daemon asked for it (exit status 0).
    pub fn is_clean(&self) -> bool {
        matches!(self, Exit::Release | Exit::Shutdown | Exit::Signal(_))
    }
}

/// Totals of one run.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub exit: Exit,
    pub kat_ok: bool,
    pub attempts: u64,
    pub proofs: u64,
    pub canaries: u64,
    pub timings: Timings,
}

/// Where the time went (for tests, benches and the logs).
#[derive(Debug, Clone, Default)]
pub struct Timings {
    /// `JobHost::new` per job switch (fills + both Merkle layer caches + B seed).
    pub host_builds: Vec<Duration>,
    /// `create_job` per job switch (device allocation + B'ᵀ).
    pub device_builds: Vec<Duration>,
    /// Completed attempts.
    pub attempts: u64,
    /// Sum over completed attempts of `set_attempt` → last chunk.
    pub attempt_wall: Duration,
    /// Sum of the chunks' device time.
    pub attempt_kernel: Duration,
    /// Sum of `set_attempt` (A-side prep on the device).
    pub set_attempt: Duration,
    /// Longest chunk.
    pub max_chunk: Duration,
    /// `SetJob` received → the cancelled attempt drained (nothing queued).
    pub cancels: Vec<Duration>,
}

impl Timings {
    fn mean(total: Duration, n: u64) -> Duration {
        if n == 0 {
            Duration::ZERO
        } else {
            total / n as u32
        }
    }

    pub fn mean_attempt_wall(&self) -> Duration {
        Self::mean(self.attempt_wall, self.attempts)
    }

    pub fn mean_attempt_kernel(&self) -> Duration {
        Self::mean(self.attempt_kernel, self.attempts)
    }

    pub fn mean_set_attempt(&self) -> Duration {
        Self::mean(self.set_attempt, self.attempts)
    }
}

/// The loosened device bound that yields about `hits` hits per attempt of `tiles` tiles.
pub fn canary_bound(tiles: u64, hits: u32) -> U256 {
    if hits == 0 || tiles == 0 {
        return U256::zero();
    }
    (U256::MAX / U256::from(tiles))
        .checked_mul(U256::from(hits))
        .unwrap_or(U256::MAX)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Debug)]
struct Control {
    job: Option<Arc<WorkUnit>>,
    /// Bumped by every `SetJob` that changes the work unit: attempts of an older epoch are
    /// cancelled.
    epoch: u64,
    /// The last pause/resume command allows GPU work (no mining before the first `Resume`).
    run: bool,
    hs: WorkerHandshake,
    duty: u8,
    exit: Option<Exit>,
    /// GPU work is queued, or a GPU call is in progress: a pause is acknowledged later.
    gpu_busy: bool,
    /// When the current epoch began.
    epoch_at: Option<Instant>,
}

impl Control {
    fn may_run(&self) -> bool {
        self.exit.is_none() && self.run && self.hs.may_issue_gpu_work() && self.job.is_some()
    }
}

#[derive(Debug, Default)]
struct Counters {
    macs: AtomicU64,
    tiles: AtomicU64,
    attempts: AtomicU64,
    total_attempts: AtomicU64,
    proofs: AtomicU64,
    canaries: AtomicU64,
}

struct Shared {
    ctl: Mutex<Control>,
    cv: Condvar,
    abort: Arc<dyn AbortFlag>,
    ack_path: PathBuf,
    out: Mutex<UnixStream>,
    counters: Counters,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Control> {
        self.ctl.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send(&self, msg: &ToDaemon) -> Result<(), IpcError> {
        let mut s = self.out.lock().unwrap_or_else(|p| p.into_inner());
        write_frame(&mut *s, msg)
    }

    fn write_ack(&self, ack: Ack) {
        if let Err(e) = write_ack_file(&self.ack_path, &ack) {
            tracing::warn!(error = %e, path = %self.ack_path.display(), "could not write the ACK");
        } else {
            tracing::debug!(ack = ack.encode().trim_end(), "handshake ACK");
        }
    }

    fn exiting(&self) -> bool {
        self.lock().exit.is_some()
    }

    /// Records why the worker must stop (the first reason wins) and stops the chunk in flight.
    fn set_exit(&self, exit: Exit) {
        let mut c = self.lock();
        if c.exit.is_none() {
            c.exit = Some(exit);
        }
        self.abort.set();
        self.cv.notify_all();
    }

    /// A pause (SIGUSR1 / `Pause`) or resume (SIGUSR2 / `Resume`) command.
    fn command(&self, c: &mut Control, sig: Signal) {
        let mut ack = c.hs.on_signal(sig);
        c.run = c.hs.may_issue_gpu_work();
        if !c.run {
            self.abort.set();
            if !c.gpu_busy {
                ack = ack.or_else(|| c.hs.on_quiescent());
            }
        }
        if let Some(a) = ack {
            self.write_ack(a);
        }
        self.cv.notify_all();
    }

    fn on_signal(&self, sig: Signal) {
        let mut c = self.lock();
        if c.exit.is_none() {
            self.command(&mut c, sig);
        }
    }

    fn on_message(&self, msg: ToWorker) {
        match msg {
            ToWorker::Hello { .. } => {}
            ToWorker::SetJob { wu } => {
                let mut c = self.lock();
                if c.job.as_deref() != Some(&*wu) {
                    tracing::info!(wu_id = wu.wu_id, job_id = %wu.job_id, "new work unit");
                    c.job = Some(Arc::new(*wu));
                    c.epoch += 1;
                    c.epoch_at = Some(Instant::now());
                    self.abort.set();
                    self.cv.notify_all();
                }
            }
            ToWorker::Pause => {
                let mut c = self.lock();
                self.command(&mut c, Signal::Usr1);
            }
            ToWorker::Resume => {
                let mut c = self.lock();
                self.command(&mut c, Signal::Usr2);
            }
            ToWorker::SetDuty { pct } => {
                let mut c = self.lock();
                c.duty = pct.clamp(1, 100);
                self.cv.notify_all();
            }
            ToWorker::Release => self.set_exit(Exit::Release),
            ToWorker::Shutdown => self.set_exit(Exit::Shutdown),
        }
    }
}

fn connect(opts: &Options) -> io::Result<UnixStream> {
    let deadline = Instant::now() + opts.connect_timeout;
    loop {
        match UnixStream::connect(&opts.sock) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn spawn_reader(sh: Arc<Shared>, mut stream: UnixStream) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("spm-ipc".into())
        .spawn(move || loop {
            match read_frame::<_, ToWorker>(&mut stream) {
                Ok(msg) => sh.on_message(msg),
                Err(IpcError::Closed) => {
                    sh.set_exit(Exit::Disconnected(
                        "the daemon closed the connection".into(),
                    ));
                    return;
                }
                Err(e) => {
                    sh.set_exit(Exit::Disconnected(format!("ipc: {e}")));
                    return;
                }
            }
        })
}

fn spawn_heartbeat(
    sh: Arc<Shared>,
    opts: &Options,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    let (beat, stats_every, telemetry, guard) = (
        opts.heartbeat,
        opts.stats_every,
        opts.telemetry,
        opts.memory_guard,
    );
    thread::Builder::new()
        .name("spm-heartbeat".into())
        .spawn(move || {
            let tele = if telemetry { Telemetry::new() } else { None };
            let mem_every = Duration::from_secs(2);
            let start = Instant::now();
            let (mut next_beat, mut next_stats, mut next_mem) =
                (start, start + stats_every, start + mem_every);
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now >= next_beat {
                    next_beat += beat;
                    if sh.send(&ToDaemon::Heartbeat { ts: unix_ms() }).is_err() {
                        sh.set_exit(Exit::Disconnected("heartbeat write failed".into()));
                        return;
                    }
                }
                if now >= next_stats {
                    next_stats += stats_every;
                    let (sm_clock_mhz, power_w) = tele.as_ref().map_or((0, 0.0), Telemetry::read);
                    let c = &sh.counters;
                    let msg = ToDaemon::Stats {
                        credited_macs: c.macs.swap(0, Ordering::Relaxed),
                        tiles: c.tiles.swap(0, Ordering::Relaxed),
                        attempts: c.attempts.swap(0, Ordering::Relaxed),
                        sm_clock_mhz,
                        power_w,
                    };
                    if sh.send(&msg).is_err() {
                        sh.set_exit(Exit::Disconnected("stats write failed".into()));
                        return;
                    }
                }
                if guard && now >= next_mem {
                    next_mem += mem_every;
                    if let Ok(snap) = memguard::read_snapshot(
                        std::path::Path::new(memguard::MEMINFO_PATH),
                        std::path::Path::new(memguard::PSI_MEMORY_PATH),
                    ) {
                        let verdict = memguard::check_running(&snap);
                        if verdict.must_exit() {
                            let msg = match verdict {
                                MemVerdict::ExitLowMemory { available_bytes } => format!(
                                    "exiting: only {:.1} GiB of memory available",
                                    available_bytes as f64 / memguard::GIB as f64
                                ),
                                MemVerdict::ExitPressure { some_avg10 } => {
                                    format!(
                                        "exiting: memory pressure {some_avg10:.1} % (some avg10)"
                                    )
                                }
                                MemVerdict::Ok => String::new(),
                            };
                            sh.set_exit(Exit::Fault(FaultKind::OutOfMemory, msg));
                        }
                    }
                }
                let wake = next_beat
                    .min(next_stats)
                    .min(if guard { next_mem } else { next_beat });
                let now = Instant::now();
                thread::sleep(
                    wake.saturating_duration_since(now)
                        .min(Duration::from_millis(50)),
                );
            }
        })
}

/// SIGUSR1/SIGUSR2/SIGTERM/SIGINT, installed before `Ready` so no signal can hit the default
/// action (terminate) once the daemon knows the worker.
struct SignalThread {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

fn spawn_signals(sh: Arc<Shared>) -> io::Result<SignalThread> {
    use tokio::signal::unix::{signal, SignalKind};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (mut usr1, mut usr2, mut term, mut int) = {
        let _guard = rt.enter();
        (
            signal(SignalKind::user_defined1())?,
            signal(SignalKind::user_defined2())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    let handle = thread::Builder::new()
        .name("spm-signals".into())
        .spawn(move || {
            rt.block_on(async move {
                loop {
                    tokio::select! {
                        _ = &mut rx => return,
                        Some(()) = usr1.recv() => sh.on_signal(Signal::Usr1),
                        Some(()) = usr2.recv() => sh.on_signal(Signal::Usr2),
                        Some(()) = term.recv() => sh.set_exit(Exit::Signal("SIGTERM")),
                        Some(()) = int.recv() => sh.set_exit(Exit::Signal("SIGINT")),
                        else => return,
                    }
                }
            });
        })?;
    Ok(SignalThread {
        stop: Some(tx),
        handle,
    })
}

/// One finished attempt, for the verifier.
struct Verify {
    seq: u64,
    wu: Arc<WorkUnit>,
    host: Arc<JobHost>,
    att: Arc<AttemptHost>,
    gpu_bound: U256,
    hits: Vec<Hit>,
    total: u32,
}

fn spawn_verifier(
    sh: Arc<Shared>,
    rx: mpsc::Receiver<Verify>,
    canary: bool,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("spm-verify".into())
        .spawn(move || {
            let mut empty_streak = 0u32;
            for v in rx {
                // Once the worker is stopping, queued attempts are dropped: the process must
                // exit (and free the GPU context) promptly.
                if sh.exiting() {
                    return;
                }
                if let Err((kind, msg)) = verify_attempt(&sh, &v, canary, &mut empty_streak) {
                    tracing::error!(?kind, %msg, "compute fault");
                    sh.set_exit(Exit::Fault(kind, msg));
                    return;
                }
            }
        })
}

/// Canary and shares of one attempt. Any disagreement is a compute fault: nothing is sent.
fn verify_attempt(
    sh: &Shared,
    v: &Verify,
    canary: bool,
    empty_streak: &mut u32,
) -> Result<(), (FaultKind, String)> {
    let mut hits = v.hits.clone();
    hits.sort_by_key(|h| (h.t_rows, h.t_cols));
    // A chunk re-run after an abort can report a tile twice; both reports must agree.
    for w in hits.windows(2) {
        if (w[0].t_rows, w[0].t_cols) == (w[1].t_rows, w[1].t_cols) && w[0].digest != w[1].digest {
            return Err((
                FaultKind::CanaryMismatch,
                format!(
                    "tile ({}, {}) reported with two digests",
                    w[0].t_rows, w[0].t_cols
                ),
            ));
        }
    }
    hits.dedup_by_key(|h| (h.t_rows, h.t_cols));
    if v.total as usize > v.hits.len() {
        tracing::warn!(
            total = v.total,
            kept = v.hits.len(),
            "hit ring overflowed; extra hits dropped"
        );
    }
    if let Some(h) = hits
        .iter()
        .find(|h| U256::from_little_endian(&h.digest) > v.gpu_bound)
    {
        return Err((
            FaultKind::CanaryMismatch,
            format!(
                "tile ({}, {}) reported above the device bound",
                h.t_rows, h.t_cols
            ),
        ));
    }
    if canary {
        if hits.is_empty() {
            *empty_streak += 1;
            if *empty_streak >= NO_HIT_LIMIT {
                return Err((
                    FaultKind::CanaryMismatch,
                    format!(
                        "{NO_HIT_LIMIT} attempts in a row without a single hit at the canary bound"
                    ),
                ));
            }
        } else {
            *empty_streak = 0;
            let h = hits[(v.seq % hits.len() as u64) as usize];
            let t = v.host.tile(&v.att, h.t_rows, h.t_cols).map_err(|e| {
                (
                    FaultKind::CanaryMismatch,
                    format!("canary ({}, {}): {e:#}", h.t_rows, h.t_cols),
                )
            })?;
            if t.digest != h.digest {
                return Err((
                    FaultKind::CanaryMismatch,
                    format!(
                        "canary tile ({}, {}) of attempt {}: CPU digest {} != GPU {}",
                        h.t_rows,
                        h.t_cols,
                        v.seq,
                        hex(&t.digest),
                        hex(&h.digest)
                    ),
                ));
            }
            sh.counters.canaries.fetch_add(1, Ordering::Relaxed);
        }
    }
    let share = v.wu.share_bound();
    let block = v.wu.block_bound();
    for h in hits
        .iter()
        .filter(|h| U256::from_little_endian(&h.digest) <= share)
    {
        if sh.exiting() {
            return Ok(());
        }
        let proof = v.host.proof(&v.att, h.t_rows, h.t_cols).map_err(|e| {
            (
                FaultKind::VerifyFailed,
                format!("proof of ({}, {}): {e:#}", h.t_rows, h.t_cols),
            )
        })?;
        verify_v3(v.host.header(), &proof, Some(v.wu.nbits_share)).map_err(|e| {
            (
                FaultKind::VerifyFailed,
                format!("local verify of ({}, {}) failed: {e:#}", h.t_rows, h.t_cols),
            )
        })?;
        let proof_bincode =
            bincode::serialize(&proof).map_err(|e| (FaultKind::Other, e.to_string()))?;
        let is_block = U256::from_little_endian(&h.digest) <= block;
        tracing::info!(
            wu_id = v.wu.wu_id,
            t_rows = h.t_rows,
            t_cols = h.t_cols,
            is_block,
            "verified share"
        );
        let msg = ToDaemon::Proof {
            wu_id: v.wu.wu_id,
            session_id: v.wu.session_id,
            job_id: v.wu.job_id.clone(),
            is_block,
            digest: h.digest,
            t_rows: h.t_rows,
            t_cols: h.t_cols,
            proof_bincode,
        };
        if sh.send(&msg).is_err() {
            return Ok(());
        }
        sh.counters.proofs.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The attempt on the device.
struct Live {
    epoch: u64,
    seq: u64,
    wu: Arc<WorkUnit>,
    host: Arc<JobHost>,
    att: Arc<AttemptHost>,
    gpu_bound: U256,
    started: Instant,
    set_attempt: Duration,
    kernel: Duration,
    chunks: u32,
}

enum Step {
    Exit,
    Cancel,
    Drain,
    Idle,
    Work {
        wu: Arc<WorkUnit>,
        epoch: u64,
        duty: u8,
    },
}

struct Miner<'a, E: Engine + ?Sized> {
    engine: &'a mut E,
    sh: Arc<Shared>,
    opts: &'a Options,
    host: Option<Arc<JobHost>>,
    device_job: Option<JobKey>,
    live: Option<Live>,
    in_flight: bool,
    next_nonce: u64,
    seq: u64,
    verify: mpsc::SyncSender<Verify>,
    timings: Timings,
}

impl<E: Engine + ?Sized> Miner<'_, E> {
    fn decide(&mut self) -> Step {
        let mut c = self.sh.lock();
        if c.exit.is_some() {
            self.sh.abort.set();
            return Step::Exit;
        }
        if self.live.as_ref().is_some_and(|l| l.epoch != c.epoch) {
            self.sh.abort.set();
            return Step::Cancel;
        }
        if !c.may_run() {
            self.sh.abort.set();
            if self.in_flight {
                return Step::Drain;
            }
            c.gpu_busy = false;
            if let Some(a) = c.hs.on_quiescent() {
                self.sh.write_ack(a);
            }
            // Wait for a command with the lock held since the check: no wake-up is lost.
            let _ = self.sh.cv.wait_timeout(c, Duration::from_millis(250));
            return Step::Idle;
        }
        Step::Work {
            wu: c.job.clone().expect("may_run implies a job"),
            epoch: c.epoch,
            duty: c.duty,
        }
    }

    /// Claims the GPU for one call if the state still allows it; clears the abort word.
    fn reserve(&self, epoch: u64) -> bool {
        let mut c = self.sh.lock();
        if !c.may_run() || c.epoch != epoch {
            return false;
        }
        self.sh.abort.clear();
        c.gpu_busy = true;
        true
    }

    /// Nothing is queued on the device any more: acknowledge a pending pause.
    fn quiescent(&self) {
        let mut c = self.sh.lock();
        c.gpu_busy = false;
        if let Some(a) = c.hs.on_quiescent() {
            self.sh.write_ack(a);
        }
    }

    /// With the abort word set, waits for the chunks still queued.
    fn drain(&mut self) -> Result<(), EngineError> {
        while self.in_flight {
            let r = match self.engine.run_chunk() {
                Ok(r) => r,
                Err(e) => {
                    self.in_flight = false;
                    self.quiescent();
                    return Err(e);
                }
            };
            self.account(r.kernel);
            match r.status {
                ChunkStatus::More => {}
                ChunkStatus::Done => {
                    self.in_flight = false;
                    self.finish()?;
                }
                ChunkStatus::Aborted => self.in_flight = false,
            }
        }
        self.quiescent();
        Ok(())
    }

    fn account(&mut self, kernel: Duration) {
        self.timings.max_chunk = self.timings.max_chunk.max(kernel);
        if let Some(l) = self.live.as_mut() {
            l.kernel += kernel;
            l.chunks += 1;
        }
    }

    /// The attempt is complete: count it and hand its hits to the verifier.
    fn finish(&mut self) -> Result<(), EngineError> {
        let Some(l) = self.live.take() else {
            return Ok(());
        };
        let (hits, total) = self.engine.hits()?;
        let spec = l.host.spec();
        let c = &self.sh.counters;
        c.macs.fetch_add(spec.credited_macs(), Ordering::Relaxed);
        c.tiles.fetch_add(spec.hash_tiles(), Ordering::Relaxed);
        c.attempts.fetch_add(1, Ordering::Relaxed);
        c.total_attempts.fetch_add(1, Ordering::Relaxed);
        let wall = l.started.elapsed();
        self.timings.attempts += 1;
        self.timings.attempt_wall += wall;
        self.timings.attempt_kernel += l.kernel;
        self.timings.set_attempt += l.set_attempt;
        tracing::debug!(
            seq = l.seq,
            nonce = l.att.nonce,
            chunks = l.chunks,
            kernel_ms = l.kernel.as_secs_f64() * 1e3,
            wall_ms = wall.as_secs_f64() * 1e3,
            hits = hits.len(),
            "attempt done"
        );
        let _ = self.verify.send(Verify {
            seq: l.seq,
            wu: l.wu,
            host: l.host,
            att: l.att,
            gpu_bound: l.gpu_bound,
            hits,
            total,
        });
        Ok(())
    }

    fn fresh_nonce(&self) -> u64 {
        self.opts
            .nonce_start
            .unwrap_or_else(|| getrandom::u64().unwrap_or_else(|_| unix_ms().rotate_left(29)))
    }

    /// A work unit could not be mined (bad shape, host build failure): report it and drop it.
    fn reject_job(&mut self, epoch: u64, msg: String) {
        tracing::warn!(%msg, "work unit rejected");
        let _ = self.sh.send(&ToDaemon::Fault {
            kind: FaultKind::Other,
            msg,
        });
        let mut c = self.sh.lock();
        if c.epoch == epoch {
            c.job = None;
        }
    }

    fn engine_error(&mut self, epoch: u64, e: EngineError) {
        if e.kind == FaultKind::Other {
            self.reject_job(epoch, e.msg);
        } else {
            self.sh.set_exit(Exit::Fault(e.kind, e.msg));
        }
    }

    fn work(&mut self, wu: Arc<WorkUnit>, epoch: u64, duty: u8) {
        let key = JobKey {
            job_key: wu.job_key,
            shape: wu.shape,
        };
        // Host job (CPU only: a pause meanwhile is acknowledged at once).
        if self.host.as_ref().map(|h| h.key()) != Some(key) {
            self.engine.destroy_job();
            self.device_job = None;
            self.host = None;
            let t0 = Instant::now();
            match JobHost::new(&wu) {
                Ok(h) => {
                    let took = t0.elapsed();
                    self.timings.host_builds.push(took);
                    tracing::info!(
                        m = wu.shape.m,
                        n = wu.shape.n,
                        k = wu.shape.k,
                        ms = took.as_secs_f64() * 1e3,
                        "host job ready (fills, Merkle layers, B seed)"
                    );
                    self.host = Some(Arc::new(h));
                    self.next_nonce = self.fresh_nonce();
                }
                Err(e) => {
                    self.reject_job(epoch, format!("cannot mine work unit {}: {e:#}", wu.wu_id))
                }
            }
            return;
        }
        let host = self.host.clone().expect("host checked above");
        // Device job (B side).
        if self.device_job != Some(key) {
            if !self.reserve(epoch) {
                return;
            }
            let t0 = Instant::now();
            let r = self.engine.create_job(&host.spec());
            self.quiescent();
            match r {
                Ok(()) => {
                    let took = t0.elapsed();
                    self.timings.device_builds.push(took);
                    tracing::info!(
                        ms = took.as_secs_f64() * 1e3,
                        "device job ready (B'ᵀ built)"
                    );
                    self.device_job = Some(key);
                }
                Err(e) => self.engine_error(epoch, e),
            }
            return;
        }
        // Attempt (A side).
        if self.live.is_none() {
            let nonce = self.next_nonce;
            self.next_nonce = nonce.wrapping_add(1);
            let att = host.attempt(nonce);
            let gpu_bound = wu
                .share_bound()
                .max(canary_bound(host.tiles(), self.opts.canary_hits));
            if !self.reserve(epoch) {
                return;
            }
            let started = Instant::now();
            let r = self
                .engine
                .set_attempt(&att.a_noise_seed, &le_bytes(gpu_bound), &att.prefix());
            let set_attempt = started.elapsed();
            self.quiescent();
            if let Err(e) = r {
                self.engine_error(epoch, e);
                return;
            }
            self.seq += 1;
            self.live = Some(Live {
                epoch,
                seq: self.seq,
                wu,
                host,
                att: Arc::new(att),
                gpu_bound,
                started,
                set_attempt,
                kernel: Duration::ZERO,
                chunks: 0,
            });
        }
        // One chunk.
        if !self.reserve(epoch) {
            return;
        }
        let r = match self.engine.run_chunk() {
            Ok(r) => r,
            Err(e) => {
                self.in_flight = false;
                self.quiescent();
                self.engine_error(epoch, e);
                return;
            }
        };
        self.account(r.kernel);
        match r.status {
            ChunkStatus::More => {
                self.in_flight = true;
                if duty < 100 {
                    // The queued chunk runs during the sleep: kernel·100/duty per chunk gives
                    // the requested busy fraction.
                    let sleep = r
                        .kernel
                        .mul_f64(100.0 / f64::from(duty))
                        .min(MAX_DUTY_SLEEP);
                    self.sleep_unless_changed(epoch, sleep);
                }
            }
            ChunkStatus::Done => {
                self.in_flight = false;
                self.quiescent();
                if let Err(e) = self.finish() {
                    self.engine_error(epoch, e);
                }
            }
            ChunkStatus::Aborted => {
                self.in_flight = false;
                self.quiescent();
            }
        }
    }

    fn sleep_unless_changed(&self, epoch: u64, d: Duration) {
        let deadline = Instant::now() + d;
        let mut c = self.sh.lock();
        loop {
            let now = Instant::now();
            if now >= deadline || !c.may_run() || c.epoch != epoch {
                return;
            }
            c = match self.sh.cv.wait_timeout(c, deadline - now) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
    }

    fn run(&mut self) {
        loop {
            match self.decide() {
                Step::Exit => {
                    if let Err(e) = self.drain() {
                        tracing::warn!(error = %e, "drain at exit");
                    }
                    self.live = None;
                    self.engine.destroy_job();
                    self.device_job = None;
                    return;
                }
                Step::Cancel => {
                    let r = self.drain();
                    if let Some(t) = self.sh.lock().epoch_at {
                        self.timings.cancels.push(t.elapsed());
                    }
                    // A cancelled attempt that completed while draining was already verified.
                    if self.live.take().is_some() {
                        tracing::debug!("attempt cancelled by a new work unit");
                    }
                    if let Err(e) = r {
                        self.sh.set_exit(Exit::Fault(e.kind, e.msg));
                    }
                }
                Step::Drain => {
                    if let Err(e) = self.drain() {
                        self.sh.set_exit(Exit::Fault(e.kind, e.msg));
                    }
                }
                Step::Idle => {}
                Step::Work { wu, epoch, duty } => self.work(wu, epoch, duty),
            }
        }
    }
}

/// Runs the worker with the engine `make_engine` builds (after the handshake and the memory
/// check), until the daemon releases it, the connection drops, a signal stops it or a fault.
pub fn run_with<E, F>(opts: Options, make_engine: F) -> Result<Outcome>
where
    E: Engine,
    F: FnOnce() -> Result<E, EngineError>,
{
    let stream = connect(&opts)
        .with_context(|| format!("cannot reach the daemon at {}", opts.sock.display()))?;
    let mut reader = stream.try_clone()?;
    let mut hello_writer = stream.try_clone()?;
    fn fail_early(w: &mut UnixStream, kind: FaultKind, msg: String) -> Result<Outcome> {
        tracing::error!(?kind, %msg, "not starting");
        let _ = write_frame(
            w,
            &ToDaemon::Fault {
                kind,
                msg: msg.clone(),
            },
        );
        Ok(Outcome {
            exit: Exit::Fault(kind, msg),
            kat_ok: false,
            attempts: 0,
            proofs: 0,
            canaries: 0,
            timings: Timings::default(),
        })
    }
    // The daemon says Hello as soon as it accepts; do not hang on a peer that never does.
    reader.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let hello = read_frame::<_, ToWorker>(&mut reader);
    reader.set_read_timeout(None)?;
    match hello {
        Ok(ToWorker::Hello { version }) if version == IPC_VERSION => {}
        Ok(other) => {
            let _ = write_frame(
                &mut hello_writer,
                &ToDaemon::Fault {
                    kind: FaultKind::Protocol,
                    msg: format!("expected Hello, got {other:?}"),
                },
            );
            bail!("protocol error: expected Hello");
        }
        Err(e) => bail!("no Hello from the daemon: {e}"),
    }
    if opts.memory_guard {
        let snap = memguard::read_snapshot(
            std::path::Path::new(memguard::MEMINFO_PATH),
            std::path::Path::new(memguard::PSI_MEMORY_PATH),
        );
        match snap {
            Ok(s) => {
                if let Err(refused) = memguard::check_start(&s, WORKER_BUDGET_BYTES) {
                    return fail_early(
                        &mut hello_writer,
                        FaultKind::OutOfMemory,
                        refused.to_string(),
                    );
                }
            }
            Err(e) => {
                return fail_early(
                    &mut hello_writer,
                    FaultKind::OutOfMemory,
                    format!("memory guard: {e}"),
                );
            }
        }
    }
    let mut engine = match make_engine() {
        Ok(e) => e,
        Err(e) => return fail_early(&mut hello_writer, e.kind, e.msg),
    };
    let device = engine.device();
    let sh = Arc::new(Shared {
        ctl: Mutex::new(Control {
            job: None,
            epoch: 0,
            run: false,
            hs: WorkerHandshake::new(),
            duty: 100,
            exit: None,
            gpu_busy: true,
            epoch_at: None,
        }),
        cv: Condvar::new(),
        abort: engine.abort_flag(),
        ack_path: opts.sock.with_file_name(ACK_FILE),
        out: Mutex::new(stream),
        counters: Counters::default(),
    });
    let signals = if opts.signals {
        match spawn_signals(sh.clone()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "signal handlers not installed");
                None
            }
        }
    } else {
        None
    };
    let stop = Arc::new(AtomicBool::new(false));
    let beat = spawn_heartbeat(sh.clone(), &opts, stop.clone())?;
    let reader_t = spawn_reader(sh.clone(), reader)?;

    // Known-answer test before anything is mined.
    let kat = kat::known_answer_test(&mut engine);
    let kat_ok = kat.is_ok();
    match &kat {
        Ok(r) => tracing::info!(
            tiles = r.tiles,
            hits = r.hits,
            ms = r.elapsed.as_secs_f64() * 1e3,
            %device,
            "known-answer test passed"
        ),
        Err(e) => tracing::error!(error = %format!("{e:#}"), "known-answer test FAILED"),
    }
    {
        let mut c = sh.lock();
        c.gpu_busy = false;
        if let Some(a) = c.hs.on_quiescent() {
            sh.write_ack(a);
        }
    }
    let ready = sh.send(&ToDaemon::Ready {
        kat_ok,
        device: device.clone(),
    });
    // Bounded: with an absurdly easy share target the mining loop waits for the verifier
    // instead of queueing without limit.
    let (verify_tx, verify_rx) = mpsc::sync_channel(VERIFY_QUEUE);
    let verifier = spawn_verifier(sh.clone(), verify_rx, opts.canary_hits > 0)?;
    let mut timings = Timings::default();
    if let Err(e) = kat {
        sh.set_exit(Exit::Fault(
            FaultKind::KatFailed,
            format!("known-answer test failed: {e:#}"),
        ));
    } else if let Err(e) = ready {
        sh.set_exit(Exit::Disconnected(format!("Ready: {e}")));
    } else {
        let mut miner = Miner {
            engine: &mut engine,
            sh: sh.clone(),
            opts: &opts,
            host: None,
            device_job: None,
            live: None,
            in_flight: false,
            next_nonce: 0,
            seq: 0,
            verify: verify_tx.clone(),
            timings: Timings::default(),
        };
        miner.run();
        timings = miner.timings;
    }
    engine.destroy_job();
    drop(verify_tx);
    let _ = verifier.join();
    let exit = sh
        .lock()
        .exit
        .clone()
        .unwrap_or_else(|| Exit::Disconnected("stopped".into()));
    if let Exit::Fault(kind, msg) = &exit {
        let _ = sh.send(&ToDaemon::Fault {
            kind: *kind,
            msg: msg.clone(),
        });
    }
    tracing::info!(?exit, "worker stopping");
    stop.store(true, Ordering::Relaxed);
    let _ = beat.join();
    if let Some(mut s) = signals {
        if let Some(tx) = s.stop.take() {
            let _ = tx.send(());
        }
        let _ = s.handle.join();
    }
    {
        let s = sh.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    let _ = reader_t.join();
    let c = &sh.counters;
    Ok(Outcome {
        exit,
        kat_ok,
        attempts: c.total_attempts.load(Ordering::Relaxed),
        proofs: c.proofs.load(Ordering::Relaxed),
        canaries: c.canaries.load(Ordering::Relaxed),
        timings,
    })
}

/// [`run_with`] for the binary: an error for anything but a requested exit.
pub fn run_to_exit<E, F>(opts: Options, make_engine: F) -> Result<()>
where
    E: Engine,
    F: FnOnce() -> Result<E, EngineError>,
{
    let out = run_with(opts, make_engine)?;
    match out.exit {
        e if e.is_clean() => Ok(()),
        Exit::Fault(kind, msg) => Err(anyhow!("{kind:?}: {msg}")),
        Exit::Disconnected(why) => Err(anyhow!("{why}")),
        _ => Ok(()),
    }
}
