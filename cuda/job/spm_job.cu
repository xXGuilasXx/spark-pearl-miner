// Mining-job orchestration behind the C ABI of spm_cuda.h: device buffers, B/A side prep, chunked
// launches of the fused GEMM, hit ring and dump read-back. No exception or abort ever crosses the
// ABI: every failure is a status code.
#include "spm_cuda.h"

#include <cuda_runtime.h>

#include <algorithm>
#include <cstring>
#include <new>

#include "../common/blake3.cuh"
#include "../common/splitmix.cuh"
#include "../common/u256.cuh"
#include "../gemm/hash_gemm.cuh"
#include "../prep/prep.cuh"

namespace {

constexpr uint64_t kDefaultBudget = 2ull << 30;  // 2 GiB
constexpr uint32_t kDefaultHitCapacity = 4096;
constexpr uint32_t kMaxPatch = 4096;
constexpr uint32_t kRank = 128;
constexpr uint64_t kRecordBytes = 104;  // TileResult::DUMP_LEN
constexpr double kTargetChunkMs = 8.0;  // adaptive chunks aim here; the contract is <= 10 ms
constexpr double kInitialMacsPerMs = 50e9;  // first-chunk estimate (50 T-MAC/s), then measured
constexpr int kGroupM = 8;              // CTA rows per raster group

// Bytes 8..20 of MiningConfiguration::to_bytes() for our 8x16 pattern (spm_pow::mining_config).
constexpr uint8_t kPatternBytes[12] = {0x07, 0x07, 0, 0, 0, 0, 0x00, 0x01, 0x03, 0x07, 0, 0};

}  // namespace

struct spm_job {
  uint32_t m = 0, n = 0, k = 0, slices = 0;
  bool dump = false;
  uint32_t source = SPM_SOURCE_FILL;
  uint64_t fill_seed = 0;

  cudaStream_t stream = nullptr;
  cudaEvent_t ev0 = nullptr, ev1 = nullptr;

  int8_t* d_a = nullptr;        // A'   m x k
  int8_t* d_bt = nullptr;       // B'ᵀ  n x k
  int8_t* d_a_base = nullptr;   // A    m x k (host source only)
  int8_t* d_a_l = nullptr;      // A_L  m x 128
  int8_t* d_b_rt = nullptr;     // B_Rᵀ n x 128
  uint8_t* d_pairs_a = nullptr; // A_R  k x 2
  uint8_t* d_pairs_b = nullptr; // B_L  k x 2
  int8_t* d_patch = nullptr;    // fill source: override region of A
  uint64_t patch_off = 0;
  uint32_t patch_len = 0;
  uint32_t* d_hit_count = nullptr;
  spm_hit_t* d_hits = nullptr;
  uint32_t hit_capacity = 0;
  uint32_t hit_read = 0;        // hits of this attempt already handed out
  uint32_t hits_total = 0;      // device hit counter as of the last spm_job_read_hits
  uint8_t* d_dump = nullptr;

  uint32_t a_seed[8] = {};
  uint32_t b_seed[8] = {};
  uint32_t bound[8] = {};
  uint8_t job_key[32] = {};

  uint64_t device_bytes = 0;
  uint64_t tiles = 0;
  uint64_t cta_tiles = 0;
  uint64_t next_cta = 0;
  bool attempt_ready = false;
  uint32_t chunk_fixed = 0;
  double ctas_per_ms = 0.0;
  uint32_t wave = 1;
  uint32_t last_chunk = 0;
  float last_chunk_ms = 0.f, last_prep_ms = 0.f, b_prep_ms = 0.f;
  int ctas_per_sm = 0;
  int32_t cuda_error = 0;
};

