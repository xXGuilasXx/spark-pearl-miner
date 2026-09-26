// Job life cycle behind the C ABI (spm_cuda.h): device buffers, B-side and A-side preparation,
// TMA descriptors, chunked launches of the fused GEMM + hash kernel, hit ring and dump readback.
#include <cuda.h>
#include <cuda_runtime.h>

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <new>
#include <utility>

#include "spm_cuda.h"

#include "../gemm/gemm_tma.cuh"
#include "../prep/prep.cuh"
#include "blake3.cuh"
#include "splitmix.cuh"

static_assert(sizeof(spm_hit_t) == sizeof(spm::gemm::HitRecord), "hit record layout");
static_assert(spm::gemm::DUMP_WORDS * 4 == 104, "dump record is TileResult::dump_bytes (104 bytes)");

namespace {

thread_local int32_t g_last_cuda_error = 0;

constexpr uint64_t kDefaultBudget = 2ull << 30;
constexpr uint32_t kDefaultHitCapacity = 4096;
// Adaptive chunk target. Chunks of the same size differ in kernel time: per CTA tile 61-94 us at
// ~2330 MHz on 131072^2 x 4096 (mean 72), with isolated chunks up to 116 us when other load on the
// SoC interferes; so the target sits well below the 10 ms cancellation rule.
constexpr uint32_t kDefaultTargetChunkUs = 4500;
constexpr uint32_t kDefaultBand = 16;
constexpr uint32_t kMaxPrefix = 4096;
// Adaptive chunks never go below this many CTA tiles per CTA (shorter chunks are dominated by the
// ramp-up and the tail wave, and their timing is not representative).
constexpr uint32_t kMinTilesPerCta = 4;
// Hard ceiling of adaptive chunks, whatever the target and the timing history: the CTA tiles per
// CTA that take kChunkCeilingUs at the clock floor at a per-SM rate below that of the slow chunks
// (~590 MAC/clk/SM on 131072^2 x 4096 at 2300 MHz; the mean is ~790). That is 60 tiles per CTA at
// k = 4096, so a throttled GPU or an estimate that lags a clock drop still ends its chunks within
// ~8 ms. Isolated chunks slowed by other load on the SoC (507 MAC/clk/SM, 1 of 183) would take
// ~8.8 ms at 1800 MHz: still under the 10 ms rule.
constexpr double kClockFloorMhz = 1800.0;
constexpr double kFloorMacPerClkPerSm = 560.0;
constexpr double kChunkCeilingUs = 8000.0;
// Chunks kept in flight: the next chunk is queued behind the running one, so the GPU does not idle
// while the host reads the finished chunk back and launches the following one.
constexpr uint32_t kPipeline = 2;
// Device counters: {tile counter, status} per pipeline slot, then the hit count.
constexpr uint32_t kCounterHits = 2 * kPipeline, kCounterWords = 2 * kPipeline + 4;

int32_t fail_cuda(cudaError_t e) {
  g_last_cuda_error = static_cast<int32_t>(e);
  (void)cudaGetLastError();  // clear a sticky launch error so later calls report their own
  return e == cudaErrorMemoryAllocation ? SPM_E_OOM : SPM_E_CUDA;
}

#define SPM_TRY(expr)                           \
  do {                                          \
    const cudaError_t spm_err_ = (expr);        \
    if (spm_err_ != cudaSuccess) return fail_cuda(spm_err_); \
  } while (0)

typedef CUresult (*EncodeTiledFn)(CUtensorMap*, CUtensorMapDataType, cuuint32_t, void*,
                                  const cuuint64_t*, const cuuint64_t*, const cuuint32_t*,
                                  const cuuint32_t*, CUtensorMapInterleave, CUtensorMapSwizzle,
                                  CUtensorMapL2promotion, CUtensorMapFloatOOBfill);

// cuTensorMapEncodeTiled through the runtime's driver entry point, so the library does not need
// to link libcuda directly.
EncodeTiledFn encode_tiled() {
  static EncodeTiledFn fn = []() -> EncodeTiledFn {
    void* p = nullptr;
    cudaDriverEntryPointQueryResult q = cudaDriverEntryPointSymbolNotFound;
    if (cudaGetDriverEntryPointByVersion("cuTensorMapEncodeTiled", &p, 12000, cudaEnableDefault, &q) !=
            cudaSuccess ||
        q != cudaDriverEntryPointSuccess) {
      (void)cudaGetLastError();
      return nullptr;
    }
    return reinterpret_cast<EncodeTiledFn>(p);
  }();
  return fn;
}

// rows x k row-major s8 operand, boxes of 64 k-bytes x `box_rows` rows, 64-byte swizzle (the
// layout the kernel's ldmatrix addressing expects).
bool encode_operand(CUtensorMap* map, const int8_t* base, uint32_t rows, uint32_t k,
                    uint32_t box_rows) {
  const EncodeTiledFn fn = encode_tiled();
  if (fn == nullptr) return false;
  const cuuint64_t dims[2] = {k, rows};
  const cuuint64_t strides[1] = {k};
  const cuuint32_t box[2] = {spm::gemm::BK, box_rows};
  const cuuint32_t elem_strides[2] = {1, 1};
  return fn(map, CU_TENSOR_MAP_DATA_TYPE_UINT8, 2, const_cast<int8_t*>(base), dims, strides, box,
            elem_strides, CU_TENSOR_MAP_INTERLEAVE_NONE, spm::gemm::OPERAND_SWIZZLE,
            CU_TENSOR_MAP_L2_PROMOTION_L2_256B, CU_TENSOR_MAP_FLOAT_OOB_FILL_NONE) == CUDA_SUCCESS;
}

spm::prep::Seed seed_from_bytes(const uint8_t bytes[32]) {
  spm::prep::Seed s;
  spm::le_words_from_bytes(bytes, s.w);
  return s;
}

}  // namespace

