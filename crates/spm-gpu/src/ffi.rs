//! Raw bindings to libspm_cuda (`cuda/include/spm_cuda.h`) — the only `unsafe` code of the crate.
//!
//! Every item exported from here is safe to call: the wrappers check buffer sizes on the Rust
//! side before handing pointers to C, keep the job pointer private to [`JobHandle`], and return
//! the raw status codes, which `job.rs` turns into typed errors. The C side never throws, aborts
//! or calls back into Rust, so no panic can cross the boundary in either direction.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicU32;

// Status codes (SPM_* in spm_cuda.h).
pub const SPM_OK: i32 = 0;
pub const SPM_ERR_NULL: i32 = -1;
pub const SPM_ERR_SHAPE: i32 = -2;
pub const SPM_ERR_CONFIG: i32 = -3;
pub const SPM_ERR_BUDGET: i32 = -4;
pub const SPM_ERR_CUDA: i32 = -5;
pub const SPM_ERR_STATE: i32 = -6;
pub const SPM_ERR_RANGE: i32 = -7;
pub const SPM_ERR_NO_DUMP: i32 = -8;
pub const SPM_ERR_INTERNAL: i32 = -9;
pub const SPM_CHUNK_MORE: i32 = 1;
pub const SPM_CHUNK_DONE: i32 = 2;
pub const SPM_CHUNK_ABORTED: i32 = 3;
pub const SPM_JOB_DUMP: u32 = 1;
pub const SPM_DUMP_RECORD_BYTES: usize = 104;
pub const SPM_JOB_DEVICE_BUDGET_BYTES: u64 = 2 << 30;

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
    m: u32,
    n: u32,
    k: u32,
    config52: *const u8,
    gen_seed: u64,
    host_a: *const i8,
    host_bt: *const i8,
    b_noise_seed: *const u8,
    bound: *const u8,
    flags: u32,
    chunk_ctas: u32,
    hit_capacity: u32,
}

/// `spm_hit_t`: one hit of the ring.
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
    pub block_m: u32,
    pub block_n: u32,
    pub tiles: u64,
    pub cta_tiles: u32,
    pub chunk_ctas: u32,
    pub chunks: u32,
    pub ctas_per_sm: u32,
    pub sm_count: u32,
    pub smem_bytes: u32,
    pub hit_capacity: u32,
    pub device_bytes: u64,
    pub last_chunk_ms: f32,
    pub last_prep_ms: f32,
    pub create_prep_ms: f32,
}

#[repr(C)]
struct JobRaw {
    _opaque: [u8; 0],
}

extern "C" {
    fn spm_cuda_device_info(out: *mut DeviceInfoRaw) -> i32;
    fn spm_cuda_imma_peak(seconds: f64) -> f64;
    fn spm_cuda_version() -> *const c_char;
    fn spm_job_create(params: *const JobParamsRaw, out: *mut *mut JobRaw) -> i32;
    fn spm_job_patch_a(job: *mut JobRaw, offset: u64, data: *const i8, len: u64) -> i32;
    fn spm_job_set_attempt(job: *mut JobRaw, a_noise_seed: *const u8, bound: *const u8) -> i32;
    fn spm_job_run_chunk(job: *mut JobRaw, status: *mut i32) -> i32;
    fn spm_job_run_attempt(job: *mut JobRaw, abort_flag: *const u32, status: *mut i32) -> i32;
    fn spm_job_read_hits(job: *mut JobRaw, out: *mut HitRaw, cap: u32, total: *mut u32) -> i32;
    fn spm_job_read_dump(job: *mut JobRaw, out: *mut u8, cap: u64, written: *mut u64) -> i32;
    fn spm_job_read_buffer(
        job: *mut JobRaw,
        which: i32,
        offset: u64,
        out: *mut c_void,
        len: u64,
    ) -> i32;
    fn spm_job_info(job: *const JobRaw, out: *mut JobInfoRaw) -> i32;
    fn spm_job_last_cuda_error(job: *const JobRaw) -> i32;
    fn spm_job_destroy(job: *mut JobRaw);
    fn spm_last_cuda_error() -> i32;
    fn spm_status_string(status: i32) -> *const c_char;
    fn spm_cuda_error_string(cuda_error: i32) -> *const c_char;
    fn spm_debug_blake3_keyed64(key32: *const u8, msg64: *const u8, out32: *mut u8) -> i32;
}

