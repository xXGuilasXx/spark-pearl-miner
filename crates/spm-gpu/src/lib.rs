//! Safe wrapper over libspm_cuda (see cuda/include/spm_cuda.h). Only the gpu-worker process links
//! this.
//!
//! `unsafe` lives in one module, [`ffi`]'s private bindings, each call with a SAFETY comment; the
//! crate root denies it and `job` forbids it. (`forbid` at the crate root would make the FFI
//! module impossible, since it cannot be relaxed further down.)
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod ffi;
mod job;

pub use job::{
    config52_for, debug_blake3_keyed64, Buffer, ChunkStatus, ErrorKind, GpuError, Hit, Hits, Job,
    JobInfo, JobParams, Matrices, TileRecord, DUMP_RECORD_BYTES, JOB_DEVICE_BUDGET_BYTES,
};

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub sm_count: u32,
    pub compute_capability: (u32, u32),
    pub max_smem_optin_bytes: u32,
    pub regs_per_sm: u32,
    /// Maximum (boost) SM clock reported by the CUDA runtime, not the current clock.
    pub sm_clock_mhz: u32,
    pub total_mem_bytes: u64,
}

pub fn version() -> String {
    ffi::version()
}

pub fn device_info() -> anyhow::Result<DeviceInfo> {
    let mut raw = ffi::DeviceInfoRaw {
        name: [0; 64],
        sm_count: 0,
        cc_major: 0,
        cc_minor: 0,
        max_smem_optin_bytes: 0,
        regs_per_sm: 0,
        sm_clock_khz: 0,
        total_mem_bytes: 0,
    };
    let rc = ffi::device_info(&mut raw);
    anyhow::ensure!(
        rc == 0,
        "cudaGetDeviceProperties failed with CUDA error {rc}"
    );
    let end = raw
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(raw.name.len());
    Ok(DeviceInfo {
        name: String::from_utf8_lossy(&raw.name[..end]).into_owned(),
        sm_count: raw.sm_count.max(0) as u32,
        compute_capability: (raw.cc_major.max(0) as u32, raw.cc_minor.max(0) as u32),
        max_smem_optin_bytes: raw.max_smem_optin_bytes.max(0) as u32,
        regs_per_sm: raw.regs_per_sm.max(0) as u32,
        sm_clock_mhz: (raw.sm_clock_khz.max(0) as u32) / 1000,
        total_mem_bytes: raw.total_mem_bytes.max(0) as u64,
    })
}

/// Register-only INT8 tensor-core peak, in T-MAC/s. Runs a kernel on the GPU for ~`seconds`.
pub fn imma_peak_tmacs(seconds: f64) -> f64 {
    ffi::imma_peak(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu_tests_enabled() -> bool {
        std::env::var("SPM_GPU_TESTS").ok().as_deref() == Some("1")
    }

    #[test]
    fn library_links_and_reports_version() {
        assert!(version().contains("libspm_cuda"));
    }

    #[test]
    fn config52_matches_the_documented_layout() {
        let c = config52_for(4096);
        assert_eq!(&c[0..4], &4096u32.to_le_bytes());
        assert_eq!(&c[4..6], &128u16.to_le_bytes());
        assert_eq!(&c[6..8], &[0, 0]);
        assert_eq!(&c[8..14], &[0x07, 0x07, 0, 0, 0, 0]);
        assert_eq!(&c[14..20], &[0x00, 0x01, 0x03, 0x07, 0, 0]);
        assert!(c[20..].iter().all(|&b| b == 0));
    }

    #[test]
    fn invalid_jobs_are_refused_before_touching_the_gpu() {
        // Shape and configuration checks run before any CUDA call, so this needs no GPU.
        let base = JobParams {
            m: 256,
            n: 256,
            k: 2048,
            config52: config52_for(2048),
            matrices: Matrices::Generated { seed: 1 },
            b_noise_seed: [0; 32],
            bound: [0; 32],
            dump: false,
            chunk_ctas: None,
            hit_capacity: None,
        };
        let shape = |m, n, k| JobParams {
            m,
            n,
            k,
            config52: config52_for(k),
            ..base.clone()
        };
        for p in [
            shape(0, 256, 2048),
            shape(96, 256, 2048),
            shape(256, 256, 1024),
            shape(256, 256, 2080),
        ] {
            assert_eq!(
                Job::new(&p).unwrap_err().kind,
                ErrorKind::Shape,
                "{}x{}x{}",
                p.m,
                p.n,
                p.k
            );
        }
        let mut wrong_k = base.clone();
        wrong_k.config52 = config52_for(4096);
        assert_eq!(Job::new(&wrong_k).unwrap_err().kind, ErrorKind::Config);
        let mut wrong_rank = base.clone();
        wrong_rank.config52[4] = 64;
        assert_eq!(Job::new(&wrong_rank).unwrap_err().kind, ErrorKind::Config);
        let a = vec![0i8; 256 * 2048];
        let short = vec![0i8; 10];
        let host = JobParams {
            matrices: Matrices::Host { a: &a, bt: &short },
            ..base.clone()
        };
        assert_eq!(Job::new(&host).unwrap_err().kind, ErrorKind::Range);
        let mut bad = vec![0i8; 256 * 2048];
        bad[77] = 65; // outside the verifier's [-64, 64]
        let host = JobParams {
            matrices: Matrices::Host { a: &a, bt: &bad },
            ..base.clone()
        };
        assert_eq!(Job::new(&host).unwrap_err().kind, ErrorKind::Range);
        let huge = JobParams {
            m: 1 << 18,
            n: 1 << 18,
            k: 4096,
            config52: config52_for(4096),
            ..base
        };
        assert_eq!(Job::new(&huge).unwrap_err().kind, ErrorKind::Budget);
    }

    /// Touches the GPU: creates a CUDA context for a few ms. Skipped unless SPM_GPU_TESTS=1, because a
    /// resident CUDA process blocks the owner's vLLM orchestration (spark-recurso require_idle_cuda).
    #[test]
    fn device_info_reads_gb10() {
        if !gpu_tests_enabled() {
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
