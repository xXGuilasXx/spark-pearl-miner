// C ABI between the Rust gpu-worker and libspm_cuda (CUDA C++). Keep this header dependency-free.
//
// Conventions: every function returns an spm_status_t (0 = success, < 0 = error, > 0 = a chunk
// status); nothing throws or aborts across this boundary. All byte strings (seeds, bounds,
// digests) are little endian exactly as the Rust side holds them. A job handle is used by one
// thread at a time; only the abort flag passed to spm_job_run_attempt may be written
// concurrently (atomically) by another thread.
#pragma once
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  char name[64];
  int32_t sm_count;
  int32_t cc_major;
  int32_t cc_minor;
  int32_t max_smem_optin_bytes;   // per block, opt-in
  int32_t regs_per_sm;
  int32_t sm_clock_khz;           // current
  int64_t total_mem_bytes;
} spm_device_info_t;

// Fills `out` for device 0. Returns 0 on success, a CUDA error code otherwise.
int32_t spm_cuda_device_info(spm_device_info_t* out);

// Runs the register-only IMMA peak probe for ~`seconds` and returns T-MAC/s (0 on failure).
double spm_cuda_imma_peak(double seconds);

// Library version string (static).
const char* spm_cuda_version(void);

// ---- PearlHash V3 jobs (M5, kernel v0) ------------------------------------------------------------

typedef int32_t spm_status_t;

#define SPM_OK 0
#define SPM_ERR_NULL (-1)         // a required pointer argument is NULL
#define SPM_ERR_SHAPE (-2)        // m, n, k outside what the kernel and consensus accept
#define SPM_ERR_CONFIG (-3)       // config52 is not the configuration this kernel implements
#define SPM_ERR_BUDGET (-4)       // the job would exceed the device-memory budget (2 GiB)
#define SPM_ERR_CUDA (-5)         // a CUDA call failed; see spm_job_last_cuda_error / spm_last_cuda_error
#define SPM_ERR_STATE (-6)        // call out of order (e.g. run before spm_job_set_attempt)
#define SPM_ERR_RANGE (-7)        // offset/length out of range or output buffer too small
#define SPM_ERR_NO_DUMP (-8)      // the job was created without SPM_JOB_DUMP
#define SPM_ERR_INTERNAL (-9)     // unexpected failure (allocation of host bookkeeping, ...)

#define SPM_CHUNK_MORE 1          // chunk done, more chunks remain in this attempt
#define SPM_CHUNK_DONE 2          // last chunk done: hits (and dump) are complete
#define SPM_CHUNK_ABORTED 3       // the abort flag was seen between chunks; running again resumes the attempt

// Job flags.
#define SPM_JOB_DUMP 1u           // debug dump: every tile writes its 104-byte record

// Device-memory budget of one job (A_base + A' + B'ᵀ + factors + ring [+ dump]).
#define SPM_JOB_DEVICE_BUDGET_BYTES (2ull << 30)

// Size of one dump record: t_rows u32, t_cols u32, transcript 16 x u32, digest 32 bytes, all LE
// (spm_cpuref::TileResult::dump_bytes).
#define SPM_DUMP_RECORD_BYTES 104u

typedef struct spm_job spm_job_t;

typedef struct {
  uint32_t m, n, k;             // A is m x k, Bᵀ is n x k; m, n multiples of 64; 2048 <= k <= 65536, k % 64 == 0
  const uint8_t* config52;      // MiningConfiguration::to_bytes(): must be k, r = 128, int7, 8x16 pattern, no MoE
  uint64_t gen_seed;            // when host_a/host_bt are NULL: spm_cpuref::fill_int7 streams on the GPU
  const int8_t* host_a;         // optional m*k entries in [-64, 64] (row major); NULL = generate
  const int8_t* host_bt;        // optional n*k entries in [-64, 64] (row major); NULL = generate
  const uint8_t* b_noise_seed;  // 32 bytes: blake3(job_key || bound_b), computed on the host (v0)
  const uint8_t* bound;         // 32 bytes LE U256: a tile hits when LE(digest) <= bound
  uint32_t flags;               // SPM_JOB_*
  uint32_t chunk_ctas;          // CTA tiles per launch chunk; 0 = auto (about 4 ms per chunk, adapted to the measured rate)
  uint32_t hit_capacity;        // hit-ring entries; 0 = 4096
} spm_job_params_t;

typedef struct {
  uint32_t t_rows;
  uint32_t t_cols;
  uint8_t digest[32];
} spm_hit_t;