struct spm_abort {
  uint32_t* host = nullptr;  // cudaHostAlloc'd, mapped
  uint32_t* dev = nullptr;   // device alias of `host`
};

struct spm_job {
  uint32_t m = 0, n = 0, k = 0, k_slices = 0;
  bool dump_mode = false;
  bool generated = true;
  uint64_t gen_seed = 0;
  uint64_t device_bytes = 0;

  int8_t* a_stored = nullptr;   // host-supplied A (m*k), or null when generated
  int8_t* a_noised = nullptr;   // A'  (m*k)
  int8_t* bt_noised = nullptr;  // B'ᵀ (n*k)
  int8_t* a_factor = nullptr;   // A_L  (m*128)
  int8_t* bt_factor = nullptr;  // B_Rᵀ (n*128)
  uint8_t* a_pairs = nullptr;   // k x 2
  uint8_t* b_pairs = nullptr;   // k x 2
  uint8_t* prefix = nullptr;    // kMaxPrefix
  uint32_t prefix_len = 0;
  uint32_t* dump = nullptr;     // m*n/128 records of 26 words
  uint64_t dump_bytes = 0;
  spm::gemm::HitRecord* hits = nullptr;
  uint32_t hit_capacity = 0;
  uint32_t* counters = nullptr;  // kCounterWords

  spm_abort_t* abort_flag = nullptr;
  bool owns_abort = false;

  CUtensorMap tmap_a, tmap_b;
  cudaStream_t stream = nullptr;
  uint64_t budget = 0;

  // Double buffering of the A side (spm_job_prepare_attempt): a spare A', A_L, A_R and prefix,
  // allocated on first use and built on a low-priority stream while chunks of the current attempt
  // run; set_attempt swaps them in when the seed and the prefix match.
  int8_t* alt_a_noised = nullptr;
  int8_t* alt_a_factor = nullptr;
  uint8_t* alt_a_pairs = nullptr;
  uint8_t* alt_prefix = nullptr;
  uint32_t alt_prefix_len = 0;
  CUtensorMap alt_tmap_a;
  cudaStream_t prep_stream = nullptr;
  cudaEvent_t prep_done = nullptr;
  bool prepared = false;  // the spare set holds the A side of prepared_seed + prepared_prefix
  uint8_t prepared_seed[32] = {0};
  uint8_t prepared_prefix[kMaxPrefix] = {0};

  // Chunks in flight, oldest at `head`; slot s owns counters[2s], counters[2s + 1], its events and
  // h_status[s] (pinned copy of its status word, written by the stream after the kernel).
  struct Chunk {
    uint32_t begin = 0, end = 0, grid = 0;
  };
  Chunk inflight[kPipeline];
  cudaEvent_t ev_begin[kPipeline] = {}, ev_end[kPipeline] = {};
  uint32_t* h_status = nullptr;
  uint32_t head = 0, count = 0;
  uint32_t enqueue_tile = 0;  // first tile not yet handed to a launch

  uint8_t b_seed[32] = {0};
  uint32_t key[8] = {0}, bound[8] = {0};
  bool attempt_ready = false;

  uint32_t tiles_m = 0, tiles_n = 0, tiles_total = 0;
  uint32_t next_tile = 0;  // first tile not yet evaluated by a finished chunk
  uint32_t ctas = 0, band = kDefaultBand;
  uint32_t chunk_tiles = 0;
  uint32_t max_chunk_tiles = 0;  // adaptive ceiling (kChunkCeilingUs at the clock floor)
  bool adaptive = true;
  double us_per_tile = 0.0;  // EMA of the kernel time per CTA tile per CTA (adaptive chunks)
  uint32_t target_chunk_us = kDefaultTargetChunkUs;
};

