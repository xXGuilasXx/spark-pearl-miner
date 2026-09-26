//! Safe job API over the fused v0 kernel: one job per (header, A, Bᵀ), one attempt per A-side
//! noise seed, chunked launches, hit ring and debug dump.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicU32;

use crate::ffi::{self, CreateArgs, HitRaw, JobHandle};

/// Bytes of one dump record (`spm_cpuref::TileResult::dump_bytes`).
pub const DUMP_RECORD_BYTES: usize = ffi::SPM_DUMP_RECORD_BYTES;
/// Device-memory budget of one job.
pub const JOB_DEVICE_BUDGET_BYTES: u64 = ffi::SPM_JOB_DEVICE_BUDGET_BYTES;

/// Why a call into libspm_cuda failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A required argument was missing (should not happen through this API).
    Null,
    /// m, n or k outside what the kernel and the consensus rules accept.
    Shape,
    /// `config52` is not the configuration the kernel implements.
    Config,
    /// The job would exceed the 2 GiB device-memory budget.
    Budget,
    /// A CUDA call failed (code in [`GpuError::cuda_error`]).
    Cuda,
    /// Call out of order, e.g. running before [`Job::set_attempt`].
    State,
    /// A value, offset or length out of range (also: host entries outside [-64, 64]).
    Range,
    /// The dump was requested from a job created without `dump`.
    NoDump,
    /// Unexpected failure inside the library.
    Internal,
    /// A status code this wrapper does not know.
    Unknown(i32),
}

impl ErrorKind {
    fn from_code(code: i32) -> Self {
        match code {
            ffi::SPM_ERR_NULL => Self::Null,
            ffi::SPM_ERR_SHAPE => Self::Shape,
            ffi::SPM_ERR_CONFIG => Self::Config,
            ffi::SPM_ERR_BUDGET => Self::Budget,
            ffi::SPM_ERR_CUDA => Self::Cuda,
            ffi::SPM_ERR_STATE => Self::State,
            ffi::SPM_ERR_RANGE => Self::Range,
            ffi::SPM_ERR_NO_DUMP => Self::NoDump,
            ffi::SPM_ERR_INTERNAL => Self::Internal,
            other => Self::Unknown(other),
        }
    }
}

/// A failed libspm_cuda call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{op}: {message}")]
pub struct GpuError {
    pub op: &'static str,
    pub kind: ErrorKind,
    /// CUDA error code when `kind == Cuda`.
    pub cuda_error: Option<i32>,
    pub message: String,
}

impl GpuError {
    fn new(op: &'static str, code: i32, cuda: i32) -> Self {
        let kind = ErrorKind::from_code(code);
        let mut message = ffi::status_string(code);
        let cuda_error = (kind == ErrorKind::Cuda).then_some(cuda);
        if let Some(c) = cuda_error {
            message = format!("{message} {c} ({})", ffi::cuda_error_string(c));
        }
        Self {
            op,
            kind,
            cuda_error,
            message,
        }
    }
}

/// Where the job's A and Bᵀ come from.
#[derive(Debug, Clone, Copy)]
pub enum Matrices<'a> {
    /// `spm_cpuref::fill_int7(seed, DOMAIN_A / DOMAIN_BT)` generated on the GPU (no upload).
    Generated { seed: u64 },
    /// Host matrices, row major: `a` is m×k, `bt` is n×k, entries in [-64, 64].
    Host { a: &'a [i8], bt: &'a [i8] },
}

/// Everything [`Job::new`] needs. The commitment (job key, Merkle roots, V3 salting, seed chain)
/// is computed on the host in v0; the GPU only needs the resulting noise seeds.
#[derive(Debug, Clone)]
pub struct JobParams<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    /// `MiningConfiguration::to_bytes()`; must be our configuration (r = 128, int7, 8×16
    /// pattern, no MoE) for this k.
    pub config52: [u8; 52],
    pub matrices: Matrices<'a>,
    /// `blake3(job_key ‖ bound_b)`.
    pub b_noise_seed: [u8; 32],
    /// Difficulty bound as a little-endian U256 (a tile hits when its digest is ≤ bound).
    pub bound: [u8; 32],
    /// Debug dump: every tile writes its 104-byte record.
    pub dump: bool,
    /// CTA tiles per launch chunk; `None` = automatic: about 4 ms per chunk, re-derived from the
    /// measured rate after every chunk (the cancellation limit is 10 ms).
    pub chunk_ctas: Option<u32>,
    /// Hit-ring capacity; `None` = 4096.
    pub hit_capacity: Option<u32>,
}

