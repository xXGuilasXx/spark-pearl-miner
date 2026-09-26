//! A CPU implementation of the job ABI, built only from the `spm-cpuref` oracle pieces. It is
//! slow (~10⁸ MAC/s) and meant for small shapes: the worker's IPC tests run the real loop over it
//! without a GPU, and it doubles as an independent check of the host pipeline (its tiles come
//! from whole noised operands, the host's canary from single rows).
//!
//! Chunks honour the abort word per tile like the kernel, can be slowed down to look like device
//! time, and faults can be injected (corrupted digests) to exercise the canary.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spm_cpuref::{
    fill_int7_at, tile_from_rows, NoiseSide, TileResult, DOMAIN_A, DOMAIN_BT, SEED_LABEL_A,
    SEED_LABEL_B, U256,
};
use spm_ipc::FaultKind;
use spm_pow::{HASH_COLS, HASH_ROWS, NOISE_RANK};

use crate::engine::{AbortFlag, ChunkReport, ChunkStatus, Engine, EngineError, Hit, JobSpec};

/// Largest m or n the CPU engine accepts.
pub const CPU_MAX_DIM: u32 = 2048;

/// Abort word in host memory.
#[derive(Debug, Default)]
pub struct AtomicAbort(AtomicBool);

impl AbortFlag for AtomicAbort {
    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
    fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Knobs of the CPU engine.
#[derive(Debug, Clone)]
pub struct CpuOptions {
    /// Hash tiles per chunk.
    pub chunk_tiles: usize,
    /// Extra time per chunk (to look like a GPU chunk of a few ms).
    pub chunk_delay: Duration,
    /// Flip a byte of every reported digest (a compute fault the canary must catch).
    pub corrupt_digests: bool,
    /// Flip a transcript word of every dumped record (a fault the known-answer test must catch).
    pub corrupt_dump: bool,
}

impl Default for CpuOptions {
    fn default() -> Self {
        Self {
            chunk_tiles: 64,
            chunk_delay: Duration::ZERO,
            corrupt_digests: false,
            corrupt_dump: false,
        }
    }
}

/// What the engine was asked to do, for tests.
#[derive(Debug, Default)]
pub struct CpuCounters {
    pub jobs_created: AtomicU64,
    pub attempts_set: AtomicU64,
    pub chunks_run: AtomicU64,
    pub chunks_aborted: AtomicU64,
}

struct CpuAttempt {
    a_noised: Vec<i8>,
    key: [u8; 32],
    bound: U256,
    next: usize,
    hits: Vec<Hit>,
    dump: Vec<TileResult>,
}

struct CpuJob {
    spec: JobSpec,
    bt_noised: Vec<i8>,
    tiles: Vec<(u32, u32)>,
    attempt: Option<CpuAttempt>,
}

pub struct CpuEngine {
    abort: Arc<AtomicAbort>,
    opts: CpuOptions,
    job: Option<CpuJob>,
    counters: Arc<CpuCounters>,
    /// Shared log of the (m, n, k) of every job created (tests).
    pub created: Arc<Mutex<Vec<(u32, u32, u32)>>>,
}

impl CpuEngine {
    pub fn new(opts: CpuOptions) -> Self {
        Self {
            abort: Arc::new(AtomicAbort::default()),
            opts,
            job: None,
            counters: Arc::new(CpuCounters::default()),
            created: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn counters(&self) -> Arc<CpuCounters> {
        self.counters.clone()
    }
}

fn other(msg: impl Into<String>) -> EngineError {
    EngineError::new(FaultKind::Other, msg)
}

fn generated(seed: u64, domain: u64, rows: usize, k: usize) -> Vec<i8> {
    let mut v = vec![0i8; rows * k];
    fill_int7_at(seed, domain, 0, &mut v);
    v
}

impl Engine for CpuEngine {
    fn device(&self) -> String {
        "cpu-reference (spm-cpuref rows, small shapes only)".into()
    }

    fn abort_flag(&self) -> Arc<dyn AbortFlag> {
        self.abort.clone()
    }

    fn create_job(&mut self, spec: &JobSpec) -> Result<(), EngineError> {
        self.job = None;
        if spec.m > CPU_MAX_DIM
            || spec.n > CPU_MAX_DIM
            || !spec.m.is_multiple_of(64)
            || !spec.n.is_multiple_of(64)
        {
            return Err(other(format!(
                "the CPU engine takes m, n multiples of 64 up to {CPU_MAX_DIM} (got {}x{})",
                spec.m, spec.n
            )));
        }
        let (n, k) = (spec.n as usize, spec.k as usize);
        let rank = usize::from(NOISE_RANK);
        let rows: Vec<usize> = (0..n).collect();
        let bt = generated(spec.gen_seed, DOMAIN_BT, n, k);
        let bt_noised = NoiseSide::new(&SEED_LABEL_B, &spec.b_noise_seed, k, rank)
            .noised_rows(&bt, &rows)
            .map_err(|e| other(e.to_string()))?;
        let mut tiles = Vec::with_capacity(spec.hash_tiles() as usize);
        for t_rows in (0..spec.m).filter(|r| r % 64 < 8) {
            for t_cols in (0..spec.n).filter(|c| c % 64 < 8 && c % 2 == 0) {
                tiles.push((t_rows, t_cols));
            }
        }
        self.counters.jobs_created.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut c) = self.created.lock() {
            c.push((spec.m, spec.n, spec.k));
        }
        self.job = Some(CpuJob {
            spec: *spec,
            bt_noised,
            tiles,
            attempt: None,
        });
        Ok(())
    }

    fn destroy_job(&mut self) {
        self.job = None;
    }

    fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound_le: &[u8; 32],
        prefix: &[i8],
    ) -> Result<(), EngineError> {
        let job = self.job.as_mut().ok_or_else(|| other("no job"))?;
        let (m, k) = (job.spec.m as usize, job.spec.k as usize);
        let mut a = generated(job.spec.gen_seed, DOMAIN_A, m, k);
        if prefix.len() > 4096 || prefix.len() > a.len() {
            return Err(other("prefix too long"));
        }
        a[..prefix.len()].copy_from_slice(prefix);
        let rows: Vec<usize> = (0..m).collect();
        let a_noised = NoiseSide::new(&SEED_LABEL_A, a_noise_seed, k, usize::from(NOISE_RANK))
            .noised_rows(&a, &rows)
            .map_err(|e| other(e.to_string()))?;
        self.counters.attempts_set.fetch_add(1, Ordering::Relaxed);
        job.attempt = Some(CpuAttempt {
            a_noised,
            key: *a_noise_seed,
            bound: U256::from_little_endian(bound_le),
            next: 0,
            hits: Vec::new(),
            dump: Vec::new(),
        });
        Ok(())
    }

