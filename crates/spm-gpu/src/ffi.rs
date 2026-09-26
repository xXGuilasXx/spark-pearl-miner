//! Raw bindings to libspm_cuda (`cuda/include/spm_cuda.h`) behind a small safe surface.
//!
//! This is the only module of the crate allowed to use `unsafe`. Every foreign call is wrapped in
//! a function whose arguments make the call sound (lengths checked against the shape, pointers
//! taken from live Rust borrows, the job handle owned and never aliased), and each `unsafe` block
//! states why. Status codes are passed through unchanged; `crate::job` turns them into errors.
//! (`unsafe_code` is allowed for this module only, at its declaration in lib.rs.)

use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;

pub const ABI_VERSION: u32 = 1;

pub const SPM_OK: i32 = 0;
pub const SPM_CHUNK_MORE: i32 = 1;
pub const SPM_CHUNK_DONE: i32 = 2;
pub const SPM_ABORTED: i32 = 3;
pub const SPM_E_INVALID: i32 = -1;

pub const SOURCE_FILL: u32 = 0;
pub const SOURCE_HOST: u32 = 1;

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

#[repr(C)]
struct JobParamsRaw {
    abi_version: u32,
    m: u32,
    n: u32,
    k: u32,
    dump_mode: u32,
    source: u32,
    fill_seed: u64,
    host_a: *const i8,
    host_bt: *const i8,
    header76: [u8; 76],
    config52: [u8; 52],
    b_noise_seed: [u8; 32],
    bound: [u8; 32],
    hit_capacity: u32,
    chunk_ctas: u32,
    mem_budget_bytes: u64,
}

/// One hit-ring record (`spm_hit_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HitRaw {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// `spm_job_info_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct JobInfoRaw {
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
    pub job_key: [u8; 32],
    pub cuda_error: i32,
    pub smem_bytes: u32,
    pub attempt_gpu_ms: f32,
    pub attempt_max_chunk_ms: f32,
    pub attempt_chunks: u32,
    pub reserved: u32,
}

#[repr(C)]
struct SpmJob {
    _opaque: [u8; 0],
}

extern "C" {
    fn spm_cuda_device_info(out: *mut DeviceInfoRaw) -> i32;
    fn spm_cuda_imma_peak(seconds: f64) -> f64;
    fn spm_cuda_version() -> *const c_char;

    fn spm_job_create(params: *const JobParamsRaw, out: *mut *mut SpmJob) -> i32;
    fn spm_job_patch_a(job: *mut SpmJob, offset: u64, bytes: *const i8, len: u32) -> i32;
    fn spm_job_set_attempt(job: *mut SpmJob, a_noise_seed: *const u8, bound: *const u8) -> i32;
    fn spm_job_run_chunk(job: *mut SpmJob) -> i32;
    fn spm_job_run(job: *mut SpmJob, abort_flag: *const u32) -> i32;
    fn spm_job_read_hits(
        job: *mut SpmJob,
        out: *mut HitRaw,
        cap: u32,
        n_out: *mut u32,
        lost_out: *mut u32,
    ) -> i32;
    fn spm_job_read_dump(job: *mut SpmJob, out: *mut u8, len: u64) -> i32;
    fn spm_job_read_debug(job: *mut SpmJob, which: i32, out: *mut c_void, len: u64) -> i32;
    fn spm_job_get_info(job: *const SpmJob, out: *mut JobInfoRaw) -> i32;
    fn spm_job_destroy(job: *mut SpmJob);
    fn spm_status_str(code: i32) -> *const c_char;
    fn spm_last_cuda_error() -> i32;
    fn spm_cuda_error_str(code: i32) -> *const c_char;
}

fn static_str(p: *const c_char) -> &'static str {
    if p.is_null() {
        return "";
    }
    // SAFETY: every string-returning function of the library returns a pointer to a static,
    // NUL-terminated string (string literals or cudaGetErrorString), valid for the program's life.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

