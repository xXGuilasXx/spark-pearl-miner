//! The only module with `unsafe`: raw declarations of cuda/include/spm_cuda.h and owned handles
//! whose methods are safe. Every call passes pointers derived from live Rust references with the
//! sizes the C side expects, and every handle is destroyed exactly once, in `Drop`.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr::{self, NonNull};

/// Mirror of `spm_device_info_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeviceInfoRaw {
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
struct JobOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct AbortOpaque {
    _private: [u8; 0],
}

/// Mirror of `spm_job_params_t`.
#[repr(C)]
pub(crate) struct JobParamsRaw {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub dump_mode: u32,
    pub gen_seed: u64,
    pub a_host: *const i8,
    pub bt_host: *const i8,
    pub b_noise_seed: [u8; 32],
    pub hit_capacity: u32,
    pub chunk_tiles: u32,
    pub target_chunk_us: u32,
    pub band_rows: u32,
    pub mem_budget_bytes: u64,
    abort_flag: *mut AbortOpaque,
}

/// Mirror of `spm_hit_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HitRaw {
    pub t_rows: u32,
    pub t_cols: u32,
    pub digest: [u8; 32],
}

/// Mirror of `spm_chunk_info_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChunkInfoRaw {
    pub tile_begin: u32,
    pub tile_end: u32,
    pub tiles_total: u32,
    pub ctas: u32,
    pub ms: f32,
}

/// Mirror of `spm_job_info_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JobInfoRaw {
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

extern "C" {
    fn spm_cuda_device_info(out: *mut DeviceInfoRaw) -> i32;
    fn spm_cuda_imma_peak(seconds: f64) -> f64;
    fn spm_cuda_version() -> *const c_char;

    fn spm_abort_create(out: *mut *mut AbortOpaque) -> i32;
    fn spm_abort_set(flag: *mut AbortOpaque, value: u32);
    fn spm_abort_get(flag: *const AbortOpaque) -> u32;
    fn spm_abort_destroy(flag: *mut AbortOpaque);

    fn spm_job_create(params: *const JobParamsRaw, out: *mut *mut JobOpaque) -> i32;
    fn spm_job_set_attempt(
        job: *mut JobOpaque,
        a_noise_seed: *const u8,
        bound: *const u8,
        a_prefix: *const u8,
        a_prefix_len: u32,
    ) -> i32;
    fn spm_job_run_chunk(job: *mut JobOpaque, info: *mut ChunkInfoRaw) -> i32;
    fn spm_job_read_hits(
        job: *mut JobOpaque,
        out: *mut HitRaw,
        capacity: u32,
        total: *mut u32,
    ) -> i32;
    fn spm_job_read_dump(job: *mut JobOpaque, out: *mut u8, len: u64) -> i32;
    fn spm_job_read_buffer(job: *mut JobOpaque, which: i32, out: *mut u8, len: u64) -> i32;
    fn spm_job_info(job: *const JobOpaque, out: *mut JobInfoRaw) -> i32;
    fn spm_job_destroy(job: *mut JobOpaque);
    fn spm_last_cuda_error() -> i32;
    fn spm_status_str(status: i32) -> *const c_char;
}

/// Static C string from the library, as an owned String.
fn static_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the library only returns pointers to static NUL-terminated strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

pub(crate) fn version() -> String {
    // SAFETY: no arguments; returns a static string.
    static_str(unsafe { spm_cuda_version() })
}

pub(crate) fn status_str(status: i32) -> String {
    // SAFETY: plain integer argument; returns a static string.
    static_str(unsafe { spm_status_str(status) })
}

pub(crate) fn last_cuda_error() -> i32 {
    // SAFETY: reads a thread-local integer on the C side.
    unsafe { spm_last_cuda_error() }
}

pub(crate) fn device_info() -> (i32, DeviceInfoRaw) {
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
    // SAFETY: `raw` is a valid, writable struct with the C layout.
    let rc = unsafe { spm_cuda_device_info(&mut raw) };
    (rc, raw)
}

pub(crate) fn imma_peak(seconds: f64) -> f64 {
    // SAFETY: scalar argument.
    unsafe { spm_cuda_imma_peak(seconds) }
}

/// Owned abort flag (host-mapped memory read by the kernels).
pub(crate) struct AbortPtr(NonNull<AbortOpaque>);

// SAFETY: the flag is a single u32 accessed with atomic loads/stores on the host side and volatile
// loads on the device; the handle is only destroyed in Drop, when no other reference exists.
unsafe impl Send for AbortPtr {}
// SAFETY: see above; `set`/`get` are atomic on the C side.
unsafe impl Sync for AbortPtr {}

impl AbortPtr {
    pub(crate) fn new() -> Result<Self, i32> {
        let mut raw: *mut AbortOpaque = ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer.
        let rc = unsafe { spm_abort_create(&mut raw) };
        match NonNull::new(raw) {
            Some(p) if rc == 0 => Ok(Self(p)),
            _ => Err(if rc == 0 { -1 } else { rc }),
        }
    }

    pub(crate) fn set(&self, value: u32) {
        // SAFETY: the handle is live for &self; the C side stores atomically.
        unsafe { spm_abort_set(self.0.as_ptr(), value) }
    }

    pub(crate) fn get(&self) -> u32 {
        // SAFETY: the handle is live for &self; the C side loads atomically.
        unsafe { spm_abort_get(self.0.as_ptr()) }
    }
}