/// Outcome of [`Job::run_chunk`] / [`Job::run_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStatus {
    /// More chunks remain in this attempt.
    More,
    /// The attempt is complete: hits (and the dump) are final.
    Done,
    /// The abort flag was seen between chunks. The cursor is kept, so running again resumes
    /// the attempt; a new [`Job::set_attempt`] starts over.
    Aborted,
}

/// One tile whose digest met the bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hit {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// Hits of the current attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hits {
    /// Every hit the kernel counted (may exceed the ring capacity).
    pub total: u32,
    /// The hits kept by the ring, in ring order.
    pub hits: Vec<Hit>,
}

impl Hits {
    /// Hits past the ring capacity that were counted but not stored.
    pub fn dropped(&self) -> u32 {
        self.total
            .saturating_sub(u32::try_from(self.hits.len()).unwrap_or(u32::MAX))
    }
}

/// One tile of a debug dump (the fields of `spm_cpuref::TileResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRecord {
    pub t_rows: u32,
    pub t_cols: u32,
    pub transcript: [u32; 16],
    pub digest: [u8; 32],
}

impl TileRecord {
    /// Parses one 104-byte record.
    pub fn from_bytes(b: &[u8; DUMP_RECORD_BYTES]) -> Self {
        let word =
            |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let mut transcript = [0u32; 16];
        for (i, t) in transcript.iter_mut().enumerate() {
            *t = word(2 + i);
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

/// Device buffers readable for debugging ([`Job::read_buffer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    /// A before noise (m×k).
    ABase,
    /// A' = A + E_A (m×k), valid after [`Job::set_attempt`].
    ANoised,
    /// B'ᵀ = Bᵀ + E_Bᵀ (n×k).
    BtNoised,
    /// A_L (m×128).
    AL,
    /// B_Rᵀ (n×128).
    BRt,
    /// Pairs of A_R: bytes 2l, 2l+1 = (p, q) of column l.
    APairs,
    /// Pairs of B_L: bytes 2l, 2l+1 = (p, q) of row l.
    BPairs,
}

impl Buffer {
    fn code(self) -> i32 {
        match self {
            Self::ABase => 0,
            Self::ANoised => 1,
            Self::BtNoised => 2,
            Self::AL => 3,
            Self::BRt => 4,
            Self::APairs => 5,
            Self::BPairs => 6,
        }
    }
}

/// Geometry, memory and timings of a job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JobInfo {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    /// CTA tile of the fused kernel.
    pub block: (u32, u32),
    /// Hash tiles per attempt (m·n/128).
    pub tiles: u64,
    pub cta_tiles: u32,
    pub chunk_ctas: u32,
    pub chunks: u32,
    pub ctas_per_sm: u32,
    pub sm_count: u32,
    pub smem_bytes: u32,
    /// Entries of the hit ring.
    pub hit_capacity: u32,
    pub device_bytes: u64,
    /// GPU time of the last chunk, ms.
    pub last_chunk_ms: f32,
    /// GPU time of the last A-side prep (`set_attempt`), ms.
    pub last_prep_ms: f32,
    /// GPU time of the matrix fill/upload and B-side prep at creation, ms.
    pub create_prep_ms: f32,
}

impl JobInfo {
    /// Credited multiply-accumulates of one full attempt (m·n·k).
    pub fn credited_macs(&self) -> u64 {
        u64::from(self.m) * u64::from(self.n) * u64::from(self.k)
    }
}

/// A PearlHash V3 job resident on the GPU.
pub struct Job {
    handle: JobHandle,
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
            .finish_non_exhaustive()
    }
}