pub fn version() -> &'static str {
    // SAFETY: no arguments; returns a static string.
    static_str(unsafe { spm_cuda_version() })
}

pub fn status_str(code: i32) -> &'static str {
    // SAFETY: pure lookup on an integer; returns a static string.
    static_str(unsafe { spm_status_str(code) })
}

/// Last CUDA error of this thread and its description.
pub fn last_cuda_error() -> (i32, &'static str) {
    // SAFETY: reads a thread-local integer; the description is a static string.
    let code = unsafe { spm_last_cuda_error() };
    // SAFETY: as above.
    (code, static_str(unsafe { spm_cuda_error_str(code) }))
}

pub fn cuda_error_str(code: i32) -> &'static str {
    // SAFETY: cudaGetErrorString accepts any value and returns a static string.
    static_str(unsafe { spm_cuda_error_str(code) })
}

pub fn device_info() -> Result<DeviceInfoRaw, i32> {
    let mut raw = DeviceInfoRaw {
        name: [0; 64],
        sm_count: 0,
        cc_major: 0,
        cc_minor: 0,
        max_smem_optin_bytes: 0,
        regs_per_sm: 0,
        sm_clock_khz: 0,
        total_mem_bytes: 0,
    };
    // SAFETY: `raw` is a valid, writable struct of the layout the C side expects.
    let rc = unsafe { spm_cuda_device_info(&mut raw) };
    if rc == 0 {
        Ok(raw)
    } else {
        Err(rc)
    }
}

pub fn imma_peak(seconds: f64) -> f64 {
    // SAFETY: plain call with a scalar argument.
    unsafe { spm_cuda_imma_peak(seconds) }
}

/// Where the committed matrices come from.
pub enum SourceArg<'a> {
    /// `spm_cpuref::fill_int7(seed, DOMAIN_A / DOMAIN_BT)`, generated on the GPU.
    Fill(u64),
    /// Host copies, m·k and n·k entries row major.
    Host { a: &'a [i8], bt: &'a [i8] },
}

pub struct CreateArgs<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub dump: bool,
    pub source: SourceArg<'a>,
    pub header76: [u8; 76],
    pub config52: [u8; 52],
    pub b_noise_seed: [u8; 32],
    pub bound: [u8; 32],
    pub hit_capacity: u32,
    pub chunk_ctas: u32,
    pub mem_budget_bytes: u64,
}

/// Owned `spm_job_t*`; destroyed on drop.
pub struct JobHandle(NonNull<SpmJob>);

// SAFETY: the handle is only used through `&mut self` (or `&self` for the read-only info call), so
// one thread at a time drives the job; the CUDA runtime API it calls is thread-safe and the job's
// stream and buffers are not tied to the creating thread.
unsafe impl Send for JobHandle {}

impl JobHandle {
    pub fn create(args: &CreateArgs<'_>) -> Result<Self, i32> {
        let (source, fill_seed, host_a, host_bt) = match args.source {
            SourceArg::Fill(seed) => (SOURCE_FILL, seed, std::ptr::null(), std::ptr::null()),
            SourceArg::Host { a, bt } => {
                let mk = u64::from(args.m) * u64::from(args.k);
                let nk = u64::from(args.n) * u64::from(args.k);
                if a.len() as u64 != mk || bt.len() as u64 != nk {
                    return Err(SPM_E_INVALID);
                }
                (SOURCE_HOST, 0, a.as_ptr(), bt.as_ptr())
            }
        };
        let raw = JobParamsRaw {
            abi_version: ABI_VERSION,
            m: args.m,
            n: args.n,
            k: args.k,
            dump_mode: u32::from(args.dump),
            source,
            fill_seed,
            host_a,
            host_bt,
            header76: args.header76,
            config52: args.config52,
            b_noise_seed: args.b_noise_seed,
            bound: args.bound,
            hit_capacity: args.hit_capacity,
            chunk_ctas: args.chunk_ctas,
            mem_budget_bytes: args.mem_budget_bytes,
        };
        let mut out: *mut SpmJob = std::ptr::null_mut();
        // SAFETY: `raw` is a valid params struct; for the host source its pointers come from live
        // slices whose lengths were just checked to be m·k and n·k (the library copies them before
        // returning); `out` is a valid place for the handle.
        let rc = unsafe { spm_job_create(&raw, &mut out) };
        match NonNull::new(out) {
            Some(job) if rc == SPM_OK => Ok(Self(job)),
            _ => Err(if rc == SPM_OK { SPM_E_INVALID } else { rc }),
        }
    }

