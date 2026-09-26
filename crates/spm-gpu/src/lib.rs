//! Safe wrapper over libspm_cuda (see cuda/include/spm_cuda.h). Only the gpu-worker process links this.
//!
//! `unsafe` lives in the private `ffi` module only; every other module forbids it.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod ffi;
mod job;

pub use job::{
    job_device_bytes, AbortHandle, AttemptStats, Buffer, Chunk, ChunkStatus, GpuError, Hit, Job,
    JobConfig, JobInfo, Operands, TileRecord, DEFAULT_MEM_BUDGET, DUMP_RECORD_LEN,
};

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub sm_count: u32,
    pub compute_capability: (u32, u32),
    pub max_smem_optin_bytes: u32,
    pub regs_per_sm: u32,
    /// `cudaDevAttrClockRate` (the rated clock, not the live one; use NVML for that).
    pub sm_clock_mhz: u32,
    pub total_mem_bytes: u64,
}

pub fn version() -> String {
    ffi::version()
}

pub fn device_info() -> anyhow::Result<DeviceInfo> {
    let (rc, raw) = ffi::device_info();
    anyhow::ensure!(
        rc == 0,
        "cudaGetDeviceProperties failed with CUDA error {rc}"
    );
    let end = raw
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(raw.name.len());
    let nonneg = |x: i32| u32::try_from(x).unwrap_or(0);
    Ok(DeviceInfo {
        name: String::from_utf8_lossy(&raw.name[..end]).into_owned(),
        sm_count: nonneg(raw.sm_count),
        compute_capability: (nonneg(raw.cc_major), nonneg(raw.cc_minor)),
        max_smem_optin_bytes: nonneg(raw.max_smem_optin_bytes),
        regs_per_sm: nonneg(raw.regs_per_sm),
        sm_clock_mhz: nonneg(raw.sm_clock_khz) / 1000,
        total_mem_bytes: u64::try_from(raw.total_mem_bytes).unwrap_or(0),
    })
}

/// Register-only INT8 tensor-core peak, in T-MAC/s. Runs a kernel on the GPU for ~`seconds`.
pub fn imma_peak_tmacs(seconds: f64) -> f64 {
    ffi::imma_peak(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn library_links_and_reports_version() {
        assert!(version().contains("libspm_cuda"));
    }
    /// Touches the GPU: creates a CUDA context for a few ms. Skipped unless SPM_GPU_TESTS=1, because a
    /// resident CUDA process blocks the owner's vLLM orchestration (spark-recurso require_idle_cuda).
    #[test]
    fn device_info_reads_gb10() {
        if std::env::var("SPM_GPU_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipped (set SPM_GPU_TESTS=1)");
            return;
        }
        let d = device_info().expect("device info");
        assert!(d.sm_count > 0);
        assert!(
            d.max_smem_optin_bytes >= 99 * 1024,
            "GB10 exposes ~99 KB opt-in smem per block"
        );
        eprintln!("{d:?}");
    }
}