fn static_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the C side only returns pointers to static NUL-terminated strings (or NULL, handled
    // above).
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

pub fn version() -> String {
    // SAFETY: no arguments; returns a static string.
    static_str(unsafe { spm_cuda_version() })
}

pub fn device_info(out: &mut DeviceInfoRaw) -> i32 {
    // SAFETY: `out` is a valid, writable struct with the C layout (repr(C), same fields).
    unsafe { spm_cuda_device_info(out) }
}

pub fn imma_peak(seconds: f64) -> f64 {
    // SAFETY: plain scalar argument and result.
    unsafe { spm_cuda_imma_peak(seconds) }
}

pub fn status_string(status: i32) -> String {
    // SAFETY: scalar argument; returns a static string.
    static_str(unsafe { spm_status_string(status) })
}

pub fn cuda_error_string(code: i32) -> String {
    // SAFETY: scalar argument; cudaGetErrorString returns a static string for any code.
    static_str(unsafe { spm_cuda_error_string(code) })
}

/// CUDA error of the last failed `spm_job_create` / debug call on this thread.
pub fn last_create_cuda_error() -> i32 {
    // SAFETY: no arguments; reads a thread-local on the C side.
    unsafe { spm_last_cuda_error() }
}

pub fn debug_blake3_keyed64(key: &[u8; 32], msg: &[u8; 64], out: &mut [u8; 32]) -> i32 {
    // SAFETY: the three pointers come from arrays of exactly the sizes the C side reads/writes.
    unsafe { spm_debug_blake3_keyed64(key.as_ptr(), msg.as_ptr(), out.as_mut_ptr()) }
}

/// Arguments of [`JobHandle::create`], borrowed for the duration of the call only.
pub struct CreateArgs<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub config52: &'a [u8; 52],
    pub gen_seed: u64,
    pub host: Option<(&'a [i8], &'a [i8])>,
    pub b_noise_seed: &'a [u8; 32],
    pub bound: &'a [u8; 32],
    pub flags: u32,
    pub chunk_ctas: u32,
    pub hit_capacity: u32,
}

/// Owner of one `spm_job_t`. Destroys it on drop.
pub struct JobHandle(NonNull<JobRaw>);

// SAFETY: the C job has no thread affinity (every entry point re-selects device 0 and the job's
// own stream) and all mutating calls take `&mut self`, so moving the handle to another thread is
// fine. It is deliberately not Sync.
unsafe impl Send for JobHandle {}

impl JobHandle {
    /// `spm_job_create`. On failure returns `(status, cuda_error)`.
    pub fn create(a: &CreateArgs<'_>) -> Result<Self, (i32, i32)> {
        let (host_a, host_bt) = match a.host {
            Some((ha, hb)) => {
                // The C side reads exactly m*k and n*k entries: refuse anything shorter or longer.
                let mk = (a.m as usize).checked_mul(a.k as usize);
                let nk = (a.n as usize).checked_mul(a.k as usize);
                if mk != Some(ha.len()) || nk != Some(hb.len()) {
                    return Err((SPM_ERR_RANGE, 0));
                }
                (ha.as_ptr(), hb.as_ptr())
            }
            None => (ptr::null(), ptr::null()),
        };
        let raw = JobParamsRaw {
            m: a.m,
            n: a.n,
            k: a.k,
            config52: a.config52.as_ptr(),
            gen_seed: a.gen_seed,
            host_a,
            host_bt,
            b_noise_seed: a.b_noise_seed.as_ptr(),
            bound: a.bound.as_ptr(),
            flags: a.flags,
            chunk_ctas: a.chunk_ctas,
            hit_capacity: a.hit_capacity,
        };
        let mut out: *mut JobRaw = ptr::null_mut();
        // SAFETY: `raw` points to borrowed arrays of the sizes the header documents (52/32/32
        // bytes, m*k and n*k entries checked above) that outlive the call; `out` is writable.
        let rc = unsafe { spm_job_create(&raw, &mut out) };
        if rc != SPM_OK {
            let cuda = if rc == SPM_ERR_CUDA {
                last_create_cuda_error()
            } else {
                0
            };
            return Err((rc, cuda));
        }
        NonNull::new(out)
            .map(JobHandle)
            .ok_or((SPM_ERR_INTERNAL, 0))
    }