    pub fn patch_a(&mut self, offset: u64, bytes: &[i8]) -> i32 {
        let Ok(len) = u32::try_from(bytes.len()) else {
            return SPM_E_INVALID;
        };
        // SAFETY: the handle is live and exclusively borrowed; `bytes` is valid for `len` bytes.
        unsafe { spm_job_patch_a(self.0.as_ptr(), offset, bytes.as_ptr(), len) }
    }

    pub fn set_attempt(&mut self, a_noise_seed: &[u8; 32], bound: Option<&[u8; 32]>) -> i32 {
        let bound = bound.map_or(std::ptr::null(), |b| b.as_ptr());
        // SAFETY: live, exclusively borrowed handle; both pointers are 32-byte arrays or null
        // (allowed for `bound`).
        unsafe { spm_job_set_attempt(self.0.as_ptr(), a_noise_seed.as_ptr(), bound) }
    }

    pub fn run_chunk(&mut self) -> i32 {
        // SAFETY: live, exclusively borrowed handle.
        unsafe { spm_job_run_chunk(self.0.as_ptr()) }
    }

    pub fn run(&mut self, abort: Option<&AtomicU32>) -> i32 {
        let flag = abort.map_or(std::ptr::null(), |a| a.as_ptr().cast_const());
        // SAFETY: live, exclusively borrowed handle; `flag` is null or points to an AtomicU32 that
        // outlives the call. The library only reads it with an atomic acquire load, so concurrent
        // atomic stores from other threads are fine.
        unsafe { spm_job_run(self.0.as_ptr(), flag) }
    }

    /// Returns (status, hits written into `out`, hits lost to ring overwrites).
    pub fn read_hits(&mut self, out: &mut [HitRaw]) -> (i32, u32, u32) {
        let cap = u32::try_from(out.len()).unwrap_or(u32::MAX);
        let (mut n, mut lost) = (0u32, 0u32);
        // SAFETY: live, exclusively borrowed handle; `out` has room for `cap` records and the
        // library writes at most `cap`; `n` and `lost` are valid out-params.
        let rc =
            unsafe { spm_job_read_hits(self.0.as_ptr(), out.as_mut_ptr(), cap, &mut n, &mut lost) };
        (rc, n, lost)
    }

    pub fn read_dump(&mut self, out: &mut [u8]) -> i32 {
        // SAFETY: live, exclusively borrowed handle; `out` is valid for `out.len()` bytes and the
        // library refuses any length other than the exact dump size.
        unsafe { spm_job_read_dump(self.0.as_ptr(), out.as_mut_ptr(), out.len() as u64) }
    }

    pub fn read_debug(&mut self, which: i32, out: &mut [u8]) -> i32 {
        // SAFETY: as for read_dump: the library writes exactly `out.len()` bytes or refuses.
        unsafe {
            spm_job_read_debug(
                self.0.as_ptr(),
                which,
                out.as_mut_ptr().cast(),
                out.len() as u64,
            )
        }
    }

    pub fn info(&self) -> (i32, JobInfoRaw) {
        let mut info = JobInfoRaw::default();
        // SAFETY: live handle (the call only reads host-side fields); `info` is a valid out-param.
        let rc = unsafe { spm_job_get_info(self.0.as_ptr(), &mut info) };
        (rc, info)
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from spm_job_create, is owned by `self` and is never used again.
        unsafe { spm_job_destroy(self.0.as_ptr()) }
    }
}