impl Job {
    /// Allocates the job, fills or uploads A and Bᵀ and builds the B side (B'ᵀ).
    pub fn new(p: &JobParams<'_>) -> Result<Self, GpuError> {
        let host = match p.matrices {
            Matrices::Generated { .. } => None,
            Matrices::Host { a, bt } => Some((a, bt)),
        };
        let gen_seed = match p.matrices {
            Matrices::Generated { seed } => seed,
            Matrices::Host { .. } => 0,
        };
        let args = CreateArgs {
            m: p.m,
            n: p.n,
            k: p.k,
            config52: &p.config52,
            gen_seed,
            host,
            b_noise_seed: &p.b_noise_seed,
            bound: &p.bound,
            flags: if p.dump { ffi::SPM_JOB_DUMP } else { 0 },
            chunk_ctas: p.chunk_ctas.unwrap_or(0),
            hit_capacity: p.hit_capacity.unwrap_or(0),
        };
        let handle = JobHandle::create(&args)
            .map_err(|(rc, cuda)| GpuError::new("spm_job_create", rc, cuda))?;
        let mut job = Self {
            handle,
            m: p.m,
            n: p.n,
            k: p.k,
            dump: p.dump,
            hit_capacity: 0,
        };
        job.hit_capacity = job.info()?.hit_capacity;
        Ok(job)
    }

    fn check(&self, op: &'static str, rc: i32) -> Result<(), GpuError> {
        if rc == ffi::SPM_OK {
            Ok(())
        } else {
            Err(GpuError::new(op, rc, self.handle.last_cuda_error()))
        }
    }

    fn chunk_status(
        &self,
        op: &'static str,
        rc: i32,
        status: i32,
    ) -> Result<ChunkStatus, GpuError> {
        self.check(op, rc)?;
        match status {
            ffi::SPM_CHUNK_MORE => Ok(ChunkStatus::More),
            ffi::SPM_CHUNK_DONE => Ok(ChunkStatus::Done),
            ffi::SPM_CHUNK_ABORTED => Ok(ChunkStatus::Aborted),
            other => Err(GpuError::new(op, other, 0)),
        }
    }

    pub fn info(&self) -> Result<JobInfo, GpuError> {
        let (rc, i) = self.handle.info();
        self.check("spm_job_info", rc)?;
        Ok(JobInfo {
            m: i.m,
            n: i.n,
            k: i.k,
            block: (i.block_m, i.block_n),
            tiles: i.tiles,
            cta_tiles: i.cta_tiles,
            chunk_ctas: i.chunk_ctas,
            chunks: i.chunks,
            ctas_per_sm: i.ctas_per_sm,
            sm_count: i.sm_count,
            smem_bytes: i.smem_bytes,
            hit_capacity: i.hit_capacity,
            device_bytes: i.device_bytes,
            last_chunk_ms: i.last_chunk_ms,
            last_prep_ms: i.last_prep_ms,
            create_prep_ms: i.create_prep_ms,
        })
    }

    /// Overwrites `data.len()` entries of A starting at linear index `offset` (row major).
    /// Takes effect at the next [`Job::set_attempt`].
    pub fn patch_a(&mut self, offset: u64, data: &[i8]) -> Result<(), GpuError> {
        let rc = self.handle.patch_a(offset, data);
        self.check("spm_job_patch_a", rc)
    }