namespace {

void release(spm_job* j) {
  if (j == nullptr) return;
  if (j->stream) cudaStreamSynchronize(j->stream);
  if (j->prep_stream) cudaStreamSynchronize(j->prep_stream);
  cudaFree(j->alt_a_noised);
  cudaFree(j->alt_a_factor);
  cudaFree(j->alt_a_pairs);
  cudaFree(j->alt_prefix);
  if (j->prep_done) cudaEventDestroy(j->prep_done);
  if (j->prep_stream) cudaStreamDestroy(j->prep_stream);
  cudaFree(j->a_stored);
  cudaFree(j->a_noised);
  cudaFree(j->bt_noised);
  cudaFree(j->a_factor);
  cudaFree(j->bt_factor);
  cudaFree(j->a_pairs);
  cudaFree(j->b_pairs);
  cudaFree(j->prefix);
  cudaFree(j->dump);
  cudaFree(j->hits);
  cudaFree(j->counters);
  if (j->h_status) cudaFreeHost(j->h_status);
  for (uint32_t s = 0; s < kPipeline; ++s) {
    if (j->ev_begin[s]) cudaEventDestroy(j->ev_begin[s]);
    if (j->ev_end[s]) cudaEventDestroy(j->ev_end[s]);
  }
  if (j->stream) cudaStreamDestroy(j->stream);
  if (j->owns_abort) spm_abort_destroy(j->abort_flag);
  (void)cudaGetLastError();
  delete j;
}

template <class T>
cudaError_t alloc(spm_job* j, T** ptr, uint64_t bytes) {
  if (bytes == 0) return cudaSuccess;
  void* p = nullptr;
  const cudaError_t e = cudaMalloc(&p, bytes);
  if (e != cudaSuccess) return e;
  *ptr = static_cast<T*>(p);
  j->device_bytes += bytes;
  return cudaSuccess;
}

bool valid_dim(uint32_t x) { return x >= 64 && x % 64 == 0 && x <= (1u << 24); }

// The factor, pairs, prefix and stream an operand build uses.
struct SideBuffers {
  int8_t* factor;
  uint8_t* pairs;
  const uint8_t* prefix;
  uint32_t prefix_len;
  cudaStream_t stream;
};

// The job's current A side (a_side) or its B side, on the job stream.
SideBuffers current_side(const spm_job* j, bool a_side) {
  if (a_side) return SideBuffers{j->a_factor, j->a_pairs, j->prefix, j->prefix_len, j->stream};
  return SideBuffers{j->bt_factor, j->b_pairs, nullptr, 0, j->stream};
}

// A-side / B-side operand build on `sb.stream`.
cudaError_t build_operand(spm_job* j, bool a_side, const SideBuffers& sb,
                          const uint8_t seed_bytes[32], int8_t* out, bool noise_only,
                          bool base_only) {
  const uint32_t rows = a_side ? j->m : j->n;
  const uint32_t label = a_side ? spm::LABEL_A_W0 : spm::LABEL_B_W0;
  spm::prep::OperandSource src;
  src.stored = a_side ? j->a_stored : (j->generated ? nullptr : j->bt_noised);
  src.gen_state = j->gen_seed ^ (a_side ? spm::DOMAIN_A : spm::DOMAIN_BT);
  src.prefix = sb.prefix;
  src.prefix_len = sb.prefix_len;
  if (base_only) {
    if (src.stored != nullptr || !j->generated) return cudaErrorInvalidValue;
    return spm::prep::launch_fill_int7(out, (uint64_t)rows * j->k, src.gen_state, sb.stream);
  }
  if (seed_bytes != nullptr) {
    const spm::prep::Seed seed = seed_from_bytes(seed_bytes);
    cudaError_t e = spm::prep::launch_uniform_factor(sb.factor, rows, seed, label, sb.stream);
    if (e != cudaSuccess) return e;
    e = spm::prep::launch_pairs(sb.pairs, j->k, seed, label, sb.stream);
    if (e != cudaSuccess) return e;
  }
  return spm::prep::launch_noised_operand(out, src, sb.factor, sb.pairs, rows, j->k, noise_only,
                                          sb.stream);
}

bool valid_attempt_args(const spm_job* j, const uint8_t* a_noise_seed, const uint8_t* a_prefix,
                        uint32_t a_prefix_len) {
  if (a_noise_seed == nullptr) return false;
  if (a_prefix_len > kMaxPrefix || (a_prefix_len != 0 && a_prefix == nullptr)) return false;
  return (uint64_t)a_prefix_len <= (uint64_t)j->m * j->k;
}

}  // namespace

