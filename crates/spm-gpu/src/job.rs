//! Safe mining-job API over the fused GEMM + hash kernel.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ffi;

/// Status codes of the C ABI (`SPM_*` in spm_cuda.h).
mod code {
    pub const OK: i32 = 0;
    pub const DONE: i32 = 1;
    pub const ABORTED: i32 = 2;
    pub const INVALID: i32 = -1;
    pub const SHAPE: i32 = -2;
    pub const BUDGET: i32 = -3;
    pub const NO_ATTEMPT: i32 = -4;
    pub const NOT_DUMP: i32 = -5;
    pub const CUDA: i32 = -6;
    pub const TMA: i32 = -7;
    pub const OOM: i32 = -8;
    pub const SIZE: i32 = -9;
}

/// Size of one dump record (`spm_cpuref::TileResult::dump_bytes`).
pub const DUMP_RECORD_LEN: usize = 104;
/// Default device-memory budget of a job.
pub const DEFAULT_MEM_BUDGET: u64 = 2 << 30;

/// Failure of a GPU call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    #[error("invalid argument")]
    Invalid,
    #[error("unsupported shape: m, n must be multiples of 64 up to 2^24 and k a multiple of 64 in [128, 65536]")]
    Shape,
    #[error("the job would exceed its device-memory budget")]
    Budget,
    #[error("no attempt set (call set_attempt first)")]
    NoAttempt,
    #[error("the job was not created in dump mode")]
    NotDump,
    #[error("CUDA error {code}")]
    Cuda { code: i32 },
    #[error("TMA descriptor encoding failed")]
    Tma,
    #[error("device allocation failed (CUDA error {code})")]
    OutOfMemory { code: i32 },
    #[error("buffer size mismatch")]
    Size,
    #[error("unexpected status {0} ({1})")]
    Unknown(i32, String),
}

impl GpuError {
    fn from_code(rc: i32) -> Self {
        match rc {
            code::INVALID => Self::Invalid,
            code::SHAPE => Self::Shape,
            code::BUDGET => Self::Budget,
            code::NO_ATTEMPT => Self::NoAttempt,
            code::NOT_DUMP => Self::NotDump,
            code::CUDA => Self::Cuda {
                code: ffi::last_cuda_error(),
            },
            code::TMA => Self::Tma,
            code::OOM => Self::OutOfMemory {
                code: ffi::last_cuda_error(),
            },
            code::SIZE => Self::Size,
            other => Self::Unknown(other, ffi::status_str(other)),
        }
    }
}

fn check(rc: i32) -> Result<(), GpuError> {
    if rc == code::OK {
        Ok(())
    } else {
        Err(GpuError::from_code(rc))
    }
}

/// Cancellation flag shared between a control thread and the running kernels. Setting it stops
/// the current chunk at the next CTA tile (well under 1 ms) and makes further chunks return
/// [`ChunkStatus::Aborted`] until it is cleared.
#[derive(Clone)]
pub struct AbortHandle(Arc<ffi::AbortPtr>);

impl AbortHandle {
    pub fn new() -> Result<Self, GpuError> {
        ffi::AbortPtr::new()
            .map(|p| Self(Arc::new(p)))
            .map_err(GpuError::from_code)
    }
    pub fn set(&self) {
        self.0.set(1);
    }
    pub fn clear(&self) {
        self.0.set(0);
    }
    pub fn is_set(&self) -> bool {
        self.0.get() != 0
    }
}

impl std::fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortHandle")
            .field("set", &self.is_set())
            .finish()
    }
}

/// Where the un-noised operands come from.
#[derive(Debug, Clone, Copy)]
pub enum Operands<'a> {
    /// `A = fill_int7(seed, DOMAIN_A)`, `Bᵀ = fill_int7(seed, DOMAIN_BT)` (`spm_cpuref::Problem::generate`),
    /// regenerated on the GPU and never stored.
    Generated { seed: u64 },
    /// Caller-supplied A (m×k) and Bᵀ (n×k), row major, copied to the device.
    Host { a: &'a [i8], bt: &'a [i8] },
}