    /// Builds the A side for `a_noise_seed` (also the jackpot key) and resets the hit ring and
    /// the chunk cursor. `bound` replaces the job's bound when given.
    pub fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound: Option<&[u8; 32]>,
    ) -> Result<(), GpuError> {
        let rc = self.handle.set_attempt(a_noise_seed, bound);
        self.check("spm_job_set_attempt", rc)
    }

    /// Runs the next chunk synchronously.
    pub fn run_chunk(&mut self) -> Result<ChunkStatus, GpuError> {
        let (rc, status) = self.handle.run_chunk();
        self.chunk_status("spm_job_run_chunk", rc, status)
    }

    /// Runs the rest of the attempt, two chunks in flight, checking `abort` before each chunk.
    /// Another thread may store a non-zero value into `abort` to stop it.
    pub fn run_attempt(&mut self, abort: &AtomicU32) -> Result<ChunkStatus, GpuError> {
        let (rc, status) = self.handle.run_attempt(abort);
        self.chunk_status("spm_job_run_attempt", rc, status)
    }

    /// Runs the rest of the attempt with no way to abort it.
    pub fn run_to_completion(&mut self) -> Result<(), GpuError> {
        let never = AtomicU32::new(0);
        match self.run_attempt(&never)? {
            ChunkStatus::Done => Ok(()),
            other => Err(GpuError {
                op: "spm_job_run_attempt",
                kind: ErrorKind::Internal,
                cuda_error: None,
                message: format!("attempt ended with {other:?} without an abort"),
            }),
        }
    }

    /// Hits of the current attempt (ring order).
    pub fn hits(&mut self) -> Result<Hits, GpuError> {
        let (rc, total) = self.handle.read_hits(&mut []);
        self.check("spm_job_read_hits", rc)?;
        let stored = (total as usize).min(self.hit_capacity as usize);
        let mut raw = vec![HitRaw::default(); stored];
        let (rc, total) = self.handle.read_hits(&mut raw);
        self.check("spm_job_read_hits", rc)?;
        Ok(Hits {
            total,
            hits: raw
                .into_iter()
                .map(|h| Hit {
                    t_rows: h.t_rows,
                    t_cols: h.t_cols,
                    digest: h.digest,
                })
                .collect(),
        })
    }

    /// The raw dump of the current attempt (tiles × 104 bytes, reference tile order).
    pub fn dump(&mut self) -> Result<Vec<u8>, GpuError> {
        if !self.dump {
            return Err(GpuError::new("spm_job_read_dump", ffi::SPM_ERR_NO_DUMP, 0));
        }
        let tiles = u64::from(self.m) * u64::from(self.n) / 128;
        let len = usize::try_from(tiles * DUMP_RECORD_BYTES as u64)
            .map_err(|_| GpuError::new("spm_job_read_dump", ffi::SPM_ERR_RANGE, 0))?;
        let mut out = vec![0u8; len];
        let (rc, written) = self.handle.read_dump(&mut out);
        self.check("spm_job_read_dump", rc)?;
        out.truncate(usize::try_from(written).unwrap_or(0));
        Ok(out)
    }

    /// The dump parsed into records.
    pub fn dump_records(&mut self) -> Result<Vec<TileRecord>, GpuError> {
        let raw = self.dump()?;
        let (records, _) = raw.as_chunks::<DUMP_RECORD_BYTES>();
        Ok(records.iter().map(TileRecord::from_bytes).collect())
    }

    /// Copies `len` bytes at `offset` of a device buffer.
    pub fn read_buffer(
        &mut self,
        which: Buffer,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, GpuError> {
        let mut out = vec![0u8; len];
        let rc = self.handle.read_buffer(which.code(), offset, &mut out);
        self.check("spm_job_read_buffer", rc)?;
        Ok(out)
    }

    /// Copies a whole device buffer.
    pub fn read_whole_buffer(&mut self, which: Buffer) -> Result<Vec<u8>, GpuError> {
        let (m, n, k) = (u64::from(self.m), u64::from(self.n), u64::from(self.k));
        let len = match which {
            Buffer::ABase | Buffer::ANoised => m * k,
            Buffer::BtNoised => n * k,
            Buffer::AL => m * 128,
            Buffer::BRt => n * 128,
            Buffer::APairs | Buffer::BPairs => 2 * k,
        };
        let len = usize::try_from(len)
            .map_err(|_| GpuError::new("spm_job_read_buffer", ffi::SPM_ERR_RANGE, 0))?;
        self.read_buffer(which, 0, len)
    }
}

/// Keyed BLAKE3 of one 64-byte block computed by the device code (self-test of blake3.cuh).
pub fn debug_blake3_keyed64(key: &[u8; 32], msg: &[u8; 64]) -> Result<[u8; 32], GpuError> {
    let mut out = [0u8; 32];
    let rc = ffi::debug_blake3_keyed64(key, msg, &mut out);
    if rc == ffi::SPM_OK {
        Ok(out)
    } else {
        Err(GpuError::new(
            "spm_debug_blake3_keyed64",
            rc,
            ffi::last_create_cuda_error(),
        ))
    }
}

/// `MiningConfiguration::to_bytes()` of our configuration for common dimension `k`
/// (r = 128, Int7xInt7ToInt32, rows {0, 8, …, 56}, cols {0, 1, 8, 9, …, 56, 57}, no MoE); the
/// only configuration the kernel accepts.
pub fn config52_for(k: u32) -> [u8; 52] {
    let mut c = [0u8; 52];
    c[0..4].copy_from_slice(&k.to_le_bytes());
    c[4..6].copy_from_slice(&128u16.to_le_bytes());
    c[8..14].copy_from_slice(&[0x07, 0x07, 0, 0, 0, 0]);
    c[14..20].copy_from_slice(&[0x00, 0x01, 0x03, 0x07, 0, 0]);
    c
}
