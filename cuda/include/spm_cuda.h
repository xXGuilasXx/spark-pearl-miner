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
// Mining job: noised operands, fused GEMM + transcript + BLAKE3 + bound compare.
//
// Lifecycle (one thread drives a job; only the abort flag may be written by another thread):
//   spm_job_create        allocates the job, builds B_Rᵀ, the B pairs and B'ᵀ = Bᵀ + E_Bᵀ (B side)
//   spm_job_patch_a       optional: overrides a range of A (the nonce in chunk 0)
//   spm_job_set_attempt   builds A_L, the A pairs and A' = A + E_A for a_noise_seed (A side),
//                         resets the hit ring and the chunk cursor
//   spm_job_run_chunk     one launch chunk (<= ~10 ms at the default job shape), or
//   spm_job_run           chunks until the attempt is done, polling *abort_flag between chunks
//   spm_job_read_hits / spm_job_read_dump
//   spm_job_destroy
// Every function returns a status code below and never throws or aborts the process.
// ---------------------------------------------------------------------------------------------

#define SPM_ABI_VERSION 1u

enum {
  SPM_OK = 0,
  SPM_CHUNK_MORE = 1,   // spm_job_run_chunk: the attempt has more chunks
  SPM_CHUNK_DONE = 2,   // spm_job_run_chunk: the attempt is complete
  SPM_ABORTED = 3,      // spm_job_run: stopped between chunks because *abort_flag != 0
  SPM_E_INVALID = -1,   // null pointer, bad ABI version or invalid argument
  SPM_E_SHAPE = -2,     // m, n, k or config52 not supported (m, n multiples of 128; k % 64 == 0,
                        // 128 <= k <= 65536; m*k and n*k < 2^31; r = 128 and our 8x16 pattern)
  SPM_E_BUDGET = -3,    // the job would exceed its device-memory budget
  SPM_E_CUDA = -4,      // a CUDA runtime call failed (spm_job_info_t.cuda_error has the code)
  SPM_E_STATE = -5,     // call out of order (run before set_attempt, ...)
  SPM_E_MODE = -6,      // not available in this mode (dump read on a mining job, ...)
  SPM_E_ALLOC = -7      // host allocation failed
};

enum {
  SPM_SOURCE_FILL = 0,  // A and Bᵀ from spm_cpuref::fill_int7(fill_seed, DOMAIN_A / DOMAIN_BT)
  SPM_SOURCE_HOST = 1   // A and Bᵀ copied from host_a / host_bt
};

enum {
  SPM_DEBUG_A_NOISED = 0,   // A'   m x k   s8
  SPM_DEBUG_BT_NOISED = 1,  // B'ᵀ  n x k   s8
  SPM_DEBUG_A_L = 2,        // A_L  m x 128 s8
  SPM_DEBUG_B_RT = 3,       // B_Rᵀ n x 128 s8
  SPM_DEBUG_PAIRS_A = 4,    // A_R  k x (p, q) u8
  SPM_DEBUG_PAIRS_B = 5     // B_L  k x (p, q) u8
};

typedef struct spm_job spm_job_t;  // opaque

typedef struct {
  uint32_t abi_version;        // SPM_ABI_VERSION
  uint32_t m, n, k;
  uint32_t dump_mode;          // 1: every tile writes its 104-byte record (G0/debug); 0: mining
  uint32_t source;             // SPM_SOURCE_FILL or SPM_SOURCE_HOST
  uint64_t fill_seed;          // SPM_SOURCE_FILL: the Problem::generate seed
  const int8_t* host_a;        // SPM_SOURCE_HOST: m*k entries, row major (copied during create)
  const int8_t* host_bt;       // SPM_SOURCE_HOST: n*k entries, row major (copied during create)
  uint8_t header76[76];        // IncompleteBlockHeader::to_bytes() (job key)
  uint8_t config52[52];        // MiningConfiguration::to_bytes(); checked against the kernel
  uint8_t b_noise_seed[32];    // from the CPU commitment
  uint8_t bound[32];           // initial difficulty bound, LE U256 (hit iff digest <= bound)
  uint32_t hit_capacity;       // hit-ring records (0 = 4096)
  uint32_t chunk_ctas;         // 128x128 CTA tiles per launch chunk (0 = adaptive, ~8 ms)
  uint64_t mem_budget_bytes;   // device-memory budget (0 = 2 GiB)
} spm_job_params_t;