namespace {

// Last CUDA error seen by this thread (also for failures before a job exists).
thread_local int32_t g_last_cuda_error = 0;

#define SPM_CK(job, expr)                               \
  do {                                                  \
    const cudaError_t spm_e_ = (expr);                  \
    if (spm_e_ != cudaSuccess) {                        \
      (job)->cuda_error = static_cast<int32_t>(spm_e_); \
      g_last_cuda_error = static_cast<int32_t>(spm_e_); \
      return SPM_E_CUDA;                                \
    }                                                   \
  } while (0)

template <class T>
int32_t alloc(spm_job* job, T** ptr, uint64_t bytes) {
  if (bytes == 0) return SPM_OK;
  SPM_CK(job, cudaMalloc(reinterpret_cast<void**>(ptr), bytes));
  job->device_bytes += bytes;
  return SPM_OK;
}

void release(spm_job* job) {
  if (job->stream) cudaStreamSynchronize(job->stream);
  void* bufs[] = {job->d_a,       job->d_bt,      job->d_a_base,    job->d_a_l, job->d_b_rt,
                  job->d_pairs_a, job->d_pairs_b, job->d_patch,     job->d_hits, job->d_dump,
                  job->d_hit_count};
  for (void* b : bufs)
    if (b) cudaFree(b);
  if (job->ev0) cudaEventDestroy(job->ev0);
  if (job->ev1) cudaEventDestroy(job->ev1);
  if (job->stream) cudaStreamDestroy(job->stream);
  delete job;
}

bool config_matches(const uint8_t cfg[52], uint32_t k) {
  const uint32_t cfg_k = static_cast<uint32_t>(cfg[0]) | (static_cast<uint32_t>(cfg[1]) << 8) |
                         (static_cast<uint32_t>(cfg[2]) << 16) | (static_cast<uint32_t>(cfg[3]) << 24);
  if (cfg_k != k) return false;
  if (cfg[4] != (kRank & 0xFF) || cfg[5] != (kRank >> 8)) return false;  // rank 128
  if (cfg[6] != 0 || cfg[7] != 0) return false;                          // Int7xInt7ToInt32
  if (std::memcmp(cfg + 8, kPatternBytes, sizeof kPatternBytes) != 0) return false;
  for (int i = 20; i < 52; ++i)
    if (cfg[i] != 0) return false;  // no MoE trailer
  return true;
}

void compute_job_key(const uint8_t header76[76], const uint8_t config52[52], uint8_t out[32]) {
  uint8_t msg[128];
  std::memcpy(msg, header76, 76);
  std::memcpy(msg + 76, config52, 52);
  uint32_t words[32];
  for (int i = 0; i < 32; ++i) {
    words[i] = static_cast<uint32_t>(msg[4 * i]) | (static_cast<uint32_t>(msg[4 * i + 1]) << 8) |
               (static_cast<uint32_t>(msg[4 * i + 2]) << 16) |
               (static_cast<uint32_t>(msg[4 * i + 3]) << 24);
  }
  uint32_t h[8];
  spm::blake3::hash_128(words, h);
  spm::u256_to_bytes(h, out);
}

spm::prep::Key key_of(const uint32_t w[8]) {
  spm::prep::Key key;
  std::memcpy(key.w, w, sizeof key.w);
  return key;
}

int32_t elapsed_ms(spm_job* job, float* ms) {
  SPM_CK(job, cudaEventSynchronize(job->ev1));
  SPM_CK(job, cudaEventElapsedTime(ms, job->ev0, job->ev1));
  return SPM_OK;
}

int32_t build_b_side(spm_job* job, const int8_t* host_bt, const int8_t* host_a) {
  const spm::prep::Key key = key_of(job->b_seed);
  SPM_CK(job, cudaEventRecord(job->ev0, job->stream));
  SPM_CK(job, spm::prep::launch_uniform_factor(job->d_b_rt, job->n, key, true, job->stream));
  SPM_CK(job, spm::prep::launch_pairs(job->d_pairs_b, job->k, key, true, job->stream));
  spm::prep::OperandSource src{};
  if (job->source == SPM_SOURCE_HOST) {
    // Bᵀ is uploaded into the B'ᵀ buffer and noised in place (it is not needed afterwards).
    SPM_CK(job, cudaMemcpyAsync(job->d_bt, host_bt, static_cast<uint64_t>(job->n) * job->k,
                                cudaMemcpyHostToDevice, job->stream));
    src.base = job->d_bt;
    SPM_CK(job, cudaMemcpyAsync(job->d_a_base, host_a, static_cast<uint64_t>(job->m) * job->k,
                                cudaMemcpyHostToDevice, job->stream));
  } else {
    src.base = nullptr;
    src.fill_stream = job->fill_seed ^ spm::kDomainBt;
  }
  SPM_CK(job, spm::prep::launch_noised_operand(job->d_bt, job->n, job->k, job->d_b_rt,
                                               job->d_pairs_b, src, job->stream));
  SPM_CK(job, cudaEventRecord(job->ev1, job->stream));
  return elapsed_ms(job, &job->b_prep_ms);
}

uint32_t next_chunk_ctas(const spm_job* job) {
  const uint64_t remaining = job->cta_tiles - job->next_cta;
  uint64_t ctas;
  if (job->chunk_fixed != 0) {
    ctas = job->chunk_fixed;
  } else {
    const double want = job->ctas_per_ms * kTargetChunkMs;
    const uint64_t waves = std::max<uint64_t>(1, static_cast<uint64_t>(want / job->wave));
    ctas = waves * job->wave;
  }
  return static_cast<uint32_t>(std::min<uint64_t>(ctas, remaining));
}

}  // namespace

