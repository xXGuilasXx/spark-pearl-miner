//! `gpu-worker --sim`: a SIMULATED GPU worker that speaks the real spm-ipc protocol and produces
//! real, locally verified shares on the CPU with the official reference miner
//! (`zk_pow::ffi::mine::try_mine_one`, our 8x16 pattern, m = n = 256, k = 2048).
//!
//! It exists to exercise the daemon (supervisor, arbiter, failover, submit path, GUI) before the
//! CUDA worker lands (M5). The reference miner can only hit the header's own nbits, so it finds
//! shares at trivial difficulty (spm-mockpool) and practically never on a real pool.
//! It never touches the GPU.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::SeedableRng;
use spm_ipc::{read_frame, write_frame, FaultKind, IpcError, ToDaemon, ToWorker, WorkUnit, IPC_VERSION};
use spm_pow::{check_cert_version_eligible, mining_config, IncompleteBlockHeader, SeedDerivation};
use spm_work::Shape;
use zk_pow::api::verify::verify_plain_proof;
use zk_pow::ffi::mine::try_mine_one;

/// The shape the simulated worker mines.
pub const SIM_SHAPE: Shape = Shape { m: 256, n: 256, k: 2048, r: 128 };
/// Device name reported in `Ready`.
pub const SIM_DEVICE: &str = "cpu-sim (reference miner, m=n=256, k=2048)";
/// Largest dimension the simulation accepts (the production shape would need gigabytes).
const SIM_MAX_DIM: u32 = 1024;
const HEARTBEAT: Duration = Duration::from_millis(500);
const STATS_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct SimOptions {
    pub sock: PathBuf,
    /// Pause between two attempts (keeps the CPU and the mock pool calm).
    pub interval: Duration,
    /// How long to keep retrying the connection to the daemon.
    pub connect_timeout: Duration,
}

#[derive(Default)]
struct Shared {
    job: Option<WorkUnit>,
    running: bool,
    duty: u8,
    exit: bool,
}