/// Job parameters. The commitment is computed on the host: pass `b_noise_seed` here and the
/// `a_noise_seed` of every attempt to [`Job::set_attempt`].
#[derive(Debug, Clone)]
pub struct JobConfig<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub operands: Operands<'a>,
    pub b_noise_seed: [u8; 32],
    /// Write one 104-byte `TileResult` record per hash tile (G0 debugging; needs m·n/128·104 bytes).
    pub dump: bool,
    /// Hit ring entries (0 = 4096).
    pub hit_capacity: u32,
    /// CTA tiles (128×256) per launch; `None` = adaptive to `target_chunk`.
    pub chunk_tiles: Option<u32>,
    /// Adaptive chunk duration target (default 6 ms, leaving margin under the 10 ms contract).
    pub target_chunk: Duration,
    /// L2 raster band height in CTA rows (0 = 16).
    pub band_rows: u32,
    /// Device-memory budget (default 2 GiB).
    pub mem_budget_bytes: u64,
    /// Shared cancellation flag; a private one is created when `None`.
    pub abort: Option<AbortHandle>,
}

impl<'a> JobConfig<'a> {
    pub fn new(m: u32, n: u32, k: u32, operands: Operands<'a>, b_noise_seed: [u8; 32]) -> Self {
        Self {
            m,
            n,
            k,
            operands,
            b_noise_seed,
            dump: false,
            hit_capacity: 0,
            chunk_tiles: None,
            target_chunk: Duration::from_millis(6),
            band_rows: 0,
            mem_budget_bytes: DEFAULT_MEM_BUDGET,
            abort: None,
        }
    }
}

/// Outcome of one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStatus {
    /// More chunks remain in the attempt.
    More,
    /// Every hash tile of the attempt has been evaluated.
    Done,
    /// The abort flag stopped the chunk; the next call re-runs it from its first tile.
    Aborted,
}

#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub status: ChunkStatus,
    pub tile_begin: u32,
    pub tile_end: u32,
    pub tiles_total: u32,
    pub ctas: u32,
    /// Kernel time (CUDA events).
    pub kernel: Duration,
}

/// A hash tile whose digest is ≤ the attempt's bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hit {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// One dump record: exactly the fields of `spm_cpuref::TileResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRecord {
    pub t_rows: u32,
    pub t_cols: u32,
    pub transcript: [u32; 16],
    pub digest: [u8; 32],
}

impl TileRecord {
    /// Parses one 104-byte record (`t_rows`, `t_cols`, 16 words, digest; all LE).
    pub fn from_bytes(b: &[u8; DUMP_RECORD_LEN]) -> Self {
        let word =
            |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let mut transcript = [0u32; 16];
        for (i, w) in transcript.iter_mut().enumerate() {
            *w = word(2 + i);
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&b[72..104]);
        Self {
            t_rows: word(0),
            t_cols: word(1),
            transcript,
            digest,
        }
    }
}

/// Debug buffers of a job (`SPM_BUF_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    /// A' (m×k s8) of the current attempt.
    ANoised,
    /// B'ᵀ (n×k s8).
    BtNoised,
    /// A_L (m×128 s8) of the current attempt.
    AFactor,
    /// B_Rᵀ (n×128 s8).
    BtFactor,
    /// A_R as k pairs {p, q}.
    APairs,
    /// B_L as k pairs {p, q}.
    BPairs,
    /// E_A (m×k s8), recomputed.
    ANoise,
    /// E_Bᵀ (n×k s8), recomputed.
    BtNoise,
    /// A (m×k s8).
    ABase,
    /// Bᵀ (n×k s8), generated jobs only.
    BtBase,
}