extern "C" {

int32_t spm_last_cuda_error(void) { return g_last_cuda_error; }

const char* spm_cuda_error_str(int32_t code) {
  return cudaGetErrorString(static_cast<cudaError_t>(code));
}

const char* spm_status_str(int32_t code) {
  switch (code) {
    case SPM_OK: return "ok";
    case SPM_CHUNK_MORE: return "chunk done, attempt has more chunks";
    case SPM_CHUNK_DONE: return "attempt complete";
    case SPM_ABORTED: return "aborted between chunks";
    case SPM_E_INVALID: return "invalid argument";
    case SPM_E_SHAPE: return "unsupported shape or configuration";
    case SPM_E_BUDGET: return "device-memory budget exceeded";
    case SPM_E_CUDA: return "CUDA runtime error";
    case SPM_E_STATE: return "call out of order";
    case SPM_E_MODE: return "not available in this mode";
    case SPM_E_ALLOC: return "host allocation failed";
    default: return "unknown status";
  }
}

int32_t spm_job_create(const spm_job_params_t* params, spm_job_t** out) {
  if (params == nullptr || out == nullptr) return SPM_E_INVALID;
  *out = nullptr;
  if (params->abi_version != SPM_ABI_VERSION) return SPM_E_INVALID;
  const uint32_t m = params->m, n = params->n, k = params->k;
  if (m == 0 || n == 0 || m % 128 != 0 || n % 128 != 0) return SPM_E_SHAPE;
  if (k < kRank || k > 65536 || k % 64 != 0) return SPM_E_SHAPE;
  const uint64_t cta_tiles = static_cast<uint64_t>(m / 128) * (n / 128);
  if (cta_tiles >= (1ull << 31)) return SPM_E_SHAPE;
  if (!config_matches(params->config52, k)) return SPM_E_SHAPE;
  if (params->source != SPM_SOURCE_FILL && params->source != SPM_SOURCE_HOST) return SPM_E_INVALID;
  if (params->source == SPM_SOURCE_HOST && (params->host_a == nullptr || params->host_bt == nullptr))
    return SPM_E_INVALID;
  if (params->dump_mode > 1) return SPM_E_INVALID;

  const bool dump = params->dump_mode == 1;
  const uint32_t hit_capacity = params->hit_capacity ? params->hit_capacity : kDefaultHitCapacity;
  const uint64_t tiles = static_cast<uint64_t>(m) * n / 128;
  const uint64_t mk = static_cast<uint64_t>(m) * k, nk = static_cast<uint64_t>(n) * k;
  const uint64_t need = mk + nk + (params->source == SPM_SOURCE_HOST ? mk : 0) +
                        static_cast<uint64_t>(m) * kRank + static_cast<uint64_t>(n) * kRank +
                        4ull * k + (params->source == SPM_SOURCE_FILL ? kMaxPatch : 0) + 4 +
                        static_cast<uint64_t>(hit_capacity) * sizeof(spm_hit_t) +
                        (dump ? tiles * kRecordBytes : 0);
  const uint64_t budget = params->mem_budget_bytes ? params->mem_budget_bytes : kDefaultBudget;
  if (need > budget) return SPM_E_BUDGET;

  spm_job* job = new (std::nothrow) spm_job();
  if (job == nullptr) return SPM_E_ALLOC;
  job->m = m;
  job->n = n;
  job->k = k;
  job->slices = k / kRank;
  job->dump = dump;
  job->source = params->source;
  job->fill_seed = params->fill_seed;
  job->hit_capacity = hit_capacity;
  job->tiles = tiles;
  job->cta_tiles = cta_tiles;
  job->chunk_fixed = params->chunk_ctas;
  spm::u256_from_bytes(params->b_noise_seed, job->b_seed);
  spm::u256_from_bytes(params->bound, job->bound);
  compute_job_key(params->header76, params->config52, job->job_key);

  int32_t rc = SPM_OK;
  auto fail = [&](int32_t code) {
    if (code != SPM_OK) release(job);
    return code;
  };
  cudaError_t e = cudaStreamCreateWithFlags(&job->stream, cudaStreamNonBlocking);
  if (e == cudaSuccess) e = cudaEventCreate(&job->ev0);
  if (e == cudaSuccess) e = cudaEventCreate(&job->ev1);
  if (e != cudaSuccess) {
    job->cuda_error = e;
    g_last_cuda_error = e;
    return fail(SPM_E_CUDA);
  }
  if ((rc = alloc(job, &job->d_a, mk)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_bt, nk)) != SPM_OK) return fail(rc);
  if (job->source == SPM_SOURCE_HOST && (rc = alloc(job, &job->d_a_base, mk)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_a_l, static_cast<uint64_t>(m) * kRank)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_b_rt, static_cast<uint64_t>(n) * kRank)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_pairs_a, 2ull * k)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_pairs_b, 2ull * k)) != SPM_OK) return fail(rc);
  if (job->source == SPM_SOURCE_FILL && (rc = alloc(job, &job->d_patch, kMaxPatch)) != SPM_OK)
    return fail(rc);
  if ((rc = alloc(job, &job->d_hit_count, 4)) != SPM_OK) return fail(rc);
  if ((rc = alloc(job, &job->d_hits, static_cast<uint64_t>(hit_capacity) * sizeof(spm_hit_t))) != SPM_OK)
    return fail(rc);
  if (dump && (rc = alloc(job, &job->d_dump, tiles * kRecordBytes)) != SPM_OK) return fail(rc);

  int sm_count = 0;
  e = cudaDeviceGetAttribute(&sm_count, cudaDevAttrMultiProcessorCount, 0);
  if (e != cudaSuccess) {
    job->cuda_error = e;
    g_last_cuda_error = e;
    return fail(SPM_E_CUDA);
  }
  job->ctas_per_sm = spm::gemm::hash_gemm_ctas_per_sm(dump);
  if (job->ctas_per_sm <= 0) {
    job->cuda_error = static_cast<int32_t>(cudaGetLastError());
    g_last_cuda_error = job->cuda_error;
    return fail(SPM_E_CUDA);
  }
  job->wave = static_cast<uint32_t>(sm_count * job->ctas_per_sm);
  job->ctas_per_ms = kInitialMacsPerMs / (128.0 * 128.0 * kRank * job->slices);

  if ((rc = build_b_side(job, params->host_bt, params->host_a)) != SPM_OK) return fail(rc);
  *out = job;
  return SPM_OK;
}

