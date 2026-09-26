//! The compute engine behind the worker loop: the job ABI of libspm_cuda (create / set attempt /
//! run chunk / read hits / dump / destroy, plus the abort word), as a trait so the same loop runs
//! over the GPU ([`crate::gpu::GpuEngine`]) or the CPU reference ([`crate::cpu::CpuEngine`]).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use spm_cpuref::TileResult;
use spm_ipc::FaultKind;

/// The cancellation word shared between the control threads and the running chunks. Setting it
/// stops the chunk in flight at its next tile; the next `run_chunk` re-runs that chunk once it is
/// cleared.
pub trait AbortFlag: Send + Sync {
    fn set(&self);
    fn clear(&self);
    fn is_set(&self) -> bool;
}

/// What one job needs on the device: the shape, the fill seed of A and Bᵀ and the B noise seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobSpec {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    /// `A = fill_int7(gen_seed, DOMAIN_A)`, `Bᵀ = fill_int7(gen_seed, DOMAIN_BT)`.
    pub gen_seed: u64,
    pub b_noise_seed: [u8; 32],
    /// One transcript record per hash tile (known-answer test only).
    pub dump: bool,
    /// Hit ring entries (0 = the engine's default).
    pub hit_capacity: u32,
}

impl JobSpec {
    /// Credited multiply-accumulates of one full attempt.
    pub fn credited_macs(&self) -> u64 {
        u64::from(self.m) * u64::from(self.n) * u64::from(self.k)
    }

    /// Hash tiles of one attempt (8 × 16 outputs each).
    pub fn hash_tiles(&self) -> u64 {
        u64::from(self.m) * u64::from(self.n) / 128
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStatus {
    /// More chunks remain; one may still be queued on the device.
    More,
    /// Every tile of the attempt was evaluated; nothing is queued.
    Done,
    /// The abort word stopped the chunk; nothing is queued.
    Aborted,
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkReport {
    pub status: ChunkStatus,
    /// Device time of the chunk.
    pub kernel: Duration,
}

/// A hash tile whose digest met the attempt's bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hit {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// An engine failure, classified for the `Fault` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub kind: FaultKind,
    pub msg: String,
}

impl EngineError {
    pub fn new(kind: FaultKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            msg: msg.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.msg)
    }
}

impl std::error::Error for EngineError {}

/// The job ABI. One job at a time; every call is made from the worker's mining thread.
pub trait Engine: Send {
    /// Name reported in `Ready` (e.g. `NVIDIA GB10 (sm_121, 48 SMs)`).
    fn device(&self) -> String;
    /// The abort word the job's chunks poll.
    fn abort_flag(&self) -> Arc<dyn AbortFlag>;
    /// Allocates the job and builds the B side (B'ᵀ). Replaces any previous job.
    fn create_job(&mut self, spec: &JobSpec) -> Result<(), EngineError>;
    /// Frees the job (device memory); a no-op without one.
    fn destroy_job(&mut self);
    /// Builds A' for `a_noise_seed` with the first `prefix.len()` entries of A replaced by
    /// `prefix` (the nonce patch), sets the bound (LE U256) and rewinds to the first tile.
    fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound_le: &[u8; 32],
        prefix: &[i8],
    ) -> Result<(), EngineError>;
    /// Waits for the next chunk of the attempt.
    fn run_chunk(&mut self) -> Result<ChunkReport, EngineError>;
    /// Hits of the attempt so far and the total found (it can exceed the ring capacity).
    fn hits(&mut self) -> Result<(Vec<Hit>, u32), EngineError>;
    /// Every tile's record in reference order (dump-mode jobs only).
    fn dump(&mut self) -> Result<Vec<TileResult>, EngineError>;
}
