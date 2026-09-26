//! Safe mining-job API over the C ABI: B/A side prep, chunked fused GEMM, hits and dumps.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicU32;

use crate::ffi;

/// Size of one dump record (`spm_cpuref::TileResult::dump_bytes`).
pub const RECORD_LEN: usize = 104;
/// Words of a tile transcript.
pub const TRANSCRIPT_WORDS: usize = 16;
/// Default device-memory budget of a job (the worker's fixed budget).
pub const DEFAULT_MEM_BUDGET: u64 = 2 << 30;

/// A failed library call.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("{call}: {status} (status {code}, CUDA error {cuda_error}: {cuda_message})")]
    Status {
        call: &'static str,
        code: i32,
        status: &'static str,
        cuda_error: i32,
        cuda_message: &'static str,
    },
    #[error("{0}")]
    Invalid(String),
}

impl GpuError {
    fn status(call: &'static str, code: i32) -> Self {
        let (cuda_error, cuda_message) = ffi::last_cuda_error();
        Self::Status {
            call,
            code,
            status: ffi::status_str(code),
            cuda_error,
            cuda_message,
        }
    }

    /// The library status code, if this error came from a library call.
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Status { code, .. } => Some(*code),
            Self::Invalid(_) => None,
        }
    }
}

fn check(call: &'static str, code: i32) -> Result<i32, GpuError> {
    if code < 0 {
        Err(GpuError::status(call, code))
    } else {
        Ok(code)
    }
}

/// Origin of the committed matrices A (m×k) and Bᵀ (n×k).
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    /// `spm_cpuref::fill_int7(seed, DOMAIN_A)` / `(seed, DOMAIN_BT)`, i.e. `Problem::generate`,
    /// generated directly on the GPU (nothing is uploaded).
    Fill { seed: u64 },
    /// Caller-supplied matrices, row major; copied to the device during [`Job::create`].
    Host { a: &'a [i8], bt: &'a [i8] },
}

/// Everything [`Job::create`] needs.
#[derive(Clone, Debug)]
pub struct JobParams<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    /// `IncompleteBlockHeader::to_bytes()`.
    pub header76: [u8; 76],
    /// `MiningConfiguration::to_bytes()`; must be our configuration for `k`.
    pub config52: [u8; 52],
    pub source: Source<'a>,
    /// From the CPU commitment (`Commitment::b_noise_seed`).
    pub b_noise_seed: [u8; 32],
    /// Initial difficulty bound, little-endian U256; a tile hits when `digest <= bound`.
    pub bound: [u8; 32],
    /// Every tile writes its 104-byte record (G0 / debugging) instead of mining.
    pub dump: bool,
    /// Hit-ring records (0 = library default, 4096).
    pub hit_capacity: u32,
    /// CTA tiles per launch chunk (0 = adaptive, ~8 ms per chunk).
    pub chunk_ctas: u32,
    /// Device-memory budget (0 = 2 GiB).
    pub mem_budget_bytes: u64,
}

/// One tile's transcript and digest as dumped by the GPU (same fields and byte layout as
/// `spm_cpuref::TileResult`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileRecord {
    pub t_rows: u32,
    pub t_cols: u32,
    pub transcript: [u32; TRANSCRIPT_WORDS],
    pub digest: [u8; 32],
}

impl TileRecord {
    pub fn from_bytes(b: &[u8; RECORD_LEN]) -> Self {
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut transcript = [0u32; TRANSCRIPT_WORDS];
        for (j, w) in transcript.iter_mut().enumerate() {
            *w = word(8 + 4 * j);
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&b[8 + 4 * TRANSCRIPT_WORDS..]);
        Self {
            t_rows: word(0),
            t_cols: word(4),
            transcript,
            digest,
        }
    }

    pub fn to_bytes(&self) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        out[0..4].copy_from_slice(&self.t_rows.to_le_bytes());
        out[4..8].copy_from_slice(&self.t_cols.to_le_bytes());
        for (j, w) in self.transcript.iter().enumerate() {
            out[8 + 4 * j..12 + 4 * j].copy_from_slice(&w.to_le_bytes());
        }
        out[8 + 4 * TRANSCRIPT_WORDS..].copy_from_slice(&self.digest);
        out
    }
}

/// A tile whose digest met the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hit {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// Hits drained from the ring by [`Job::read_hits`].
#[derive(Clone, Debug, Default)]
pub struct Hits {
    pub hits: Vec<Hit>,
    /// Hits overwritten in the ring before they were read (the ring was too small).
    pub lost: u32,
}

/// Outcome of [`Job::run_chunk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunk {
    More,
    Done,
}

/// Outcome of [`Job::run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    Done,
    /// The abort flag was seen between chunks; calling `run` again resumes the attempt.
    Aborted,
}