int32_t spm_job_patch_a(spm_job_t* job, uint64_t offset, const int8_t* bytes, uint32_t len) {
  if (job == nullptr || (len != 0 && bytes == nullptr) || len > kMaxPatch) return SPM_E_INVALID;
  const uint64_t mk = static_cast<uint64_t>(job->m) * job->k;
  if (offset > mk || len > mk - offset) return SPM_E_INVALID;
  if (job->source == SPM_SOURCE_HOST) {
    if (len != 0)
      SPM_CK(job, cudaMemcpyAsync(job->d_a_base + offset, bytes, len, cudaMemcpyHostToDevice,
                                  job->stream));
  } else {
    if (len != 0)
      SPM_CK(job, cudaMemcpyAsync(job->d_patch, bytes, len, cudaMemcpyHostToDevice, job->stream));
    job->patch_off = offset;
    job->patch_len = len;
  }
  SPM_CK(job, cudaStreamSynchronize(job->stream));
  job->attempt_ready = false;  // A' is stale until the next set_attempt
  return SPM_OK;
}

int32_t spm_job_set_attempt(spm_job_t* job, const uint8_t a_noise_seed[32], const uint8_t bound[32]) {
  if (job == nullptr || a_noise_seed == nullptr) return SPM_E_INVALID;
  job->attempt_ready = false;
  spm::u256_from_bytes(a_noise_seed, job->a_seed);
  if (bound != nullptr) spm::u256_from_bytes(bound, job->bound);
  const spm::prep::Key key = key_of(job->a_seed);
  SPM_CK(job, cudaEventRecord(job->ev0, job->stream));
  SPM_CK(job, spm::prep::launch_uniform_factor(job->d_a_l, job->m, key, false, job->stream));
  SPM_CK(job, spm::prep::launch_pairs(job->d_pairs_a, job->k, key, false, job->stream));
  spm::prep::OperandSource src{};
  if (job->source == SPM_SOURCE_HOST) {
    src.base = job->d_a_base;
  } else {
    src.base = nullptr;
    src.fill_stream = job->fill_seed ^ spm::kDomainA;
    src.patch = job->d_patch;
    src.patch_off = job->patch_off;
    src.patch_len = job->patch_len;
  }
  SPM_CK(job, spm::prep::launch_noised_operand(job->d_a, job->m, job->k, job->d_a_l,
                                               job->d_pairs_a, src, job->stream));
  SPM_CK(job, cudaMemsetAsync(job->d_hit_count, 0, sizeof(uint32_t), job->stream));
  if (job->dump)
    SPM_CK(job, cudaMemsetAsync(job->d_dump, 0xFF, job->tiles * kRecordBytes, job->stream));
  SPM_CK(job, cudaEventRecord(job->ev1, job->stream));
  const int32_t rc = elapsed_ms(job, &job->last_prep_ms);
  if (rc != SPM_OK) return rc;
  job->next_cta = 0;
  job->hit_read = 0;
  job->hits_total = 0;
  job->attempt_ready = true;
  return SPM_OK;
}