    fn run_chunk(&mut self) -> Result<ChunkReport, EngineError> {
        let start = Instant::now();
        let job = self.job.as_mut().ok_or_else(|| other("no job"))?;
        let k = job.spec.k as usize;
        let rank = usize::from(NOISE_RANK);
        let att = job.attempt.as_mut().ok_or_else(|| other("no attempt"))?;
        if att.next >= job.tiles.len() {
            return Ok(ChunkReport {
                status: ChunkStatus::Done,
                kernel: Duration::ZERO,
            });
        }
        if self.abort.is_set() {
            self.counters.chunks_aborted.fetch_add(1, Ordering::Relaxed);
            return Ok(ChunkReport {
                status: ChunkStatus::Aborted,
                kernel: Duration::ZERO,
            });
        }
        let end = (att.next + self.opts.chunk_tiles.max(1)).min(job.tiles.len());
        let mut found = Vec::new();
        let mut records = Vec::new();
        for &(t_rows, t_cols) in &job.tiles[att.next..end] {
            if self.abort.is_set() {
                // The chunk stops; the next call re-runs it from its first tile.
                self.counters.chunks_aborted.fetch_add(1, Ordering::Relaxed);
                return Ok(ChunkReport {
                    status: ChunkStatus::Aborted,
                    kernel: start.elapsed(),
                });
            }
            let mut a = Vec::with_capacity(8 * k);
            for d in HASH_ROWS {
                let r = (t_rows + d) as usize;
                a.extend_from_slice(&att.a_noised[r * k..(r + 1) * k]);
            }
            let mut bt = Vec::with_capacity(16 * k);
            for d in HASH_COLS {
                let c = (t_cols + d) as usize;
                bt.extend_from_slice(&job.bt_noised[c * k..(c + 1) * k]);
            }
            let mut t = tile_from_rows(t_rows, t_cols, &a, &bt, k, rank, &att.key)
                .map_err(|e| other(e.to_string()))?;
            if job.spec.dump {
                let mut rec = t;
                if self.opts.corrupt_dump {
                    rec.transcript[3] ^= 1;
                }
                records.push(rec);
            }
            if U256::from_little_endian(&t.digest) <= att.bound {
                if self.opts.corrupt_digests {
                    t.digest[0] ^= 0x01;
                }
                found.push(Hit {
                    t_rows,
                    t_cols,
                    digest: t.digest,
                });
            }
        }
        if !self.opts.chunk_delay.is_zero() {
            std::thread::sleep(self.opts.chunk_delay);
        }
        att.next = end;
        att.hits.extend(found);
        att.dump.extend(records);
        self.counters.chunks_run.fetch_add(1, Ordering::Relaxed);
        Ok(ChunkReport {
            status: if end == job.tiles.len() {
                ChunkStatus::Done
            } else {
                ChunkStatus::More
            },
            kernel: start.elapsed(),
        })
    }

    fn hits(&mut self) -> Result<(Vec<Hit>, u32), EngineError> {
        let att = self
            .job
            .as_ref()
            .and_then(|j| j.attempt.as_ref())
            .ok_or_else(|| other("no attempt"))?;
        Ok((att.hits.clone(), att.hits.len() as u32))
    }

    fn dump(&mut self) -> Result<Vec<TileResult>, EngineError> {
        let job = self.job.as_ref().ok_or_else(|| other("no job"))?;
        if !job.spec.dump {
            return Err(other("the job was not created in dump mode"));
        }
        let att = job.attempt.as_ref().ok_or_else(|| other("no attempt"))?;
        Ok(att.dump.clone())
    }
}