typedef struct {
  uint32_t m, n, k;
  uint32_t block_m, block_n;    // CTA tile of the fused kernel
  uint64_t tiles;               // hash tiles per attempt (m * n / 128)
  uint32_t cta_tiles;           // CTA tiles per attempt
  uint32_t chunk_ctas;          // CTA tiles of the next chunk (auto mode adapts it after every chunk)
  uint32_t chunks;              // chunks per attempt at the current chunk size
  uint32_t ctas_per_sm;         // occupancy of the fused kernel
  uint32_t sm_count;
  uint32_t smem_bytes;          // dynamic shared memory per CTA
  uint32_t hit_capacity;        // entries of the hit ring
  uint64_t device_bytes;        // device memory held by the job
  float last_chunk_ms;          // GPU time of the last chunk (events)
  float last_prep_ms;           // GPU time of the last spm_job_set_attempt (A side)
  float create_prep_ms;         // GPU time of the B side (and matrix fill/upload) at creation
} spm_job_info_t;

// Buffers readable with spm_job_read_buffer (debugging and tests).
#define SPM_BUF_A_BASE 0          // m x k, A before noise (after patches)
#define SPM_BUF_A_NOISED 1        // m x k, A' = A + E_A (valid after spm_job_set_attempt)
#define SPM_BUF_BT_NOISED 2       // n x k, B'ᵀ = Bᵀ + E_Bᵀ
#define SPM_BUF_A_L 3             // m x 128, uniform factor of A
#define SPM_BUF_B_RT 4            // n x 128, uniform factor of B (transposed)
#define SPM_BUF_A_PAIRS 5         // 2k bytes: (p, q) of A_R column l at [2l, 2l+1]
#define SPM_BUF_B_PAIRS 6         // 2k bytes: (p, q) of B_L row l

// Creates a job on device 0: allocates its buffers, fills or uploads A_base and Bᵀ and builds
// the B side (B_Rᵀ, B pairs, B'ᵀ = Bᵀ + E_Bᵀ in place). On success *out owns the job.
spm_status_t spm_job_create(const spm_job_params_t* params, spm_job_t** out);

// Overwrites bytes [offset, offset + len) of A_base (e.g. the nonce in chunk 0). Takes effect at
// the next spm_job_set_attempt.
spm_status_t spm_job_patch_a(spm_job_t* job, uint64_t offset, const int8_t* data, uint64_t len);

// Starts an attempt: builds the A side (A_L, A pairs, A' = A + E_A) keyed by `a_noise_seed`
// (32 bytes, also the jackpot key), resets the hit ring and the chunk cursor. `bound` (32 bytes
// LE) replaces the job's bound when not NULL.
spm_status_t spm_job_set_attempt(spm_job_t* job, const uint8_t* a_noise_seed, const uint8_t* bound);

// Runs the next chunk of the attempt synchronously; *status = SPM_CHUNK_MORE or SPM_CHUNK_DONE.
spm_status_t spm_job_run_chunk(spm_job_t* job, int32_t* status);

// Runs the remaining chunks, reading *abort_flag (atomically, relaxed) before each chunk; a
// non-zero flag stops the attempt with *status = SPM_CHUNK_ABORTED. abort_flag may be NULL.
spm_status_t spm_job_run_attempt(spm_job_t* job, const uint32_t* abort_flag, int32_t* status);

// Copies up to `cap` hits of the current attempt to `out` (ring order, not tile order) and sets
// *total to the number of hits found (which may exceed the ring capacity; extra hits are lost).
spm_status_t spm_job_read_hits(spm_job_t* job, spm_hit_t* out, uint32_t cap, uint32_t* total);

// Copies the dump of the current attempt (tiles * 104 bytes, reference tile order) to `out`.
spm_status_t spm_job_read_dump(spm_job_t* job, uint8_t* out, uint64_t cap, uint64_t* written);

// Copies bytes [offset, offset + len) of an internal buffer (SPM_BUF_*) to `out`.
spm_status_t spm_job_read_buffer(spm_job_t* job, int32_t which, uint64_t offset, void* out, uint64_t len);

// Fills `out` with the job's geometry, memory and timings.
spm_status_t spm_job_info(const spm_job_t* job, spm_job_info_t* out);

// CUDA error code of the last failing call on this job (0 if none).
int32_t spm_job_last_cuda_error(const spm_job_t* job);

// Frees the job and its device memory. NULL is ignored.
void spm_job_destroy(spm_job_t* job);

// CUDA error code of the last spm_job_create that failed with SPM_ERR_CUDA on this thread.
int32_t spm_last_cuda_error(void);

// Static description of a status code or of a CUDA error code.
const char* spm_status_string(spm_status_t status);
const char* spm_cuda_error_string(int32_t cuda_error);

// Keyed BLAKE3 of one 64-byte block computed by the device code (self-test of blake3.cuh).
spm_status_t spm_debug_blake3_keyed64(const uint8_t* key32, const uint8_t* msg64, uint8_t* out32);

#ifdef __cplusplus
}
#endif