int32_t spm_job_run_chunk(spm_job_t* job) {
  if (job == nullptr) return SPM_E_INVALID;
  if (!job->attempt_ready) return SPM_E_STATE;
  if (job->next_cta >= job->cta_tiles) return SPM_CHUNK_DONE;

  const uint32_t ctas = next_chunk_ctas(job);
  spm::gemm::HashGemmParams p{};
  p.a = job->d_a;
  p.bt = job->d_bt;
  p.m = static_cast<int32_t>(job->m);
  p.n = static_cast<int32_t>(job->n);
  p.k = static_cast<int32_t>(job->k);
  p.slices = static_cast<int32_t>(job->slices);
  p.tiles_m = static_cast<int32_t>(job->m / 128);
  p.tiles_n = static_cast<int32_t>(job->n / 128);
  p.total_ctas = static_cast<int32_t>(job->cta_tiles);
  p.cta_base = static_cast<int32_t>(job->next_cta);
  p.group_m = kGroupM;
  std::memcpy(p.key, job->a_seed, sizeof p.key);
  std::memcpy(p.bound, job->bound, sizeof p.bound);
  p.dump = job->d_dump;
  p.hit_count = job->d_hit_count;
  p.hits = job->d_hits;
  p.hit_capacity = job->hit_capacity;

  SPM_CK(job, cudaEventRecord(job->ev0, job->stream));
  SPM_CK(job, spm::gemm::launch_hash_gemm(p, static_cast<int>(ctas), job->dump, job->stream));
  SPM_CK(job, cudaEventRecord(job->ev1, job->stream));
  float ms = 0.f;
  const int32_t rc = elapsed_ms(job, &ms);
  if (rc != SPM_OK) return rc;
  job->last_chunk = ctas;
  job->last_chunk_ms = ms;
  // Re-estimate the rate from full-size chunks only (a short tail chunk under-fills the GPU).
  if (job->chunk_fixed == 0 && ctas >= 4 * job->wave && ms > 0.05f) {
    const double rate = static_cast<double>(ctas) / ms;
    job->ctas_per_ms = 0.5 * job->ctas_per_ms + 0.5 * rate;
  }
  job->next_cta += ctas;
  return job->next_cta >= job->cta_tiles ? SPM_CHUNK_DONE : SPM_CHUNK_MORE;
}

int32_t spm_job_run(spm_job_t* job, const uint32_t* abort_flag) {
  if (job == nullptr) return SPM_E_INVALID;
  for (;;) {
    if (abort_flag != nullptr && __atomic_load_n(abort_flag, __ATOMIC_ACQUIRE) != 0) return SPM_ABORTED;
    const int32_t rc = spm_job_run_chunk(job);
    if (rc == SPM_CHUNK_DONE) return SPM_OK;
    if (rc != SPM_CHUNK_MORE) return rc;
  }
}