impl Drop for AbortPtr {
    fn drop(&mut self) {
        // SAFETY: created by spm_abort_create and destroyed only here.
        unsafe { spm_abort_destroy(self.0.as_ptr()) }
    }
}

/// Parameters of [`JobPtr::create`]; host operands are borrowed only for the call.
pub(crate) struct CreateArgs<'a> {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub dump_mode: bool,
    pub gen_seed: u64,
    pub host: Option<(&'a [i8], &'a [i8])>,
    pub b_noise_seed: [u8; 32],
    pub hit_capacity: u32,
    pub chunk_tiles: u32,
    pub target_chunk_us: u32,
    pub band_rows: u32,
    pub mem_budget_bytes: u64,
}

/// Owned job handle.
pub(crate) struct JobPtr(NonNull<JobOpaque>);

// SAFETY: a job is only driven through &mut (one thread at a time); the CUDA runtime calls it makes
// are thread safe, and the handle is destroyed only in Drop.
unsafe impl Send for JobPtr {}

impl JobPtr {
    /// The caller checked that host operands have m*k and n*k entries; `abort` must outlive the
    /// job (the safe wrapper keeps an Arc to it).
    pub(crate) fn create(args: &CreateArgs<'_>, abort: &AbortPtr) -> Result<Self, i32> {
        let (a_host, bt_host) = match args.host {
            Some((a, bt)) => (a.as_ptr(), bt.as_ptr()),
            None => (ptr::null(), ptr::null()),
        };
        let raw_params = JobParamsRaw {
            m: args.m,
            n: args.n,
            k: args.k,
            dump_mode: u32::from(args.dump_mode),
            gen_seed: args.gen_seed,
            a_host,
            bt_host,
            b_noise_seed: args.b_noise_seed,
            hit_capacity: args.hit_capacity,
            chunk_tiles: args.chunk_tiles,
            target_chunk_us: args.target_chunk_us,
            band_rows: args.band_rows,
            mem_budget_bytes: args.mem_budget_bytes,
            abort_flag: abort.0.as_ptr(),
        };
        let mut raw: *mut JobOpaque = ptr::null_mut();
        // SAFETY: `raw_params` is a valid struct; the host operand pointers (if any) point to
        // m*k / n*k readable bytes that stay borrowed for this call (the C side copies them and
        // synchronizes before returning); `raw` is a valid out-pointer.
        let rc = unsafe { spm_job_create(&raw_params, &mut raw) };
        match NonNull::new(raw) {
            Some(p) if rc == 0 => Ok(Self(p)),
            _ => Err(if rc == 0 { -1 } else { rc }),
        }
    }

    pub(crate) fn set_attempt(
        &mut self,
        a_noise_seed: &[u8; 32],
        bound: &[u8; 32],
        prefix: &[u8],
    ) -> i32 {
        let Ok(len) = u32::try_from(prefix.len()) else {
            return -1;
        };
        // SAFETY: seed and bound are 32 readable bytes; `prefix` is `len` readable bytes that the
        // C side copies before returning.
        unsafe {
            spm_job_set_attempt(
                self.0.as_ptr(),
                a_noise_seed.as_ptr(),
                bound.as_ptr(),
                if prefix.is_empty() {
                    ptr::null()
                } else {
                    prefix.as_ptr()
                },
                len,
            )
        }
    }

    pub(crate) fn run_chunk(&mut self) -> (i32, ChunkInfoRaw) {
        let mut info = ChunkInfoRaw::default();
        // SAFETY: live handle, valid out-pointer.
        let rc = unsafe { spm_job_run_chunk(self.0.as_ptr(), &mut info) };
        (rc, info)
    }

    /// Fills `out` (its length is the capacity) and returns (status, total hits found).
    pub(crate) fn read_hits(&mut self, out: &mut [HitRaw]) -> (i32, u32) {
        let Ok(cap) = u32::try_from(out.len()) else {
            return (-1, 0);
        };
        let mut total = 0u32;
        // SAFETY: `out` is `cap` writable records; `total` is a valid out-pointer.
        let rc = unsafe { spm_job_read_hits(self.0.as_ptr(), out.as_mut_ptr(), cap, &mut total) };
        (rc, total)
    }

    pub(crate) fn read_dump(&mut self, out: &mut [u8]) -> i32 {
        // SAFETY: `out` is out.len() writable bytes; the C side checks the length.
        unsafe { spm_job_read_dump(self.0.as_ptr(), out.as_mut_ptr(), out.len() as u64) }
    }

    pub(crate) fn read_buffer(&mut self, which: i32, out: &mut [u8]) -> i32 {
        // SAFETY: `out` is out.len() writable bytes; the C side checks the length.
        unsafe { spm_job_read_buffer(self.0.as_ptr(), which, out.as_mut_ptr(), out.len() as u64) }
    }

    pub(crate) fn info(&self) -> (i32, JobInfoRaw) {
        let mut info = JobInfoRaw::default();
        // SAFETY: live handle, valid out-pointer.
        let rc = unsafe { spm_job_info(self.0.as_ptr(), &mut info) };
        (rc, info)
    }
}

impl Drop for JobPtr {
    fn drop(&mut self) {
        // SAFETY: created by spm_job_create and destroyed only here.
        unsafe { spm_job_destroy(self.0.as_ptr()) }
    }
}