struct Counters {
    macs: AtomicU64,
    attempts: AtomicU64,
    tiles: AtomicU64,
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(w: &Writer, msg: &ToDaemon) -> Result<(), IpcError> {
    let mut s = w.lock().map_err(|_| IpcError::Closed)?;
    write_frame(&mut *s, msg)
}

fn connect(opts: &SimOptions) -> io::Result<UnixStream> {
    let deadline = Instant::now() + opts.connect_timeout;
    loop {
        match UnixStream::connect(&opts.sock) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Known-answer test: the reference miner on a fixed header must produce a proof the official
/// verifier accepts.
pub fn known_answer_test() -> bool {
    let cfg = match mining_config(SIM_SHAPE.k) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let header = IncompleteBlockHeader {
        version: 0x2000_0000,
        prev_block: [0x11; 32],
        merkle_root: [0x22; 32],
        timestamp: 0x6666_6666,
        nbits: 0x1d7f_ffff,
    };
    let (m, n, k) = (SIM_SHAPE.m as usize, SIM_SHAPE.n as usize, SIM_SHAPE.k as usize);
    for seed in 0..8u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        if let Ok(Some(p)) = try_mine_one(&mut rng, m, n, k, header, cfg, None, false, SeedDerivation::Salted) {
            return verify_plain_proof(&header, &p, None, SeedDerivation::Salted).is_ok();
        }
    }
    false
}

/// One attempt on `wu`: `Ok(Some(bincode proof))` on a verified hit.
fn attempt(wu: &WorkUnit, rng: &mut StdRng) -> Result<Option<Vec<u8>>, String> {
    let header = wu.block_header().map_err(|e| e.to_string())?;
    let cfg = wu.mining_config().map_err(|e| e.to_string())?;
    let (m, n, k) = (wu.shape.m as usize, wu.shape.n as usize, wu.shape.k as usize);
    let found = try_mine_one(rng, m, n, k, header, cfg, None, false, SeedDerivation::Salted).map_err(|e| e.to_string())?;
    let Some(proof) = found else { return Ok(None) };
    // Verify exactly as the pool will, before anything leaves the worker.
    check_cert_version_eligible(wu.cert_version, &proof).map_err(|e| format!("cert version: {e}"))?;
    verify_plain_proof(&header, &proof, Some(wu.nbits_share), SeedDerivation::Salted)
        .map_err(|e| format!("local verify failed: {e}"))?;
    bincode::serialize(&proof).map(Some).map_err(|e| e.to_string())
}

fn miner_loop(state: Arc<(Mutex<Shared>, Condvar)>, w: Writer, c: Arc<Counters>, interval: Duration, stop: Arc<AtomicBool>) {
    let mut rng = StdRng::from_os_rng();
    loop {
        let wu = {
            let (lock, cv) = &*state;
            let mut s = match lock.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            while !s.exit && !(s.running && s.job.is_some()) {
                s = match cv.wait(s) {
                    Ok(s) => s,
                    Err(_) => return,
                };
            }
            if s.exit {
                return;
            }
            let pause = interval.mul_f64(100.0 / f64::from(s.duty.clamp(1, 100)));
            (s.job.clone(), pause)
        };
        let (Some(wu), pause) = wu else { continue };
        if wu.shape.m > SIM_MAX_DIM || wu.shape.n > SIM_MAX_DIM {
            let _ = send(&w, &ToDaemon::Fault {
                kind: FaultKind::Other,
                msg: format!("the simulated worker only mines small shapes (got m={} n={}); set worker.simulate = true in the daemon", wu.shape.m, wu.shape.n),
            });
            if let Ok(mut s) = state.0.lock() {
                s.job = None;
            }
            continue;
        }
        match attempt(&wu, &mut rng) {
            Ok(hit) => {
                c.attempts.fetch_add(1, Ordering::Relaxed);
                c.macs.fetch_add(u64::from(wu.shape.m) * u64::from(wu.shape.n) * u64::from(wu.shape.k), Ordering::Relaxed);
                c.tiles.fetch_add(u64::from(wu.shape.m / 64) * u64::from(wu.shape.n / 64), Ordering::Relaxed);
                if let Some(proof_bincode) = hit {
                    // Still the current job? A superseded job's hit is dropped here.
                    let current = state.0.lock().ok().and_then(|s| s.job.as_ref().map(|j| j.wu_id)) == Some(wu.wu_id);
                    if current {
                        let msg = ToDaemon::Proof {
                            wu_id: wu.wu_id,
                            session_id: wu.session_id,
                            job_id: wu.job_id.clone(),
                            is_block: false,
                            digest: proof_digest(&proof_bincode),
                            t_rows: 0,
                            t_cols: 0,
                            proof_bincode,
                        };
                        if send(&w, &msg).is_err() {
                            stop.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = send(&w, &ToDaemon::Fault { kind: FaultKind::VerifyFailed, msg: e });
            }
        }
        let deadline = Instant::now() + pause;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) || state.0.lock().map(|s| s.exit).unwrap_or(true) {
                return;
            }
            thread::sleep(Duration::from_millis(20).min(pause));
        }
    }
}

/// The reference miner does not return the jackpot hash; the simulation reports SHA-256 of the
/// proof instead (an identifier, never compared against a bound).
fn proof_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Run the simulated worker until the daemon says Release/Shutdown or the socket closes.
pub fn run(opts: SimOptions) -> anyhow::Result<()> {
    let stream = connect(&opts).map_err(|e| anyhow::anyhow!("cannot reach the daemon at {}: {e}", opts.sock.display()))?;
    let mut reader = stream.try_clone()?;
    let w: Writer = Arc::new(Mutex::new(stream));
    match read_frame::<_, ToWorker>(&mut reader)? {
        ToWorker::Hello { version } if version == IPC_VERSION => {}
        other => {
            let _ = send(&w, &ToDaemon::Fault { kind: FaultKind::Protocol, msg: format!("expected Hello, got {other:?}") });
            anyhow::bail!("protocol error: expected Hello");
        }
    }
    let kat_ok = known_answer_test();
    send(&w, &ToDaemon::Ready { kat_ok, device: SIM_DEVICE.to_string() })?;
    if !kat_ok {
        let _ = send(&w, &ToDaemon::Fault { kind: FaultKind::KatFailed, msg: "reference-miner known-answer test failed".into() });
        anyhow::bail!("known-answer test failed");
    }

    let state = Arc::new((Mutex::new(Shared { duty: 100, ..Shared::default() }), Condvar::new()));
    let counters = Arc::new(Counters { macs: AtomicU64::new(0), attempts: AtomicU64::new(0), tiles: AtomicU64::new(0) });
    let stop = Arc::new(AtomicBool::new(false));

    let miner = {
        let (state, w, c, stop) = (state.clone(), w.clone(), counters.clone(), stop.clone());
        thread::spawn(move || miner_loop(state, w, c, opts.interval, stop))
    };
    let beat = {
        let (w, c, stop) = (w.clone(), counters.clone(), stop.clone());
        thread::spawn(move || {
            let mut last_stats = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(HEARTBEAT);
                if send(&w, &ToDaemon::Heartbeat { ts: crate::paths::unix_ms() }).is_err() {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                if last_stats.elapsed() >= STATS_EVERY {
                    last_stats = Instant::now();
                    let msg = ToDaemon::Stats {
                        credited_macs: c.macs.swap(0, Ordering::Relaxed),
                        tiles: c.tiles.swap(0, Ordering::Relaxed),
                        attempts: c.attempts.swap(0, Ordering::Relaxed),
                        sm_clock_mhz: 0,
                        power_w: 0.0,
                    };
                    if send(&w, &msg).is_err() {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        })
    };

    let result = loop {
        let msg = match read_frame::<_, ToWorker>(&mut reader) {
            Ok(m) => m,
            Err(IpcError::Closed) => break Ok(()),
            Err(e) => break Err(anyhow::anyhow!("ipc: {e}")),
        };
        let (lock, cv) = &*state;
        let mut s = lock.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        match msg {
            ToWorker::Hello { .. } => {}
            ToWorker::SetJob { wu } => s.job = Some(*wu),
            ToWorker::Pause => s.running = false,
            ToWorker::Resume => s.running = true,
            ToWorker::SetDuty { pct } => s.duty = pct.clamp(1, 100),
            ToWorker::Release | ToWorker::Shutdown => break Ok(()),
        }
        cv.notify_all();
    };
    stop.store(true, Ordering::Relaxed);
    if let Ok(mut s) = state.0.lock() {
        s.exit = true;
    }
    state.1.notify_all();
    let _ = w.lock().map(|s| s.shutdown(std::net::Shutdown::Both));
    let _ = beat.join();
    let _ = miner.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kat_passes() {
        assert!(known_answer_test());
    }
}