impl Buffer {
    fn raw(self) -> i32 {
        match self {
            Self::ANoised => 0,
            Self::BtNoised => 1,
            Self::AFactor => 2,
            Self::BtFactor => 3,
            Self::APairs => 4,
            Self::BPairs => 5,
            Self::ANoise => 6,
            Self::BtNoise => 7,
            Self::ABase => 8,
            Self::BtBase => 9,
        }
    }
}

/// Static facts about a job and its kernel.
#[derive(Debug, Clone, Copy)]
pub struct JobInfo {
    pub device_bytes: u64,
    pub tiles_m: u32,
    pub tiles_n: u32,
    pub k_slices: u32,
    pub ctas: u32,
    pub chunk_tiles: u32,
    pub regs_per_thread: u32,
    pub local_bytes: u32,
    pub smem_bytes: u32,
    pub threads: u32,
}

/// Totals of one attempt run to completion (or abort).
#[derive(Debug, Clone, Copy)]
pub struct AttemptStats {
    pub completed: bool,
    pub chunks: u32,
    pub kernel: Duration,
    pub max_chunk: Duration,
    pub wall: Duration,
}

/// A mining job on the GPU: B'ᵀ is built once, A' once per attempt.
pub struct Job {
    raw: ffi::JobPtr,
    abort: AbortHandle,
    m: u32,
    n: u32,
    k: u32,
    dump: bool,
    hit_capacity: u32,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("m", &self.m)
            .field("n", &self.n)
            .field("k", &self.k)
            .field("dump", &self.dump)
            .finish()
    }
}

impl Job {
    /// Allocates the job and builds B_Rᵀ, the B_L pairs and B'ᵀ.
    pub fn new(cfg: &JobConfig<'_>) -> Result<Self, GpuError> {
        let rows = |r: u32| (r as usize).checked_mul(cfg.k as usize);
        let host = match cfg.operands {
            Operands::Generated { .. } => None,
            Operands::Host { a, bt } => {
                if rows(cfg.m) != Some(a.len()) || rows(cfg.n) != Some(bt.len()) {
                    return Err(GpuError::Size);
                }
                Some((a, bt))
            }
        };
        let gen_seed = match cfg.operands {
            Operands::Generated { seed } => seed,
            Operands::Host { .. } => 0,
        };
        let abort = match &cfg.abort {
            Some(a) => a.clone(),
            None => AbortHandle::new()?,
        };
        let target_us = u32::try_from(cfg.target_chunk.as_micros())
            .unwrap_or(u32::MAX)
            .max(1);
        let args = ffi::CreateArgs {
            m: cfg.m,
            n: cfg.n,
            k: cfg.k,
            dump_mode: cfg.dump,
            gen_seed,
            host,
            b_noise_seed: cfg.b_noise_seed,
            hit_capacity: cfg.hit_capacity,
            chunk_tiles: cfg.chunk_tiles.unwrap_or(0),
            target_chunk_us: target_us,
            band_rows: cfg.band_rows,
            mem_budget_bytes: cfg.mem_budget_bytes,
        };
        let raw = ffi::JobPtr::create(&args, &abort.0).map_err(GpuError::from_code)?;
        Ok(Self {
            raw,
            abort,
            m: cfg.m,
            n: cfg.n,
            k: cfg.k,
            dump: cfg.dump,
            hit_capacity: if cfg.hit_capacity == 0 {
                4096
            } else {
                cfg.hit_capacity
            },
        })
    }