    pub fn patch_a(&mut self, offset: u64, data: &[i8]) -> i32 {
        // SAFETY: the job pointer is live (owned by self); `data` is valid for data.len() bytes.
        unsafe { spm_job_patch_a(self.0.as_ptr(), offset, data.as_ptr(), data.len() as u64) }
    }

    pub fn set_attempt(&mut self, a_noise_seed: &[u8; 32], bound: Option<&[u8; 32]>) -> i32 {
        let bound_ptr = bound.map_or(ptr::null(), |b| b.as_ptr());
        // SAFETY: live job; the seed and the optional bound are 32-byte arrays.
        unsafe { spm_job_set_attempt(self.0.as_ptr(), a_noise_seed.as_ptr(), bound_ptr) }
    }

    /// Returns `(rc, chunk_status)`.
    pub fn run_chunk(&mut self) -> (i32, i32) {
        let mut status = 0i32;
        // SAFETY: live job; `status` is writable.
        let rc = unsafe { spm_job_run_chunk(self.0.as_ptr(), &mut status) };
        (rc, status)
    }

    /// Returns `(rc, chunk_status)`. `abort` may be set from other threads during the call.
    pub fn run_attempt(&mut self, abort: &AtomicU32) -> (i32, i32) {
        let mut status = 0i32;
        // SAFETY: live job; the flag pointer is valid for the whole call (borrowed) and the C side
        // only reads it with an atomic load, matching the Rust side's atomic stores.
        let rc = unsafe { spm_job_run_attempt(self.0.as_ptr(), abort.as_ptr(), &mut status) };
        (rc, status)
    }

    /// Returns `(rc, total_hits)`; fills at most `out.len()` entries.
    pub fn read_hits(&mut self, out: &mut [HitRaw]) -> (i32, u32) {
        let cap = u32::try_from(out.len()).unwrap_or(u32::MAX);
        let mut total = 0u32;
        // SAFETY: live job; `out` is writable for `cap` <= out.len() entries of the C layout.
        let rc = unsafe { spm_job_read_hits(self.0.as_ptr(), out.as_mut_ptr(), cap, &mut total) };
        (rc, total)
    }

    /// Returns `(rc, bytes_written)`.
    pub fn read_dump(&mut self, out: &mut [u8]) -> (i32, u64) {
        let mut written = 0u64;
        // SAFETY: live job; `out` is writable for out.len() bytes, which is the cap passed.
        let rc = unsafe {
            spm_job_read_dump(
                self.0.as_ptr(),
                out.as_mut_ptr(),
                out.len() as u64,
                &mut written,
            )
        };
        (rc, written)
    }

    pub fn read_buffer(&mut self, which: i32, offset: u64, out: &mut [u8]) -> i32 {
        // SAFETY: live job; `out` is writable for exactly the out.len() bytes requested.
        unsafe {
            spm_job_read_buffer(
                self.0.as_ptr(),
                which,
                offset,
                out.as_mut_ptr().cast(),
                out.len() as u64,
            )
        }
    }

    pub fn info(&self) -> (i32, JobInfoRaw) {
        let mut info = JobInfoRaw::default();
        // SAFETY: live job; `info` is a writable struct with the C layout.
        let rc = unsafe { spm_job_info(self.0.as_ptr(), &mut info) };
        (rc, info)
    }

    pub fn last_cuda_error(&self) -> i32 {
        // SAFETY: live job.
        unsafe { spm_job_last_cuda_error(self.0.as_ptr()) }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: the pointer came from spm_job_create, is owned by self and freed exactly once.
        unsafe { spm_job_destroy(self.0.as_ptr()) }
    }
}
