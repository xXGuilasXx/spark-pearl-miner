// C ABI between the Rust gpu-worker and libspm_cuda (CUDA C++). Keep this header dependency-free.
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

// ---------------------------------------------------------------------------------------------
// Mining job (M5): fused noised int8 GEMM + per-slice transcript + keyed BLAKE3 + bound compare.
//
// Life cycle: spm_job_create (B side: B_Rᵀ, B_L pairs, B'ᵀ) -> spm_job_set_attempt (A side: A_L,
// A_R pairs, A') -> spm_job_run_chunk until it returns SPM_DONE -> spm_job_read_hits /
// spm_job_read_dump -> next attempt ... -> spm_job_destroy.
//
// The commitment (job key, Merkle roots, V3 salting, seed chain) is computed on the host (v0), so
// the job takes the derived noise seeds: b_noise_seed at creation, a_noise_seed per attempt.
// Every call returns a status code; nothing throws or aborts the process.
// ---------------------------------------------------------------------------------------------

enum {
  SPM_OK = 0,            // chunk done, more chunks remain in the attempt
  SPM_DONE = 1,          // the attempt's last chunk is done: every hash tile was evaluated
  SPM_ABORTED = 2,       // the abort flag stopped the chunk; the attempt is incomplete
  SPM_E_INVALID = -1,    // null pointer or bad argument
  SPM_E_SHAPE = -2,      // m, n, k outside what the kernel supports
  SPM_E_BUDGET = -3,     // the job would exceed its device-memory budget
  SPM_E_NO_ATTEMPT = -4, // run_chunk before set_attempt
  SPM_E_NOT_DUMP = -5,   // read_dump on a job created without dump mode
  SPM_E_CUDA = -6,       // CUDA runtime error (spm_last_cuda_error() has the code)
  SPM_E_TMA = -7,        // cuTensorMapEncodeTiled unavailable or rejected an operand
  SPM_E_OOM = -8,        // device allocation failed
  SPM_E_SIZE = -9        // output buffer has the wrong size
};

// Buffers readable with spm_job_read_buffer (debugging / G0 stage localization).
enum {
  SPM_BUF_A_NOISED = 0,   // A'  m x k s8 (current attempt)
  SPM_BUF_BT_NOISED = 1,  // B'ᵀ n x k s8
  SPM_BUF_A_FACTOR = 2,   // A_L  m x 128 s8 (current attempt)
  SPM_BUF_BT_FACTOR = 3,  // B_Rᵀ n x 128 s8
  SPM_BUF_A_PAIRS = 4,    // A_R  k x {p, q} u8 (current attempt)
  SPM_BUF_B_PAIRS = 5,    // B_L  k x {p, q} u8
  SPM_BUF_A_NOISE = 6,    // E_A  m x k s8 (recomputed on demand)
  SPM_BUF_BT_NOISE = 7,   // E_Bᵀ n x k s8 (recomputed on demand)
  SPM_BUF_A_BASE = 8,     // A    m x k s8 (generated jobs: regenerated on demand)
  SPM_BUF_BT_BASE = 9     // Bᵀ   n x k s8 (generated jobs only)
};

typedef struct spm_job spm_job_t;
typedef struct spm_abort spm_abort_t;

typedef struct {
  uint32_t m;                 // multiple of 64, <= 2^24
  uint32_t n;                 // multiple of 64, <= 2^24
  uint32_t k;                 // multiple of 64, 128 <= k <= 65536; floor(k/128) slices are hashed
  uint32_t dump_mode;         // 1: write one 104-byte TileResult record per hash tile
  uint64_t gen_seed;          // A = fill_int7(gen_seed, DOMAIN_A), Bᵀ = fill_int7(gen_seed, DOMAIN_BT)
  const int8_t* a_host;       // optional m*k row-major A (with bt_host); NULL = generated from gen_seed
  const int8_t* bt_host;      // optional n*k row-major Bᵀ
  uint8_t b_noise_seed[32];
  uint32_t hit_capacity;      // hit ring entries; 0 = 4096
  uint32_t chunk_tiles;       // CTA tiles (128 x 256) per launch; 0 = adaptive
  uint32_t target_chunk_us;   // adaptive chunk target; 0 = 6000
  uint32_t band_rows;         // raster band height in CTA rows; 0 = 16
  uint64_t mem_budget_bytes;  // 0 = 2 GiB
  spm_abort_t* abort_flag;    // optional; polled before every tile and before every chunk
} spm_job_params_t;