    /// Starts an attempt: builds A_L, the A_R pairs and A' for `a_noise_seed`, sets the bound
    /// (32 bytes, little-endian U256), clears the hits and rewinds to the first tile. A chunk of
    /// the previous attempt that is still queued runs to its end first (set the abort flag to cut
    /// it short).
    pub fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound: &[u8; 32],
    ) -> Result<(), GpuError> {
        check(self.raw.set_attempt(a_noise_seed, bound, &[]))
    }

    /// Same as [`Job::set_attempt`] with the first `prefix.len()` (≤ 4096) entries of A replaced
    /// by `prefix` (the nonce patch of the attempt).
    pub fn set_attempt_with_prefix(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound: &[u8; 32],
        prefix: &[i8],
    ) -> Result<(), GpuError> {
        let bytes: Vec<u8> = prefix.iter().map(|&x| x as u8).collect();
        check(self.raw.set_attempt(a_noise_seed, bound, &bytes))
    }

    /// Waits for the next chunk (≤ ~10 ms of kernel time with the default target) and reports
    /// it. Launches are pipelined: the chunk after it is already queued when this returns
    /// [`ChunkStatus::More`], so the GPU does not idle between calls.
    pub fn run_chunk(&mut self) -> Result<Chunk, GpuError> {
        let (rc, info) = self.raw.run_chunk();
        let status = match rc {
            code::OK => ChunkStatus::More,
            code::DONE => ChunkStatus::Done,
            code::ABORTED => ChunkStatus::Aborted,
            other => return Err(GpuError::from_code(other)),
        };
        Ok(Chunk {
            status,
            tile_begin: info.tile_begin,
            tile_end: info.tile_end,
            tiles_total: info.tiles_total,
            ctas: info.ctas,
            kernel: Duration::from_secs_f64(f64::from(info.ms.max(0.0)) / 1e3),
        })
    }

    /// Runs chunks until the attempt is done or aborted.
    pub fn run_attempt(&mut self) -> Result<AttemptStats, GpuError> {
        let start = Instant::now();
        let mut stats = AttemptStats {
            completed: false,
            chunks: 0,
            kernel: Duration::ZERO,
            max_chunk: Duration::ZERO,
            wall: Duration::ZERO,
        };
        loop {
            let c = self.run_chunk()?;
            if c.ctas > 0 {
                stats.chunks += 1;
                stats.kernel += c.kernel;
                stats.max_chunk = stats.max_chunk.max(c.kernel);
            }
            match c.status {
                ChunkStatus::More => continue,
                ChunkStatus::Done => stats.completed = true,
                ChunkStatus::Aborted => {}
            }
            break;
        }
        stats.wall = start.elapsed();
        Ok(stats)
    }

    /// Hits of the current attempt (at most the ring capacity) and the total count found; the
    /// order is the order in which CTAs found them, not the reference order. The list is complete
    /// once [`Job::run_chunk`] returned [`ChunkStatus::Done`]; before that, hits of the chunk that is
    /// still running may be missing.
    pub fn hits(&mut self) -> Result<(Vec<Hit>, u32), GpuError> {
        let mut raw = vec![ffi::HitRaw::default(); self.hit_capacity as usize];
        let (rc, total) = self.raw.read_hits(&mut raw);
        check(rc)?;
        raw.truncate(total.min(self.hit_capacity) as usize);
        Ok((
            raw.into_iter()
                .map(|h| Hit {
                    t_rows: h.t_rows,
                    t_cols: h.t_cols,
                    digest: h.digest,
                })
                .collect(),
            total,
        ))
    }

    /// Dump-mode records as raw bytes (m·n/128 × 104, reference order).
    pub fn dump_bytes(&mut self) -> Result<Vec<u8>, GpuError> {
        if !self.dump {
            return Err(GpuError::NotDump);
        }
        let len = self.m as usize * self.n as usize / 128 * DUMP_RECORD_LEN;
        let mut out = vec![0u8; len];
        check(self.raw.read_dump(&mut out))?;
        Ok(out)
    }

    /// Dump-mode records, parsed.
    pub fn dump_records(&mut self) -> Result<Vec<TileRecord>, GpuError> {
        let bytes = self.dump_bytes()?;
        Ok(bytes
            .as_chunks::<DUMP_RECORD_LEN>()
            .0
            .iter()
            .map(TileRecord::from_bytes)
            .collect())
    }

    /// Copies a debug buffer (see [`Buffer`]).
    pub fn read_buffer(&mut self, which: Buffer) -> Result<Vec<u8>, GpuError> {
        let (m, n, k) = (self.m as usize, self.n as usize, self.k as usize);
        let len = match which {
            Buffer::ANoised | Buffer::ANoise | Buffer::ABase => m * k,
            Buffer::BtNoised | Buffer::BtNoise | Buffer::BtBase => n * k,
            Buffer::AFactor => m * 128,
            Buffer::BtFactor => n * 128,
            Buffer::APairs | Buffer::BPairs => 2 * k,
        };
        let mut out = vec![0u8; len];
        check(self.raw.read_buffer(which.raw(), &mut out))?;
        Ok(out)
    }

    pub fn info(&self) -> Result<JobInfo, GpuError> {
        let (rc, i) = self.raw.info();
        check(rc)?;
        Ok(JobInfo {
            device_bytes: i.device_bytes,
            tiles_m: i.tiles_m,
            tiles_n: i.tiles_n,
            k_slices: i.k_slices,
            ctas: i.ctas,
            chunk_tiles: i.chunk_tiles,
            regs_per_thread: i.regs_per_thread,
            local_bytes: i.local_bytes,
            smem_bytes: i.smem_bytes,
            threads: i.threads,
        })
    }

    /// The job's cancellation flag.
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    /// Credited multiply-accumulates of one full attempt (m·n·k).
    pub fn credited_macs(&self) -> u64 {
        u64::from(self.m) * u64::from(self.n) * u64::from(self.k)
    }

    pub fn shape(&self) -> (u32, u32, u32) {
        (self.m, self.n, self.k)
    }
}