/// Debug read-backs (`SPM_DEBUG_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugBuffer {
    /// A' (m×k).
    NoisedA = 0,
    /// B'ᵀ (n×k).
    NoisedBt = 1,
    /// A_L (m×128).
    AL = 2,
    /// B_Rᵀ (n×128).
    BRt = 3,
    /// A_R pairs (k × (p, q)).
    PairsA = 4,
    /// B_L pairs (k × (p, q)).
    PairsB = 5,
}

/// Snapshot of a job's state (`spm_job_info_t`).
#[derive(Clone, Copy, Debug)]
pub struct JobInfo {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub slices: u32,
    pub tiles: u64,
    pub cta_tiles: u64,
    pub next_cta: u64,
    pub device_bytes: u64,
    pub chunk_ctas: u32,
    pub ctas_per_sm: u32,
    pub last_chunk_ms: f32,
    pub last_prep_ms: f32,
    pub b_prep_ms: f32,
    pub hits_total: u32,
    /// `blake3(header76 ‖ config52)` as computed by the library.
    pub job_key: [u8; 32],
    pub cuda_error: i32,
    pub smem_bytes: u32,
    /// GPU time of the computed chunks of the current attempt.
    pub attempt_gpu_ms: f32,
    /// Longest chunk of the current attempt.
    pub attempt_max_chunk_ms: f32,
    /// Computed chunks of the current attempt.
    pub attempt_chunks: u32,
}

/// A mining job living on the GPU. Drop frees every device buffer.
pub struct Job {
    handle: ffi::JobHandle,
    m: u32,
    n: u32,
    k: u32,
    dump: bool,
}

impl Job {
    /// Allocates the job and builds the B side (B_Rᵀ, B pairs, B'ᵀ).
    pub fn create(p: &JobParams<'_>) -> Result<Self, GpuError> {
        let source = match p.source {
            Source::Fill { seed } => ffi::SourceArg::Fill(seed),
            Source::Host { a, bt } => {
                let mk = u64::from(p.m) * u64::from(p.k);
                let nk = u64::from(p.n) * u64::from(p.k);
                if a.len() as u64 != mk || bt.len() as u64 != nk {
                    return Err(GpuError::Invalid(format!(
                        "host matrices must hold m*k = {mk} and n*k = {nk} entries (got {} and {})",
                        a.len(),
                        bt.len()
                    )));
                }
                ffi::SourceArg::Host { a, bt }
            }
        };
        let args = ffi::CreateArgs {
            m: p.m,
            n: p.n,
            k: p.k,
            dump: p.dump,
            source,
            header76: p.header76,
            config52: p.config52,
            b_noise_seed: p.b_noise_seed,
            bound: p.bound,
            hit_capacity: p.hit_capacity,
            chunk_ctas: p.chunk_ctas,
            mem_budget_bytes: p.mem_budget_bytes,
        };
        let handle = ffi::JobHandle::create(&args)
            .map_err(|code| GpuError::status("spm_job_create", code))?;
        Ok(Self {
            handle,
            m: p.m,
            n: p.n,
            k: p.k,
            dump: p.dump,
        })
    }

    /// Overrides entries `[offset, offset + bytes.len())` of A (at most 4096 bytes, e.g. the
    /// nonce in chunk 0). Takes effect at the next [`Job::set_attempt`].
    pub fn patch_a(&mut self, offset: u64, bytes: &[i8]) -> Result<(), GpuError> {
        check("spm_job_patch_a", self.handle.patch_a(offset, bytes)).map(|_| ())
    }