extern "C" {

int32_t spm_last_cuda_error(void) { return g_last_cuda_error; }

const char* spm_status_str(int32_t status) {
  switch (status) {
    case SPM_OK: return "ok";
    case SPM_DONE: return "attempt done";
    case SPM_ABORTED: return "aborted";
    case SPM_E_INVALID: return "invalid argument";
    case SPM_E_SHAPE: return "unsupported shape (m, n multiples of 64 up to 2^24; k multiple of 64 in [128, 65536])";
    case SPM_E_BUDGET: return "device-memory budget exceeded";
    case SPM_E_NO_ATTEMPT: return "no attempt set";
    case SPM_E_NOT_DUMP: return "job was not created in dump mode";
    case SPM_E_CUDA: return "CUDA error";
    case SPM_E_TMA: return "TMA descriptor encoding failed";
    case SPM_E_OOM: return "device allocation failed";
    case SPM_E_SIZE: return "buffer size mismatch";
    default: return "unknown status";
  }
}

int32_t spm_abort_create(spm_abort_t** out) {
  if (out == nullptr) return SPM_E_INVALID;
  *out = nullptr;
  spm_abort_t* a = new (std::nothrow) spm_abort_t();
  if (a == nullptr) return SPM_E_OOM;
  void* host = nullptr;
  cudaError_t e = cudaHostAlloc(&host, 64, cudaHostAllocMapped | cudaHostAllocPortable);
  if (e != cudaSuccess) {
    delete a;
    return fail_cuda(e);
  }
  a->host = static_cast<uint32_t*>(host);
  __atomic_store_n(a->host, 0u, __ATOMIC_SEQ_CST);
  void* dev = nullptr;
  e = cudaHostGetDevicePointer(&dev, host, 0);
  if (e != cudaSuccess) {
    cudaFreeHost(host);
    delete a;
    return fail_cuda(e);
  }
  a->dev = static_cast<uint32_t*>(dev);
  *out = a;
  return SPM_OK;
}

void spm_abort_set(spm_abort_t* flag, uint32_t value) {
  if (flag != nullptr && flag->host != nullptr) __atomic_store_n(flag->host, value, __ATOMIC_SEQ_CST);
}

uint32_t spm_abort_get(const spm_abort_t* flag) {
  if (flag == nullptr || flag->host == nullptr) return 0;
  return __atomic_load_n(flag->host, __ATOMIC_SEQ_CST);
}

void spm_abort_destroy(spm_abort_t* flag) {
  if (flag == nullptr) return;
  if (flag->host) cudaFreeHost(flag->host);
  delete flag;
}

int32_t spm_job_create(const spm_job_params_t* params, spm_job_t** out) {
  if (params == nullptr || out == nullptr) return SPM_E_INVALID;
  *out = nullptr;
  const spm_job_params_t& p = *params;
  if (!valid_dim(p.m) || !valid_dim(p.n) || p.k < 128 || p.k > 65536 || p.k % 64 != 0)
    return SPM_E_SHAPE;
  if ((p.a_host == nullptr) != (p.bt_host == nullptr)) return SPM_E_INVALID;

  const uint64_t m = p.m, n = p.n, k = p.k;
  // CTA tile ids travel as int32 (-1 is the stop command).
  const uint64_t cta_tiles = ((m + spm::gemm::BM - 1) / spm::gemm::BM) * ((n + spm::gemm::BN - 1) / spm::gemm::BN);
  if (cta_tiles > (uint64_t)INT32_MAX) return SPM_E_SHAPE;
  const bool generated = p.a_host == nullptr;
  const uint64_t hit_capacity = p.hit_capacity ? p.hit_capacity : kDefaultHitCapacity;
  const uint64_t dump_bytes = p.dump_mode ? m * n / 128 * 104 : 0;
  const uint64_t need = m * k + n * k + (generated ? 0 : m * k) + (m + n) * spm::prep::RANK +
                        4 * k + kMaxPrefix + dump_bytes + hit_capacity * sizeof(spm_hit_t) +
                        kCounterWords * 4;
  const uint64_t budget = p.mem_budget_bytes ? p.mem_budget_bytes : kDefaultBudget;
  if (need > budget) return SPM_E_BUDGET;

  spm_job* j = new (std::nothrow) spm_job();
  if (j == nullptr) return SPM_E_OOM;
  j->m = p.m;
  j->n = p.n;
  j->k = p.k;
  j->k_slices = p.k / spm::gemm::SLICE_K;
  j->dump_mode = p.dump_mode != 0;
  j->generated = generated;
  j->gen_seed = p.gen_seed;
  j->dump_bytes = dump_bytes;
  j->budget = budget;
  j->hit_capacity = (uint32_t)hit_capacity;
  j->band = p.band_rows ? p.band_rows : kDefaultBand;
  j->target_chunk_us = p.target_chunk_us ? p.target_chunk_us : kDefaultTargetChunkUs;
  memcpy(j->b_seed, p.b_noise_seed, 32);
  j->tiles_m = (p.m + spm::gemm::BM - 1) / spm::gemm::BM;
  j->tiles_n = (p.n + spm::gemm::BN - 1) / spm::gemm::BN;
  j->tiles_total = j->tiles_m * j->tiles_n;

  int32_t rc = SPM_OK;
  auto fail = [&](int32_t code) {
    release(j);
    return code;
  };
  int device = 0, sms = 0;
  cudaError_t e = cudaGetDevice(&device);
  if (e == cudaSuccess) e = cudaDeviceGetAttribute(&sms, cudaDevAttrMultiProcessorCount, device);
  if (e != cudaSuccess) return fail(fail_cuda(e));
  j->ctas = (uint32_t)std::max(sms, 1);
  if (p.chunk_tiles) {
    j->adaptive = false;
    j->chunk_tiles = p.chunk_tiles;
  } else {
    const double tile_macs = (double)spm::gemm::BM * spm::gemm::BN * (double)(j->k_slices * spm::gemm::SLICE_K);
    const double ceiling = std::floor(kChunkCeilingUs * kClockFloorMhz * kFloorMacPerClkPerSm / tile_macs);
    j->max_chunk_tiles = (uint32_t)std::min<double>(
        UINT32_MAX, std::max<double>(kMinTilesPerCta, ceiling) * j->ctas);
    // First guess: 1 T-MAC/s per SM (below the measured ~1.8), adapted after every chunk.
    const double tile_us = tile_macs / 1e6;
    const double per_cta = std::max<double>(kMinTilesPerCta, (double)j->target_chunk_us / tile_us);
    j->chunk_tiles = (uint32_t)std::min<double>(per_cta * j->ctas, j->max_chunk_tiles);
  }

  if (p.abort_flag != nullptr) {
    j->abort_flag = p.abort_flag;
  } else {
    rc = spm_abort_create(&j->abort_flag);
    if (rc != SPM_OK) return fail(rc);
    j->owns_abort = true;
  }

  // Load the GEMM kernel now (lazy module loading would otherwise land inside the first chunk).
  cudaFuncAttributes attr;
  e = spm::gemm::gemm_hash_s8_attributes(&attr);
  if (e == cudaSuccess) e = spm::gemm::configure_gemm_hash_s8();
  if (e != cudaSuccess) return fail(fail_cuda(e));

  // Highest stream priority for the chunks: the A-side builds of spm_job_prepare_attempt run on a
  // lowest-priority stream and only take SMs no queued chunk CTA is waiting for.
  int lowest = 0, highest = 0;
  e = cudaDeviceGetStreamPriorityRange(&lowest, &highest);
  if (e == cudaSuccess)
    e = cudaStreamCreateWithPriority(&j->stream, cudaStreamNonBlocking, highest);
  for (uint32_t s = 0; s < kPipeline && e == cudaSuccess; ++s) {
    e = cudaEventCreate(&j->ev_begin[s]);
    if (e == cudaSuccess) e = cudaEventCreate(&j->ev_end[s]);
  }
  if (e == cudaSuccess) {
    void* hs = nullptr;
    e = cudaHostAlloc(&hs, kPipeline * sizeof(uint32_t), cudaHostAllocDefault);
    j->h_status = static_cast<uint32_t*>(hs);
  }
  if (e == cudaSuccess && !generated) e = alloc(j, &j->a_stored, m * k);
  if (e == cudaSuccess) e = alloc(j, &j->a_noised, m * k);
  if (e == cudaSuccess) e = alloc(j, &j->bt_noised, n * k);
  if (e == cudaSuccess) e = alloc(j, &j->a_factor, m * spm::prep::RANK);
  if (e == cudaSuccess) e = alloc(j, &j->bt_factor, n * spm::prep::RANK);
  if (e == cudaSuccess) e = alloc(j, &j->a_pairs, 2 * k);
  if (e == cudaSuccess) e = alloc(j, &j->b_pairs, 2 * k);
  if (e == cudaSuccess) e = alloc(j, &j->prefix, (uint64_t)kMaxPrefix);
  if (e == cudaSuccess && dump_bytes) e = alloc(j, &j->dump, dump_bytes);
  if (e == cudaSuccess) e = alloc(j, &j->hits, hit_capacity * sizeof(spm_hit_t));
  if (e == cudaSuccess) e = alloc(j, &j->counters, (uint64_t)kCounterWords * 4);
  if (e != cudaSuccess) return fail(fail_cuda(e));

  if (!generated) {
    e = cudaMemcpyAsync(j->a_stored, p.a_host, m * k, cudaMemcpyHostToDevice, j->stream);
    if (e == cudaSuccess)
      e = cudaMemcpyAsync(j->bt_noised, p.bt_host, n * k, cudaMemcpyHostToDevice, j->stream);
    if (e != cudaSuccess) return fail(fail_cuda(e));
  }
  // B side, once per job: B_Rᵀ, B_L pairs, B'ᵀ = Bᵀ + E_Bᵀ (in place over the uploaded Bᵀ).
  e = build_operand(j, false, current_side(j, false), j->b_seed, j->bt_noised, false, false);
  if (e == cudaSuccess) e = cudaMemsetAsync(j->counters, 0, kCounterWords * 4, j->stream);
  if (e == cudaSuccess) e = cudaStreamSynchronize(j->stream);
  if (e != cudaSuccess) return fail(fail_cuda(e));

  if (!encode_operand(&j->tmap_a, j->a_noised, p.m, p.k, spm::gemm::BM) ||
      !encode_operand(&j->tmap_b, j->bt_noised, p.n, p.k, spm::gemm::BN))
    return fail(SPM_E_TMA);
  *out = j;
  return SPM_OK;
}

int32_t spm_job_set_attempt(spm_job_t* j, const uint8_t a_noise_seed[32], const uint8_t bound[32],
                            const uint8_t* a_prefix, uint32_t a_prefix_len) {
  if (j == nullptr || bound == nullptr) return SPM_E_INVALID;
  if (!valid_attempt_args(j, a_noise_seed, a_prefix, a_prefix_len)) return SPM_E_INVALID;
  j->attempt_ready = false;
  // Chunks of the previous attempt still in flight finish first (abort them for a faster switch).
  SPM_TRY(cudaStreamSynchronize(j->stream));
  j->head = j->count = 0;
  spm::le_words_from_bytes(a_noise_seed, j->key);
  spm::le_words_from_bytes(bound, j->bound);
  const bool use_prepared =
      j->prepared && memcmp(j->prepared_seed, a_noise_seed, 32) == 0 &&
      j->alt_prefix_len == a_prefix_len &&
      (a_prefix_len == 0 || memcmp(j->prepared_prefix, a_prefix, a_prefix_len) == 0);
  if (use_prepared) {
    // The A side was built ahead on the prep stream: swap it in. The old set becomes the spare; it
    // is idle, since the job stream is drained.
    j->prepared = false;
    SPM_TRY(cudaEventSynchronize(j->prep_done));
    std::swap(j->a_noised, j->alt_a_noised);
    std::swap(j->a_factor, j->alt_a_factor);
    std::swap(j->a_pairs, j->alt_a_pairs);
    std::swap(j->prefix, j->alt_prefix);
    std::swap(j->prefix_len, j->alt_prefix_len);
    std::swap(j->tmap_a, j->alt_tmap_a);
  } else {
    j->prefix_len = a_prefix_len;
    if (a_prefix_len != 0)
      SPM_TRY(cudaMemcpyAsync(j->prefix, a_prefix, a_prefix_len, cudaMemcpyHostToDevice, j->stream));
    SPM_TRY(build_operand(j, true, current_side(j, true), a_noise_seed, j->a_noised, false, false));
  }
  SPM_TRY(cudaMemsetAsync(j->counters, 0, kCounterWords * 4, j->stream));
  SPM_TRY(cudaStreamSynchronize(j->stream));
  j->next_tile = j->enqueue_tile = 0;
  j->attempt_ready = true;
  return SPM_OK;
}

int32_t spm_job_prepare_attempt(spm_job_t* j, const uint8_t a_noise_seed[32], const uint8_t* a_prefix,
                                uint32_t a_prefix_len) {
  if (j == nullptr) return SPM_E_INVALID;
  if (!valid_attempt_args(j, a_noise_seed, a_prefix, a_prefix_len)) return SPM_E_INVALID;
  j->prepared = false;
  if (j->alt_a_noised == nullptr) {
    const uint64_t m = j->m, k = j->k;
    const uint64_t extra = m * k + m * spm::prep::RANK + 2 * k + kMaxPrefix;
    if (j->device_bytes + extra > j->budget) return SPM_E_BUDGET;
    // Lowest priority: the prep kernels take the SMs the GEMM leaves idle at chunk tails, and a
    // queued chunk's CTAs wait at most for one prep block.
    int lowest = 0, highest = 0;
    cudaError_t e = cudaDeviceGetStreamPriorityRange(&lowest, &highest);
    if (e == cudaSuccess && j->prep_stream == nullptr)
      e = cudaStreamCreateWithPriority(&j->prep_stream, cudaStreamNonBlocking, lowest);
    if (e == cudaSuccess && j->prep_done == nullptr)
      e = cudaEventCreateWithFlags(&j->prep_done, cudaEventDisableTiming);
    if (e == cudaSuccess && j->alt_a_factor == nullptr)
      e = alloc(j, &j->alt_a_factor, m * spm::prep::RANK);
    if (e == cudaSuccess && j->alt_a_pairs == nullptr) e = alloc(j, &j->alt_a_pairs, 2 * k);
    if (e == cudaSuccess && j->alt_prefix == nullptr)
      e = alloc(j, &j->alt_prefix, (uint64_t)kMaxPrefix);
    // A' last: it is the "allocated" marker, so a failure above is retried by the next call.
    if (e == cudaSuccess) e = alloc(j, &j->alt_a_noised, m * k);
    if (e != cudaSuccess) return fail_cuda(e);
    if (!encode_operand(&j->alt_tmap_a, j->alt_a_noised, j->m, j->k, spm::gemm::BM)) {
      cudaFree(j->alt_a_noised);
      j->alt_a_noised = nullptr;
      j->device_bytes -= m * k;
      return SPM_E_TMA;
    }
  }
  memcpy(j->prepared_seed, a_noise_seed, 32);
  if (a_prefix_len != 0) memcpy(j->prepared_prefix, a_prefix, a_prefix_len);
  j->alt_prefix_len = a_prefix_len;
  if (a_prefix_len != 0)
    SPM_TRY(cudaMemcpyAsync(j->alt_prefix, a_prefix, a_prefix_len, cudaMemcpyHostToDevice,
                            j->prep_stream));
  const SideBuffers spare{j->alt_a_factor, j->alt_a_pairs, j->alt_prefix, a_prefix_len,
                          j->prep_stream};
  SPM_TRY(build_operand(j, true, spare, a_noise_seed, j->alt_a_noised, false, false));
  SPM_TRY(cudaEventRecord(j->prep_done, j->prep_stream));
  j->prepared = true;
  return SPM_OK;
}

namespace {

// Queues the next chunk of the attempt on the job stream (pipeline slot head + count).
cudaError_t enqueue_chunk(spm_job* j) {
  const uint32_t begin = j->enqueue_tile;
  // Split what is left of the attempt into equal chunks no larger than chunk_tiles.
  const uint64_t left = j->tiles_total - begin;
  const uint64_t cap = std::max<uint32_t>(1, j->chunk_tiles);
  const uint64_t pieces = std::max<uint64_t>(1, (left + cap - 1) / cap);
  const uint32_t end = begin + (uint32_t)((left + pieces - 1) / pieces);
  const uint32_t slot = (j->head + j->count) % kPipeline;
  const uint32_t grid = std::min<uint32_t>(j->ctas, end - begin);

  spm::gemm::Params prm;
  memset(&prm, 0, sizeof(prm));
  prm.m = j->m;
  prm.n = j->n;
  prm.k_slices = j->k_slices;
  prm.tiles_m = j->tiles_m;
  prm.tiles_n = j->tiles_n;
  prm.band = std::max<uint32_t>(1, std::min(j->band, j->tiles_m));
  prm.tile_begin = begin;
  prm.tile_end = end;
  prm.tile_counter = j->counters + 2 * slot;
  prm.status = j->counters + 2 * slot + 1;
  prm.abort_flag = j->abort_flag->dev;
  memcpy(prm.key, j->key, sizeof(prm.key));
  memcpy(prm.bound, j->bound, sizeof(prm.bound));
  prm.dump = j->dump;
  prm.dump_row_stride = j->n / 16;
  prm.hits = j->hits;
  prm.hit_count = j->counters + kCounterHits;
  prm.hit_capacity = j->hit_capacity;

  cudaError_t e = cudaMemsetAsync(j->counters + 2 * slot, 0, 2 * sizeof(uint32_t), j->stream);
  if (e == cudaSuccess) e = cudaEventRecord(j->ev_begin[slot], j->stream);
  if (e == cudaSuccess) e = spm::gemm::launch_gemm_hash_s8(j->tmap_a, j->tmap_b, prm, grid, j->stream);
  if (e == cudaSuccess)
    e = cudaMemcpyAsync(j->h_status + slot, prm.status, sizeof(uint32_t), cudaMemcpyDeviceToHost, j->stream);
  if (e == cudaSuccess) e = cudaEventRecord(j->ev_end[slot], j->stream);
  if (e != cudaSuccess) return e;
  j->inflight[slot].begin = begin;
  j->inflight[slot].end = end;
  j->inflight[slot].grid = grid;
  j->count += 1;
  j->enqueue_tile = end;
  return cudaSuccess;
}

// Drops every queued chunk after an abort or an error: waits for the stream and rewinds the launch
// cursor to the first unfinished tile.
void drain(spm_job* j) {
  cudaStreamSynchronize(j->stream);
  (void)cudaGetLastError();
  j->head = j->count = 0;
  j->enqueue_tile = j->next_tile;
}

}  // namespace

int32_t spm_job_run_chunk(spm_job_t* j, spm_chunk_info_t* info) {
  if (j == nullptr) return SPM_E_INVALID;
  if (!j->attempt_ready) return SPM_E_NO_ATTEMPT;
  if (info != nullptr) {
    info->tile_begin = info->tile_end = j->next_tile;
    info->tiles_total = j->tiles_total;
    info->ctas = 0;
    info->ms = 0.f;
  }
  // Keep the pipeline full while tiles remain and no abort is pending.
  while (j->count < kPipeline && j->enqueue_tile < j->tiles_total && spm_abort_get(j->abort_flag) == 0) {
    const cudaError_t e = enqueue_chunk(j);
    if (e != cudaSuccess) {
      const int32_t rc = fail_cuda(e);
      drain(j);
      return rc;
    }
  }
  if (j->count == 0) return j->next_tile >= j->tiles_total ? SPM_DONE : SPM_ABORTED;

  const uint32_t slot = j->head;
  const spm_job::Chunk c = j->inflight[slot];
  float ms = 0.f;
  cudaError_t e = cudaEventSynchronize(j->ev_end[slot]);
  if (e == cudaSuccess) e = cudaEventElapsedTime(&ms, j->ev_begin[slot], j->ev_end[slot]);
  if (e != cudaSuccess) {
    const int32_t rc = fail_cuda(e);
    drain(j);
    return rc;
  }
  const uint32_t status = j->h_status[slot];
  j->head = (j->head + 1) % kPipeline;
  j->count -= 1;
  if (info != nullptr) {
    info->tile_begin = c.begin;
    info->tile_end = c.end;
    info->ctas = c.grid;
    info->ms = ms;
  }
  if (status & spm::gemm::STATUS_ABORTED) {
    // The chunk stopped early; the next call re-runs it from its first tile.
    drain(j);
    return SPM_ABORTED;
  }

  const uint32_t begin = c.begin, end = c.end, grid = c.grid;
  j->next_tile = end;
  const double per_cta_tiles = (double)(end - begin) / grid;
  if (j->adaptive && ms > 0.f && per_cta_tiles >= 4.0) {
    // Tiles per CTA that fit the target, from a smoothed time per CTA tile (short chunks are
    // skipped: their tail wave inflates the estimate).
    // React quickly to slower chunks (clock drop, a co-resident workload), slowly to faster ones.
    const double sample = (double)ms * 1000.0 / per_cta_tiles;
    const double w = sample > j->us_per_tile ? 0.5 : 0.2;
    j->us_per_tile = j->us_per_tile > 0.0 ? (1.0 - w) * j->us_per_tile + w * sample : sample;
    double next = std::floor((double)j->target_chunk_us / j->us_per_tile) * grid;
    next = std::min(next, 1.25 * j->chunk_tiles);  // grow slowly, shrink at once
    next = std::min<double>(next, j->max_chunk_tiles);
    j->chunk_tiles = (uint32_t)std::max<double>(kMinTilesPerCta * j->ctas, next);
  }
  return j->next_tile >= j->tiles_total ? SPM_DONE : SPM_OK;
}

int32_t spm_job_read_hits(spm_job_t* j, spm_hit_t* out, uint32_t capacity, uint32_t* total) {
  if (j == nullptr || total == nullptr || (capacity != 0 && out == nullptr)) return SPM_E_INVALID;
  uint32_t count = 0;
  SPM_TRY(cudaMemcpy(&count, j->counters + kCounterHits, sizeof(count), cudaMemcpyDeviceToHost));
  *total = count;
  const uint32_t n = std::min(std::min(count, j->hit_capacity), capacity);
  if (n != 0) SPM_TRY(cudaMemcpy(out, j->hits, (size_t)n * sizeof(spm_hit_t), cudaMemcpyDeviceToHost));
  return SPM_OK;
}

int32_t spm_job_read_dump(spm_job_t* j, uint8_t* out, uint64_t len) {
  if (j == nullptr || out == nullptr) return SPM_E_INVALID;
  if (!j->dump_mode) return SPM_E_NOT_DUMP;
  if (len != j->dump_bytes) return SPM_E_SIZE;
  SPM_TRY(cudaStreamSynchronize(j->stream));
  SPM_TRY(cudaMemcpy(out, j->dump, len, cudaMemcpyDeviceToHost));
  return SPM_OK;
}

int32_t spm_job_read_buffer(spm_job_t* j, int32_t which, uint8_t* out, uint64_t len) {
  if (j == nullptr || out == nullptr) return SPM_E_INVALID;
  const uint64_t m = j->m, n = j->n, k = j->k;
  const void* src = nullptr;
  uint64_t size = 0;
  switch (which) {
    case SPM_BUF_A_NOISED: src = j->a_noised; size = m * k; break;
    case SPM_BUF_BT_NOISED: src = j->bt_noised; size = n * k; break;
    case SPM_BUF_A_FACTOR: src = j->a_factor; size = m * spm::prep::RANK; break;
    case SPM_BUF_BT_FACTOR: src = j->bt_factor; size = n * spm::prep::RANK; break;
    case SPM_BUF_A_PAIRS: src = j->a_pairs; size = 2 * k; break;
    case SPM_BUF_B_PAIRS: src = j->b_pairs; size = 2 * k; break;
    case SPM_BUF_A_NOISE:
    case SPM_BUF_A_BASE: size = m * k; break;
    case SPM_BUF_BT_NOISE:
    case SPM_BUF_BT_BASE: size = n * k; break;
    default: return SPM_E_INVALID;
  }
  if (len != size) return SPM_E_SIZE;
  if (src != nullptr) {
    SPM_TRY(cudaStreamSynchronize(j->stream));
    SPM_TRY(cudaMemcpy(out, src, size, cudaMemcpyDeviceToHost));
    return SPM_OK;
  }
  const bool a_side = which == SPM_BUF_A_NOISE || which == SPM_BUF_A_BASE;
  const bool base_only = which == SPM_BUF_A_BASE || which == SPM_BUF_BT_BASE;
  if (base_only && !j->generated) {
    if (!a_side) return SPM_E_INVALID;  // the uploaded Bᵀ was noised in place
    SPM_TRY(cudaMemcpy(out, j->a_stored, size, cudaMemcpyDeviceToHost));
    return SPM_OK;
  }
  int8_t* tmp = nullptr;
  SPM_TRY(cudaMalloc(&tmp, size));
  cudaError_t e =
      build_operand(j, a_side, current_side(j, a_side), nullptr, tmp, !base_only, base_only);
  if (e == cudaSuccess) e = cudaStreamSynchronize(j->stream);
  if (e == cudaSuccess) e = cudaMemcpy(out, tmp, size, cudaMemcpyDeviceToHost);
  cudaFree(tmp);
  if (e != cudaSuccess) return fail_cuda(e);
  return SPM_OK;
}

int32_t spm_job_info(const spm_job_t* j, spm_job_info_t* out) {
  if (j == nullptr || out == nullptr) return SPM_E_INVALID;
  memset(out, 0, sizeof(*out));
  out->device_bytes = j->device_bytes;
  out->tiles_m = j->tiles_m;
  out->tiles_n = j->tiles_n;
  out->k_slices = j->k_slices;
  out->ctas = j->ctas;
  out->chunk_tiles = j->chunk_tiles;
  cudaFuncAttributes attr;
  if (spm::gemm::gemm_hash_s8_attributes(&attr) != cudaSuccess) return fail_cuda(cudaGetLastError());
  out->regs_per_thread = (uint32_t)attr.numRegs;
  out->local_bytes = (uint32_t)attr.localSizeBytes;
  out->smem_bytes = spm::gemm::SMEM_BYTES;
  out->threads = spm::gemm::THREADS;
  return SPM_OK;
}

void spm_job_destroy(spm_job_t* j) { release(j); }

}  // extern "C"
