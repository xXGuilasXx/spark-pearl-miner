//! Safe wrapper over libspm_cuda (see cuda/include/spm_cuda.h). Only the gpu-worker process links this.
use std::ffi::CStr;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DeviceInfoRaw {
    pub name: [u8; 64],
    pub sm_count: i32,
    pub cc_major: i32,
    pub cc_minor: i32,
    pub max_smem_optin_bytes: i32,
    pub regs_per_sm: i32,
    pub sm_clock_khz: i32,
    pub total_mem_bytes: i64,
}

extern "C" {
    fn spm_cuda_device_info(out: *mut DeviceInfoRaw) -> i32;
    fn spm_cuda_imma_peak(seconds: f64) -> f64;
    fn spm_cuda_version() -> *const std::os::raw::c_char;
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub sm_count: u32,
    pub compute_capability: (u32, u32),
    pub max_smem_optin_bytes: u32,
    pub regs_per_sm: u32,
    pub sm_clock_mhz: u32,
    pub total_mem_bytes: u64,
}

pub fn version() -> String {
    // SAFETY: the C side returns a pointer to a static NUL-terminated string.
    unsafe { CStr::from_ptr(spm_cuda_version()) }.to_string_lossy().into_owned()
}

pub fn device_info() -> anyhow::Result<DeviceInfo> {
    let mut raw = DeviceInfoRaw { name: [0; 64], sm_count: 0, cc_major: 0, cc_minor: 0, max_smem_optin_bytes: 0, regs_per_sm: 0, sm_clock_khz: 0, total_mem_bytes: 0 };
    // SAFETY: `raw` is a valid, writable struct of the layout the C side expects.
    let rc = unsafe { spm_cuda_device_info(&mut raw) };
    anyhow::ensure!(rc == 0, "cudaGetDeviceProperties failed with CUDA error {rc}");
    let end = raw.name.iter().position(|&b| b == 0).unwrap_or(raw.name.len());
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
    // SAFETY: plain FFI call with a scalar argument.
    unsafe { spm_cuda_imma_peak(seconds) }
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
        if std::env::var("SPM_GPU_TESTS").ok().as_deref() != Some("1") { eprintln!("skipped (set SPM_GPU_TESTS=1)"); return; }
        let d = device_info().expect("device info");
        assert!(d.sm_count > 0);
        assert!(d.max_smem_optin_bytes >= 99 * 1024, "GB10 exposes ~99 KB opt-in smem per block");
        eprintln!("{d:?}");
    }
}
