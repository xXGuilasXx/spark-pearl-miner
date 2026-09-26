//! The GPU engine: [`Engine`] over `spm_gpu::Job` (libspm_cuda, sm_121a).

use std::sync::Arc;

use spm_cpuref::TileResult;
use spm_gpu::{AbortHandle, ChunkStatus as GpuStatus, GpuError, Job, JobConfig, Operands};
use spm_ipc::FaultKind;

use crate::engine::{AbortFlag, ChunkReport, ChunkStatus, Engine, EngineError, Hit, JobSpec};

struct GpuAbort(AbortHandle);

impl AbortFlag for GpuAbort {
    fn set(&self) {
        self.0.set();
    }
    fn clear(&self) {
        self.0.clear();
    }
    fn is_set(&self) -> bool {
        self.0.is_set()
    }
}

fn gpu_err(what: &str, e: GpuError) -> EngineError {
    let kind = match e {
        GpuError::OutOfMemory { .. } | GpuError::Budget => FaultKind::OutOfMemory,
        GpuError::Cuda { .. } | GpuError::Tma | GpuError::Unknown(..) => FaultKind::Cuda,
        GpuError::Invalid
        | GpuError::Shape
        | GpuError::NoAttempt
        | GpuError::NotDump
        | GpuError::Size => FaultKind::Other,
    };
    EngineError::new(kind, format!("{what}: {e}"))
}

/// The device engine. Creating it creates the process's CUDA context (the abort word is
/// host-mapped memory).
pub struct GpuEngine {
    abort: AbortHandle,
    flag: Arc<GpuAbort>,
    job: Option<Job>,
    device: String,
}

impl GpuEngine {
    pub fn new() -> Result<Self, EngineError> {
        let info =
            spm_gpu::device_info().map_err(|e| EngineError::new(FaultKind::Cuda, e.to_string()))?;
        let abort = AbortHandle::new().map_err(|e| gpu_err("abort word", e))?;
        let device = format!(
            "{} (sm_{}{}, {} SMs, {:.0} GiB)",
            info.name,
            info.compute_capability.0,
            info.compute_capability.1,
            info.sm_count,
            info.total_mem_bytes as f64 / f64::from(1u32 << 30)
        );
        Ok(Self {
            flag: Arc::new(GpuAbort(abort.clone())),
            abort,
            job: None,
            device,
        })
    }

    fn job(&mut self) -> Result<&mut Job, EngineError> {
        self.job
            .as_mut()
            .ok_or_else(|| EngineError::new(FaultKind::Other, "no job"))
    }
}

impl Engine for GpuEngine {
    fn device(&self) -> String {
        self.device.clone()
    }

    fn abort_flag(&self) -> Arc<dyn AbortFlag> {
        self.flag.clone()
    }

    fn create_job(&mut self, spec: &JobSpec) -> Result<(), EngineError> {
        // Free the previous job first: two default-shape jobs would not fit the 2 GiB budget.
        self.job = None;
        let mut cfg = JobConfig::new(
            spec.m,
            spec.n,
            spec.k,
            Operands::Generated {
                seed: spec.gen_seed,
            },
            spec.b_noise_seed,
        );
        cfg.dump = spec.dump;
        cfg.hit_capacity = spec.hit_capacity;
        // Chunk size: the library's adaptive default (4.5 ms target, capped at ~8 ms of work at
        // an 1800 MHz clock), which keeps every chunk under the 10 ms cancellation rule.
        cfg.abort = Some(self.abort.clone());
        self.job = Some(Job::new(&cfg).map_err(|e| gpu_err("job create", e))?);
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
        self.job()?
            .set_attempt_with_prefix(a_noise_seed, bound_le, prefix)
            .map_err(|e| gpu_err("set attempt", e))
    }

    fn run_chunk(&mut self) -> Result<ChunkReport, EngineError> {
        let c = self
            .job()?
            .run_chunk()
            .map_err(|e| gpu_err("run chunk", e))?;
        Ok(ChunkReport {
            status: match c.status {
                GpuStatus::More => ChunkStatus::More,
                GpuStatus::Done => ChunkStatus::Done,
                GpuStatus::Aborted => ChunkStatus::Aborted,
            },
            kernel: c.kernel,
        })
    }

    fn hits(&mut self) -> Result<(Vec<Hit>, u32), EngineError> {
        let (hits, total) = self.job()?.hits().map_err(|e| gpu_err("read hits", e))?;
        Ok((
            hits.into_iter()
                .map(|h| Hit {
                    t_rows: h.t_rows,
                    t_cols: h.t_cols,
                    digest: h.digest,
                })
                .collect(),
            total,
        ))
    }

    fn dump(&mut self) -> Result<Vec<TileResult>, EngineError> {
        let records = self
            .job()?
            .dump_records()
            .map_err(|e| gpu_err("read dump", e))?;
        Ok(records
            .into_iter()
            .map(|r| TileResult {
                t_rows: r.t_rows,
                t_cols: r.t_cols,
                transcript: r.transcript,
                digest: r.digest,
            })
            .collect())
    }
}