/// Bytes the job would hold on the device (the same formula `spm_job_create` checks against the
/// budget).
pub fn job_device_bytes(
    m: u32,
    n: u32,
    k: u32,
    host_operands: bool,
    dump: bool,
    hit_capacity: u32,
) -> u64 {
    let (m, n, k) = (u64::from(m), u64::from(n), u64::from(k));
    let hits = u64::from(if hit_capacity == 0 {
        4096
    } else {
        hit_capacity
    });
    m * k
        + n * k
        + if host_operands { m * k } else { 0 }
        + (m + n) * 128
        + 4 * k
        + 4096
        + if dump {
            m * n / 128 * DUMP_RECORD_LEN as u64
        } else {
            0
        }
        + hits * 40
        + 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_job_fits_the_budget() {
        // 131072^2 x 4096 with generated operands: A' + B'ᵀ + factors + pairs + prefix + hit ring
        // + counters, exactly what spm_job_create allocates.
        let bytes = job_device_bytes(131_072, 131_072, 4096, false, false, 0);
        assert_eq!(bytes, 1_107_480_608);
        assert!(bytes < DEFAULT_MEM_BUDGET);
        assert!(job_device_bytes(131_072, 131_072, 4096, true, false, 0) < DEFAULT_MEM_BUDGET);
    }

    #[test]
    fn tile_record_parses_little_endian_fields() {
        let mut b = [0u8; DUMP_RECORD_LEN];
        b[0..4].copy_from_slice(&7u32.to_le_bytes());
        b[4..8].copy_from_slice(&66u32.to_le_bytes());
        for i in 0..16u32 {
            let at = 8 + 4 * i as usize;
            b[at..at + 4].copy_from_slice(&(0x0101_0101 * i).to_le_bytes());
        }
        for (i, x) in b[72..].iter_mut().enumerate() {
            *x = i as u8;
        }
        let r = TileRecord::from_bytes(&b);
        assert_eq!((r.t_rows, r.t_cols), (7, 66));
        assert_eq!(r.transcript[15], 0x0f0f_0f0f);
        assert_eq!(r.digest[31], 31);
    }

    #[test]
    fn host_operands_are_size_checked_before_touching_the_gpu() {
        let a = vec![0i8; 64 * 2048];
        let bt = vec![0i8; 64 * 2048 - 1];
        let cfg = JobConfig::new(64, 64, 2048, Operands::Host { a: &a, bt: &bt }, [0; 32]);
        assert_eq!(Job::new(&cfg).unwrap_err(), GpuError::Size);
    }
}