int32_t spm_job_read_hits(spm_job_t* job, spm_hit_t* out, uint32_t cap, uint32_t* n_out,
                          uint32_t* lost_out) {
  if (job == nullptr || n_out == nullptr || (cap != 0 && out == nullptr)) return SPM_E_INVALID;
  *n_out = 0;
  if (lost_out) *lost_out = 0;
  if (job->dump) return SPM_E_MODE;
  uint32_t count = 0;
  SPM_CK(job, cudaMemcpyAsync(&count, job->d_hit_count, sizeof count, cudaMemcpyDeviceToHost,
                              job->stream));
  SPM_CK(job, cudaStreamSynchronize(job->stream));
  job->hits_total = count;
  uint32_t avail = count - job->hit_read;
  uint32_t lost = 0;
  if (avail > job->hit_capacity) {
    lost = avail - job->hit_capacity;
    job->hit_read += lost;
    avail = job->hit_capacity;
  }
  const uint32_t take = std::min(avail, cap);
  uint32_t done = 0;
  while (done < take) {
    const uint32_t slot = (job->hit_read + done) % job->hit_capacity;
    const uint32_t run = std::min(take - done, job->hit_capacity - slot);
    SPM_CK(job, cudaMemcpyAsync(out + done, job->d_hits + slot, run * sizeof(spm_hit_t),
                                cudaMemcpyDeviceToHost, job->stream));
    done += run;
  }
  SPM_CK(job, cudaStreamSynchronize(job->stream));
  job->hit_read += take;
  *n_out = take;
  if (lost_out) *lost_out = lost;
  return SPM_OK;
}

int32_t spm_job_read_dump(spm_job_t* job, uint8_t* out, uint64_t len) {
  if (job == nullptr || out == nullptr) return SPM_E_INVALID;
  if (!job->dump) return SPM_E_MODE;
  if (len != job->tiles * kRecordBytes) return SPM_E_INVALID;
  SPM_CK(job, cudaMemcpyAsync(out, job->d_dump, len, cudaMemcpyDeviceToHost, job->stream));
  SPM_CK(job, cudaStreamSynchronize(job->stream));
  return SPM_OK;
}

int32_t spm_job_read_debug(spm_job_t* job, int32_t which, void* out, uint64_t len) {
  if (job == nullptr || out == nullptr) return SPM_E_INVALID;
  const void* src = nullptr;
  uint64_t size = 0;
  switch (which) {
    case SPM_DEBUG_A_NOISED: src = job->d_a; size = static_cast<uint64_t>(job->m) * job->k; break;
    case SPM_DEBUG_BT_NOISED: src = job->d_bt; size = static_cast<uint64_t>(job->n) * job->k; break;
    case SPM_DEBUG_A_L: src = job->d_a_l; size = static_cast<uint64_t>(job->m) * kRank; break;
    case SPM_DEBUG_B_RT: src = job->d_b_rt; size = static_cast<uint64_t>(job->n) * kRank; break;
    case SPM_DEBUG_PAIRS_A: src = job->d_pairs_a; size = 2ull * job->k; break;
    case SPM_DEBUG_PAIRS_B: src = job->d_pairs_b; size = 2ull * job->k; break;
    default: return SPM_E_INVALID;
  }
  if (len != size) return SPM_E_INVALID;
  SPM_CK(job, cudaMemcpyAsync(out, src, size, cudaMemcpyDeviceToHost, job->stream));
  SPM_CK(job, cudaStreamSynchronize(job->stream));
  return SPM_OK;
}

int32_t spm_job_get_info(const spm_job_t* job, spm_job_info_t* out) {
  if (job == nullptr || out == nullptr) return SPM_E_INVALID;
  std::memset(out, 0, sizeof *out);
  out->m = job->m;
  out->n = job->n;
  out->k = job->k;
  out->slices = job->slices;
  out->tiles = job->tiles;
  out->cta_tiles = job->cta_tiles;
  out->next_cta = job->next_cta;
  out->device_bytes = job->device_bytes;
  out->chunk_ctas = job->last_chunk;
  out->ctas_per_sm = static_cast<uint32_t>(job->ctas_per_sm);
  out->last_chunk_ms = job->last_chunk_ms;
  out->last_prep_ms = job->last_prep_ms;
  out->b_prep_ms = job->b_prep_ms;
  out->hits_total = job->hits_total;
  std::memcpy(out->job_key, job->job_key, sizeof out->job_key);
  out->cuda_error = job->cuda_error;
  out->smem_bytes = static_cast<uint32_t>(spm::gemm::hash_gemm_smem_bytes());
  return SPM_OK;
}

void spm_job_destroy(spm_job_t* job) {
  if (job != nullptr) release(job);
}

}  // extern "C"