typedef struct {
  uint32_t t_rows;
  uint32_t t_cols;
  uint8_t digest[32];          // blake3_keyed(a_noise_seed, transcript), LE U256
} spm_hit_t;

typedef struct {
  uint32_t m, n, k, slices;
  uint64_t tiles;              // hash tiles (m*n/128)
  uint64_t cta_tiles;          // 128x128 CTA tiles of one attempt
  uint64_t next_cta;           // progress of the current attempt
  uint64_t device_bytes;       // device memory held by the job
  uint32_t chunk_ctas;         // size of the last launched chunk
  uint32_t ctas_per_sm;        // resident CTAs per SM of the GEMM kernel
  float last_chunk_ms;         // GPU time of the last chunk
  float last_prep_ms;          // A side prep of the last spm_job_set_attempt
  float b_prep_ms;             // B side prep (spm_job_create)
  uint32_t hits_total;         // hits of the current attempt as of the last spm_job_read_hits
  uint8_t job_key[32];         // blake3(header76 || config52), computed by the library
  int32_t cuda_error;          // last CUDA error (cudaError_t), 0 if none
  uint32_t smem_bytes;         // dynamic shared memory per CTA of the GEMM kernel
  float attempt_gpu_ms;        // GPU time of the computed chunks of the current attempt
  float attempt_max_chunk_ms;  // longest chunk of the current attempt
  uint32_t attempt_chunks;     // computed chunks of the current attempt
  uint32_t reserved;
} spm_job_info_t;

int32_t spm_job_create(const spm_job_params_t* params, spm_job_t** out);

// Overrides entries [offset, offset + len) of A (len <= 4096). SPM_SOURCE_HOST: written into the
// stored A; SPM_SOURCE_FILL: kept as the single override region on top of the generated A (a new
// call replaces it). Takes effect at the next spm_job_set_attempt.
int32_t spm_job_patch_a(spm_job_t* job, uint64_t offset, const int8_t* bytes, uint32_t len);

// A side prep for `a_noise_seed`; `bound` may be NULL to keep the current bound.
int32_t spm_job_set_attempt(spm_job_t* job, const uint8_t a_noise_seed[32], const uint8_t bound[32]);

// Launches the next chunk and waits for it: SPM_CHUNK_MORE, SPM_CHUNK_DONE or an error.
int32_t spm_job_run_chunk(spm_job_t* job);

// Runs chunks until the attempt is done (SPM_OK) or *abort_flag (may be NULL; read with acquire
// semantics before every chunk) becomes non-zero (SPM_ABORTED): the latency is the rest of the
// running chunk, and calling spm_job_run again resumes exactly where the attempt stopped.
int32_t spm_job_run(spm_job_t* job, const uint32_t* abort_flag);

// Copies up to `cap` hits recorded since the previous call into `out`; `*n_out` receives the
// count, `*lost_out` (may be NULL) the hits overwritten in the ring before they were read.
int32_t spm_job_read_hits(spm_job_t* job, spm_hit_t* out, uint32_t cap, uint32_t* n_out,
                          uint32_t* lost_out);

// Dump mode: copies tiles * 104 bytes (TileResult::dump_bytes records, reference order).
int32_t spm_job_read_dump(spm_job_t* job, uint8_t* out, uint64_t len);

// Copies one of the SPM_DEBUG_* buffers (len must be its exact size).
int32_t spm_job_read_debug(spm_job_t* job, int32_t which, void* out, uint64_t len);

int32_t spm_job_get_info(const spm_job_t* job, spm_job_info_t* out);

void spm_job_destroy(spm_job_t* job);

// Static description of a status code.
const char* spm_status_str(int32_t code);

// Last CUDA error (cudaError_t) that made a spm_job_* call of this thread fail, 0 if none; and its
// static description.
int32_t spm_last_cuda_error(void);
const char* spm_cuda_error_str(int32_t code);

#ifdef __cplusplus
}
#endif