typedef struct {
  uint32_t t_rows;
  uint32_t t_cols;
  uint8_t digest[32];  // blake3_keyed(a_noise_seed, transcript), compared as LE U256
} spm_hit_t;

typedef struct {
  uint32_t tile_begin;   // CTA tiles of this chunk, in raster order
  uint32_t tile_end;
  uint32_t tiles_total;  // CTA tiles of the whole attempt
  uint32_t ctas;         // persistent CTAs launched
  float ms;              // kernel time of the chunk (CUDA events)
} spm_chunk_info_t;

typedef struct {
  uint64_t device_bytes;     // device memory held by the job
  uint32_t tiles_m;          // CTA tiles along m (128 rows each)
  uint32_t tiles_n;          // CTA tiles along n (256 columns each)
  uint32_t k_slices;
  uint32_t ctas;             // persistent grid size (SM count)
  uint32_t chunk_tiles;      // current CTA tiles per chunk
  uint32_t regs_per_thread;  // GEMM kernel attributes
  uint32_t local_bytes;      // per-thread local memory (spills + stack)
  uint32_t smem_bytes;       // dynamic shared memory per CTA
  uint32_t threads;          // threads per CTA
} spm_job_info_t;

// Abort flag in host-mapped memory, shared between the control thread and running kernels.
int32_t spm_abort_create(spm_abort_t** out);
void spm_abort_set(spm_abort_t* flag, uint32_t value);
uint32_t spm_abort_get(const spm_abort_t* flag);
void spm_abort_destroy(spm_abort_t* flag);

// Allocates the job, builds B'ᵀ. On success *out owns the job (free with spm_job_destroy).
int32_t spm_job_create(const spm_job_params_t* params, spm_job_t** out);

// New attempt: builds A' for `a_noise_seed`, sets the bound (32 bytes, LE U256), clears the hit
// ring and rewinds the tile cursor. `a_prefix` (optional, <= 4096 bytes) overrides the first
// entries of A (the nonce patch).
int32_t spm_job_set_attempt(spm_job_t* job, const uint8_t a_noise_seed[32], const uint8_t bound[32],
                            const uint8_t* a_prefix, uint32_t a_prefix_len);

// Waits for the next chunk of the attempt and describes it in `info`. Returns SPM_OK, SPM_DONE,
// SPM_ABORTED or an error. Launches are pipelined: while a chunk is waited on, the following one is
// already queued, so after SPM_OK one chunk may still be running (the abort flag stops it within a
// tile). The abort flag is checked before every launch and by every CTA before every tile; an
// aborted chunk is re-run from its first tile by the next call once the flag is cleared (hits of
// the partial chunk may then repeat).
int32_t spm_job_run_chunk(spm_job_t* job, spm_chunk_info_t* info);

// Copies up to `capacity` hits; *total receives the number of hits found so far (it can exceed
// the ring capacity, in which case the extra hits were dropped). Complete for the attempt once
// run_chunk returned SPM_DONE; before that, hits of a still-running chunk may be missing.
int32_t spm_job_read_hits(spm_job_t* job, spm_hit_t* out, uint32_t capacity, uint32_t* total);

// Dump mode: copies the m*n/128 records of 104 bytes (t_rows, t_cols, transcript[16], digest; LE)
// in reference order. `len` must be exactly m*n/128*104.
int32_t spm_job_read_dump(spm_job_t* job, uint8_t* out, uint64_t len);

// Copies one of the SPM_BUF_* buffers; `len` must be its exact size.
int32_t spm_job_read_buffer(spm_job_t* job, int32_t which, uint8_t* out, uint64_t len);

int32_t spm_job_info(const spm_job_t* job, spm_job_info_t* out);

void spm_job_destroy(spm_job_t* job);

// The CUDA error code behind the last SPM_E_CUDA / SPM_E_OOM on this thread (0 if none).
int32_t spm_last_cuda_error(void);

// Static description of a status code.
const char* spm_status_str(int32_t status);

#ifdef __cplusplus
}
#endif