    /// Builds the A side (A_L, A pairs, A') for `a_noise_seed`, resets the hit ring and the chunk
    /// cursor. `bound: None` keeps the current bound.
    pub fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound: Option<&[u8; 32]>,
    ) -> Result<(), GpuError> {
        check(
            "spm_job_set_attempt",
            self.handle.set_attempt(a_noise_seed, bound),
        )
        .map(|_| ())
    }

    /// Runs one launch chunk and waits for it.
    pub fn run_chunk(&mut self) -> Result<Chunk, GpuError> {
        match check("spm_job_run_chunk", self.handle.run_chunk())? {
            ffi::SPM_CHUNK_MORE => Ok(Chunk::More),
            ffi::SPM_CHUNK_DONE => Ok(Chunk::Done),
            other => Err(GpuError::Invalid(format!(
                "unexpected chunk status {other}"
            ))),
        }
    }

    /// Runs chunks until the attempt is complete, checking `abort` (if any) before every chunk:
    /// an abort takes effect within the running chunk, and calling `run` again resumes exactly
    /// where the attempt stopped.
    pub fn run(&mut self, abort: Option<&AtomicU32>) -> Result<Run, GpuError> {
        match check("spm_job_run", self.handle.run(abort))? {
            ffi::SPM_OK => Ok(Run::Done),
            ffi::SPM_ABORTED => Ok(Run::Aborted),
            other => Err(GpuError::Invalid(format!("unexpected run status {other}"))),
        }
    }

    /// Drains the hits recorded since the previous call (mining mode).
    pub fn read_hits(&mut self) -> Result<Hits, GpuError> {
        let mut out = Hits::default();
        let mut buf = vec![ffi::HitRaw::default(); 1024];
        loop {
            let (rc, n, lost) = self.handle.read_hits(&mut buf);
            check("spm_job_read_hits", rc)?;
            out.lost += lost;
            out.hits.extend(buf[..n as usize].iter().map(|h| Hit {
                t_rows: h.t_rows,
                t_cols: h.t_cols,
                digest: h.digest,
            }));
            if (n as usize) < buf.len() {
                return Ok(out);
            }
        }
    }

    /// Every tile's record in the reference order (dump mode).
    pub fn read_dump(&mut self) -> Result<Vec<TileRecord>, GpuError> {
        if !self.dump {
            return Err(GpuError::Invalid(
                "read_dump needs a job created with dump = true".into(),
            ));
        }
        let tiles = u64::from(self.m) * u64::from(self.n) / 128;
        let len = usize::try_from(tiles * RECORD_LEN as u64)
            .map_err(|_| GpuError::Invalid("dump does not fit in memory".into()))?;
        let mut bytes = vec![0u8; len];
        check("spm_job_read_dump", self.handle.read_dump(&mut bytes))?;
        Ok(bytes
            .as_chunks::<RECORD_LEN>()
            .0
            .iter()
            .map(TileRecord::from_bytes)
            .collect())
    }

    /// Copies one of the intermediate device buffers.
    pub fn read_debug(&mut self, which: DebugBuffer) -> Result<Vec<u8>, GpuError> {
        let (m, n, k) = (u64::from(self.m), u64::from(self.n), u64::from(self.k));
        let len = match which {
            DebugBuffer::NoisedA => m * k,
            DebugBuffer::NoisedBt => n * k,
            DebugBuffer::AL => m * 128,
            DebugBuffer::BRt => n * 128,
            DebugBuffer::PairsA | DebugBuffer::PairsB => 2 * k,
        };
        let len = usize::try_from(len).map_err(|_| GpuError::Invalid("buffer too large".into()))?;
        let mut out = vec![0u8; len];
        check(
            "spm_job_read_debug",
            self.handle.read_debug(which as i32, &mut out),
        )?;
        Ok(out)
    }

    pub fn info(&self) -> Result<JobInfo, GpuError> {
        let (rc, r) = self.handle.info();
        check("spm_job_get_info", rc)?;
        Ok(JobInfo {
            m: r.m,
            n: r.n,
            k: r.k,
            slices: r.slices,
            tiles: r.tiles,
            cta_tiles: r.cta_tiles,
            next_cta: r.next_cta,
            device_bytes: r.device_bytes,
            chunk_ctas: r.chunk_ctas,
            ctas_per_sm: r.ctas_per_sm,
            last_chunk_ms: r.last_chunk_ms,
            last_prep_ms: r.last_prep_ms,
            b_prep_ms: r.b_prep_ms,
            hits_total: r.hits_total,
            job_key: r.job_key,
            cuda_error: r.cuda_error,
            smem_bytes: r.smem_bytes,
            attempt_gpu_ms: r.attempt_gpu_ms,
            attempt_max_chunk_ms: r.attempt_max_chunk_ms,
            attempt_chunks: r.attempt_chunks,
        })
    }
}

/// Human-readable text of a library status code.
pub fn status_text(code: i32) -> &'static str {
    ffi::status_str(code)
}

/// Description of a CUDA error code.
pub fn cuda_error_text(code: i32) -> &'static str {
    ffi::cuda_error_str(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let mut b = [0u8; RECORD_LEN];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i * 7 + 3) as u8;
        }
        let r = TileRecord::from_bytes(&b);
        assert_eq!(r.to_bytes(), b);
        assert_eq!(r.t_rows, u32::from_le_bytes([3, 10, 17, 24]));
    }

    #[test]
    fn invalid_calls_are_status_codes_not_panics() {
        // Library-side validation only: none of these reaches the GPU.
        let p = JobParams {
            m: 100, // not a multiple of 128
            n: 128,
            k: 2048,
            header76: [0; 76],
            config52: [0; 52],
            source: Source::Fill { seed: 1 },
            b_noise_seed: [0; 32],
            bound: [0; 32],
            dump: false,
            hit_capacity: 0,
            chunk_ctas: 0,
            mem_budget_bytes: 0,
        };
        let err = Job::create(&p).err().expect("shape must be refused");
        assert_eq!(err.code(), Some(-2), "{err}");
        let bad_host = JobParams {
            m: 128,
            source: Source::Host {
                a: &[0; 3],
                bt: &[0; 3],
            },
            ..p
        };
        assert!(matches!(Job::create(&bad_host), Err(GpuError::Invalid(_))));
        assert_eq!(status_text(-3), "device-memory budget exceeded");
    }
}
