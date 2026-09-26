// Job object behind the C ABI (spm_cuda.h): buffers, B-side prep at creation, A-side prep per
// attempt, chunked launches of the fused kernel, hit ring and debug dump read-back.
//
// Nothing here throws: allocations use nothrow new, every CUDA call is checked and turned into
// SPM_ERR_CUDA with the CUDA code kept on the job (or per thread for spm_job_create).
#include <chrono>
#include <cstdint>
#include <cstring>
#include <new>
#include <thread>

#include <cuda_runtime.h>

#include "blake3.cuh"
#include "noise_hash.cuh"
#include "spm_cuda.h"
#include "spm_internal.h"
#include "splitmix.cuh"

// The Rust bindings (crates/spm-gpu/src/ffi.rs) mirror these layouts; its tests pin the same sizes.
static_assert(sizeof(spm_job_params_t) == 80, "spm_job_params_t layout changed");
static_assert(sizeof(spm_hit_t) == 40, "spm_hit_t layout changed");
static_assert(sizeof(spm_job_info_t) == 88, "spm_job_info_t layout changed");
static_assert(sizeof(spm::DeviceHit) == 40, "DeviceHit layout changed");

namespace {

using spm::DeviceHit;
using spm::GemmArgs;
using spm::GemmGeometry;
using spm::Words8;

constexpr uint32_t kDefaultHitCapacity = 4096;
constexpr uint32_t kMaxHitCapacity = 1u << 20;
// Auto chunk size: whole waves, at most kChunkTargetMs long if the kernel ran at the measured
// register-only IMMA peak (108.6 T-MAC/s at stock clocks / 48 SMs). After every chunk the size is
// re-derived from the measured rate so that chunks keep lasting about kChunkTargetMs when the GPU
// is slower (lower clocks, another context time-slicing, DRAM contention from the CPU): the
// cancellation limit is 10 ms per chunk and up to two chunks are in flight.
constexpr double kChunkTargetMs = 4.0;
constexpr double kPerSmMacRateCeiling = 2.26e12;
constexpr uint32_t kMinK = 2048, kMaxK = 65536, kMaxDim = 1u << 24;
constexpr size_t kAlign = 256;

thread_local int32_t g_last_create_cuda_error = 0;

// Expected MiningConfiguration::to_bytes() for our configuration, except bytes 0..4 (= k).
constexpr uint8_t kConfigTail[48] = {
    0x80, 0x00,                          // rank 128 (u16 LE)
    0x00, 0x00,                          // MMAType::Int7xInt7ToInt32
    0x07, 0x07, 0x00, 0x00, 0x00, 0x00,  // rows pattern {0, 8, ..., 56}
    0x00, 0x01, 0x03, 0x07, 0x00, 0x00,  // cols pattern {0, 1, 8, 9, ..., 56, 57}
    // 32 zero bytes: no MoE
};

Words8 words_from_bytes(const uint8_t* b) {
  Words8 w{};
  for (int i = 0; i < 8; ++i) {
    w.w[i] = static_cast<uint32_t>(b[4 * i]) | (static_cast<uint32_t>(b[4 * i + 1]) << 8) |
             (static_cast<uint32_t>(b[4 * i + 2]) << 16) | (static_cast<uint32_t>(b[4 * i + 3]) << 24);
  }
  return w;
}

void bytes_from_words(const uint32_t* w, uint8_t* b) {
  for (int i = 0; i < 8; ++i) {
    b[4 * i] = static_cast<uint8_t>(w[i]);
    b[4 * i + 1] = static_cast<uint8_t>(w[i] >> 8);
    b[4 * i + 2] = static_cast<uint8_t>(w[i] >> 16);
    b[4 * i + 3] = static_cast<uint8_t>(w[i] >> 24);
  }
}

size_t align_up(size_t x) { return (x + kAlign - 1) / kAlign * kAlign; }

bool entries_in_signal_range(const int8_t* p, uint64_t len) {
  // The verifier accepts [-64, 64]; A' = A + E_A stays in s8 only within it.
  for (uint64_t i = 0; i < len; ++i) {
    if (p[i] < -64 || p[i] > 64) return false;
  }
  return true;
}

}  // namespace

struct spm_job {
  uint32_t m = 0, n = 0, k = 0;
  bool dump = false;
  GemmGeometry geo{};
  uint32_t sm_count = 0;
  uint32_t cta_tiles = 0;
  uint32_t chunk_ctas = 0;  // size of the next chunk
  uint32_t chunk_max = 0;   // auto mode: size at the IMMA peak rate (upper bound)
  uint32_t wave = 0;        // resident CTAs of the whole GPU (sm_count * ctas_per_sm)
  bool adaptive = false;    // auto chunk size (params->chunk_ctas == 0)
  uint32_t slot_ctas[2] = {0, 0};  // CTA count of the chunk timed by each event pair
  uint32_t cursor = 0;
  bool attempt_ready = false;

  void* arena = nullptr;
  uint64_t device_bytes = 0;
  int8_t* a_base = nullptr;
  int8_t* a_noised = nullptr;
  int8_t* bt_noised = nullptr;
  int8_t* a_l = nullptr;
  int8_t* b_rt = nullptr;
  uint8_t* a_pairs = nullptr;
  uint8_t* b_pairs = nullptr;
  uint32_t* hit_count = nullptr;
  uint32_t* abort_dev = nullptr;  // device abort word polled by the fused kernel's CTAs
  DeviceHit* hits = nullptr;
  uint32_t hit_capacity = 0;
  uint8_t* dump_buf = nullptr;

  Words8 key{};
  Words8 bound{};

  cudaStream_t stream = nullptr;
  cudaStream_t side_stream = nullptr;  // raises the abort word while chunks run on `stream`
  cudaEvent_t ev[4] = {nullptr, nullptr, nullptr, nullptr};
  int32_t last_cuda_error = 0;
  float last_chunk_ms = 0.f;
  float last_prep_ms = 0.f;
  float create_prep_ms = 0.f;

  uint64_t tiles() const { return static_cast<uint64_t>(m) * n / 128u; }
  uint64_t dump_bytes() const { return tiles() * SPM_DUMP_RECORD_BYTES; }

  spm_status_t fail(cudaError_t e) {
    last_cuda_error = static_cast<int32_t>(e);
    return SPM_ERR_CUDA;
  }

  GemmArgs gemm_args() const {
    GemmArgs a{};
    a.a = a_noised;
    a.bt = bt_noised;
    a.m = m;
    a.n = n;
    a.k = k;
    std::memcpy(a.key, key.w, sizeof(a.key));
    std::memcpy(a.bound, bound.w, sizeof(a.bound));
    a.hit_count = hit_count;
    a.hits = hits;
    a.hit_capacity = hit_capacity;
    a.dump = dump ? dump_buf : nullptr;
    a.abort_flag = abort_dev;
    return a;
  }

  // Enqueues the next chunk between events ev[2*slot] and ev[2*slot+1]; advances the cursor.
  // A remainder under a quarter chunk is folded into this chunk instead of trailing on its own.
  cudaError_t enqueue_chunk(int slot) {
    const uint32_t left = cta_tiles - cursor;
    uint32_t count = left < chunk_ctas ? left : chunk_ctas;
    if (adaptive && left - count < chunk_ctas / 4) count = left;
    slot_ctas[slot] = count;
    cudaError_t e = cudaEventRecord(ev[2 * slot], stream);
    if (e != cudaSuccess) return e;
    e = spm::launch_gemm_v0(gemm_args(), cursor, count, dump, stream);
    if (e != cudaSuccess) return e;
    e = cudaEventRecord(ev[2 * slot + 1], stream);
    if (e != cudaSuccess) return e;
    cursor += count;
    return cudaSuccess;
  }

  cudaError_t finish_chunk(int slot, bool adapt = true) {
    cudaError_t e = cudaEventSynchronize(ev[2 * slot + 1]);
    if (e != cudaSuccess) return e;
    float ms = 0.f;
    e = cudaEventElapsedTime(&ms, ev[2 * slot], ev[2 * slot + 1]);
    if (e != cudaSuccess) return e;
    last_chunk_ms = ms;
    if (adapt && adaptive && ms > 0.f && slot_ctas[slot] >= wave) {
      // Next chunks: kChunkTargetMs at the rate just measured, in whole waves, within
      // [one wave, chunk_max].
      const double per_ms = static_cast<double>(slot_ctas[slot]) / ms;
      uint64_t want = static_cast<uint64_t>(kChunkTargetMs * per_ms) / wave * wave;
      if (want < wave) want = wave;
      if (want > chunk_max) want = chunk_max;
      chunk_ctas = static_cast<uint32_t>(want);
    }
    return cudaSuccess;
  }

  void release() {
    if (stream) cudaStreamSynchronize(stream);
    if (side_stream) cudaStreamSynchronize(side_stream);
    for (cudaEvent_t& e : ev) {
      if (e) cudaEventDestroy(e);
      e = nullptr;
    }
    if (stream) cudaStreamDestroy(stream);
    stream = nullptr;
    if (side_stream) cudaStreamDestroy(side_stream);
    side_stream = nullptr;
    if (arena) cudaFree(arena);
    arena = nullptr;
  }
};

namespace {

spm_status_t validate_params(const spm_job_params_t* p) {
  if (!p || !p->config52 || !p->b_noise_seed || !p->bound) return SPM_ERR_NULL;
  if ((p->host_a == nullptr) != (p->host_bt == nullptr)) return SPM_ERR_NULL;
  const uint32_t m = p->m, n = p->n, k = p->k;
  if (m == 0 || n == 0 || m % 64 || n % 64 || m > kMaxDim || n > kMaxDim) return SPM_ERR_SHAPE;
  if (k % 64 || k < kMinK || k > kMaxK) return SPM_ERR_SHAPE;
  const uint32_t cfg_k = static_cast<uint32_t>(p->config52[0]) | (static_cast<uint32_t>(p->config52[1]) << 8) |
                         (static_cast<uint32_t>(p->config52[2]) << 16) |
                         (static_cast<uint32_t>(p->config52[3]) << 24);
  if (cfg_k != k || std::memcmp(p->config52 + 4, kConfigTail, sizeof(kConfigTail)) != 0) {
    return SPM_ERR_CONFIG;
  }
  if (p->hit_capacity > kMaxHitCapacity) return SPM_ERR_RANGE;
  return SPM_OK;
}

}  // namespace

extern "C" {

spm_status_t spm_job_create(const spm_job_params_t* params, spm_job_t** out) {
  if (!out) return SPM_ERR_NULL;
  *out = nullptr;
  const spm_status_t v = validate_params(params);
  if (v != SPM_OK) return v;
  const uint32_t m = params->m, n = params->n, k = params->k;
  if (params->host_a && (!entries_in_signal_range(params->host_a, static_cast<uint64_t>(m) * k) ||
                         !entries_in_signal_range(params->host_bt, static_cast<uint64_t>(n) * k))) {
    return SPM_ERR_RANGE;
  }

  const GemmGeometry geo = spm::gemm_v0_geometry(m, n);
  const uint64_t cta_tiles = static_cast<uint64_t>(geo.tiles_m) * geo.tiles_n;
  if (cta_tiles >= (1ull << 31)) return SPM_ERR_SHAPE;

  // Arena layout (every piece 256-byte aligned).
  const bool dump = (params->flags & SPM_JOB_DUMP) != 0;
  const uint32_t hit_capacity = params->hit_capacity ? params->hit_capacity : kDefaultHitCapacity;
  const uint64_t mk = static_cast<uint64_t>(m) * k, nk = static_cast<uint64_t>(n) * k;
  size_t off = 0;
  const size_t off_a_base = off;
  off = align_up(off + mk);
  const size_t off_a_noised = off;
  off = align_up(off + mk);
  const size_t off_bt = off;
  off = align_up(off + nk);
  const size_t off_a_l = off;
  off = align_up(off + static_cast<uint64_t>(m) * spm::NOISE_RANK);
  const size_t off_b_rt = off;
  off = align_up(off + static_cast<uint64_t>(n) * spm::NOISE_RANK);
  const size_t off_a_pairs = off;
  off = align_up(off + 2ull * k);
  const size_t off_b_pairs = off;
  off = align_up(off + 2ull * k);
  const size_t off_count = off;
  off = align_up(off + sizeof(uint32_t));
  const size_t off_abort = off;
  off = align_up(off + sizeof(uint32_t));
  const size_t off_hits = off;
  off = align_up(off + static_cast<uint64_t>(hit_capacity) * sizeof(DeviceHit));
  const size_t off_dump = off;
  if (dump) off = align_up(off + static_cast<uint64_t>(m) * n / 128u * SPM_DUMP_RECORD_BYTES);
  const uint64_t total = off;
  if (total > SPM_JOB_DEVICE_BUDGET_BYTES) return SPM_ERR_BUDGET;

  spm_job* job = new (std::nothrow) spm_job();
  if (!job) return SPM_ERR_INTERNAL;
  job->m = m;
  job->n = n;
  job->k = k;
  job->dump = dump;
  job->geo = geo;
  job->cta_tiles = static_cast<uint32_t>(cta_tiles);
  job->hit_capacity = hit_capacity;
  job->bound = words_from_bytes(params->bound);

  auto bail = [&](cudaError_t e) {
    g_last_create_cuda_error = static_cast<int32_t>(e);
    job->release();
    delete job;
    return SPM_ERR_CUDA;
  };

  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return bail(e);
  int sms = 0;
  e = cudaDeviceGetAttribute(&sms, cudaDevAttrMultiProcessorCount, 0);
  if (e != cudaSuccess) return bail(e);
  job->sm_count = static_cast<uint32_t>(sms);
  uint32_t ctas_per_sm = 0;
  e = spm::gemm_v0_prepare(&ctas_per_sm);
  if (e != cudaSuccess) return bail(e);
  job->geo.ctas_per_sm = ctas_per_sm;

  // Chunk size (see kChunkTargetMs): whole waves; a fixed params->chunk_ctas disables adaptation.
  const uint32_t wave = job->sm_count * ctas_per_sm;
  job->wave = wave;
  uint32_t chunk = params->chunk_ctas;
  if (chunk == 0) {
    const double macs_per_cta = static_cast<double>(geo.block_m) * geo.block_n * (k / 128u * 128u);
    const double wave_ms = 1e3 * macs_per_cta * ctas_per_sm / kPerSmMacRateCeiling;
    uint32_t waves = static_cast<uint32_t>(kChunkTargetMs / wave_ms);
    if (waves == 0) waves = 1;
    chunk = waves * wave;
    job->adaptive = true;
  }
  job->chunk_ctas = chunk < job->cta_tiles ? chunk : job->cta_tiles;
  job->chunk_max = job->chunk_ctas;

  e = cudaStreamCreateWithFlags(&job->stream, cudaStreamNonBlocking);
  if (e != cudaSuccess) return bail(e);
  e = cudaStreamCreateWithFlags(&job->side_stream, cudaStreamNonBlocking);
  if (e != cudaSuccess) return bail(e);
  for (cudaEvent_t& ev : job->ev) {
    e = cudaEventCreate(&ev);
    if (e != cudaSuccess) return bail(e);
  }
  e = cudaMalloc(&job->arena, total);
  if (e != cudaSuccess) return bail(e);
  job->device_bytes = total;
  uint8_t* base = static_cast<uint8_t*>(job->arena);
  job->a_base = reinterpret_cast<int8_t*>(base + off_a_base);
  job->a_noised = reinterpret_cast<int8_t*>(base + off_a_noised);
  job->bt_noised = reinterpret_cast<int8_t*>(base + off_bt);
  job->a_l = reinterpret_cast<int8_t*>(base + off_a_l);
  job->b_rt = reinterpret_cast<int8_t*>(base + off_b_rt);
  job->a_pairs = base + off_a_pairs;
  job->b_pairs = base + off_b_pairs;
  job->hit_count = reinterpret_cast<uint32_t*>(base + off_count);
  job->abort_dev = reinterpret_cast<uint32_t*>(base + off_abort);
  job->hits = reinterpret_cast<DeviceHit*>(base + off_hits);
  job->dump_buf = dump ? base + off_dump : nullptr;

  e = cudaEventRecord(job->ev[0], job->stream);
  if (e != cudaSuccess) return bail(e);
  if (params->host_a) {
    e = cudaMemcpyAsync(job->a_base, params->host_a, mk, cudaMemcpyHostToDevice, job->stream);
    if (e != cudaSuccess) return bail(e);
    e = cudaMemcpyAsync(job->bt_noised, params->host_bt, nk, cudaMemcpyHostToDevice, job->stream);
    if (e != cudaSuccess) return bail(e);
  } else {
    e = spm::launch_fill_int7(job->a_base, params->gen_seed, spm::DOMAIN_A, m, k, job->stream);
    if (e != cudaSuccess) return bail(e);
    e = spm::launch_fill_int7(job->bt_noised, params->gen_seed, spm::DOMAIN_BT, n, k, job->stream);
    if (e != cudaSuccess) return bail(e);
  }
  const Words8 b_key = words_from_bytes(params->b_noise_seed);
  e = spm::launch_uniform_factor(job->b_rt, n, b_key, spm::LABEL_B, job->stream);
  if (e != cudaSuccess) return bail(e);
  e = spm::launch_perm_pairs(job->b_pairs, k, b_key, spm::LABEL_B, job->stream);
  if (e != cudaSuccess) return bail(e);
  e = spm::launch_apply_noise(job->bt_noised, job->bt_noised, job->b_rt, job->b_pairs, n, k,
                              job->stream);
  if (e != cudaSuccess) return bail(e);
  e = cudaMemsetAsync(job->hit_count, 0, sizeof(uint32_t), job->stream);
  if (e != cudaSuccess) return bail(e);
  e = cudaMemsetAsync(job->abort_dev, 0, sizeof(uint32_t), job->stream);
  if (e != cudaSuccess) return bail(e);
  e = cudaEventRecord(job->ev[1], job->stream);
  if (e != cudaSuccess) return bail(e);
  e = cudaEventSynchronize(job->ev[1]);
  if (e != cudaSuccess) return bail(e);
  e = cudaEventElapsedTime(&job->create_prep_ms, job->ev[0], job->ev[1]);
  if (e != cudaSuccess) return bail(e);

  *out = job;
  return SPM_OK;
}

spm_status_t spm_job_patch_a(spm_job_t* job, uint64_t offset, const int8_t* data, uint64_t len) {
  if (!job || (!data && len)) return SPM_ERR_NULL;
  const uint64_t size = static_cast<uint64_t>(job->m) * job->k;
  if (offset > size || len > size - offset) return SPM_ERR_RANGE;
  if (!entries_in_signal_range(data, len)) return SPM_ERR_RANGE;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaMemcpyAsync(job->a_base + offset, data, len, cudaMemcpyHostToDevice, job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaStreamSynchronize(job->stream);
  if (e != cudaSuccess) return job->fail(e);
  job->attempt_ready = false;  // A' is stale until the next spm_job_set_attempt
  return SPM_OK;
}

spm_status_t spm_job_set_attempt(spm_job_t* job, const uint8_t* a_noise_seed, const uint8_t* bound) {
  if (!job || !a_noise_seed) return SPM_ERR_NULL;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  job->attempt_ready = false;
  job->key = words_from_bytes(a_noise_seed);
  if (bound) job->bound = words_from_bytes(bound);
  e = cudaEventRecord(job->ev[0], job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = spm::launch_uniform_factor(job->a_l, job->m, job->key, spm::LABEL_A, job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = spm::launch_perm_pairs(job->a_pairs, job->k, job->key, spm::LABEL_A, job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = spm::launch_apply_noise(job->a_base, job->a_noised, job->a_l, job->a_pairs, job->m, job->k,
                              job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaMemsetAsync(job->hit_count, 0, sizeof(uint32_t), job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaMemsetAsync(job->abort_dev, 0, sizeof(uint32_t), job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaEventRecord(job->ev[1], job->stream);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaEventSynchronize(job->ev[1]);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaEventElapsedTime(&job->last_prep_ms, job->ev[0], job->ev[1]);
  if (e != cudaSuccess) return job->fail(e);
  job->cursor = 0;
  job->attempt_ready = true;
  return SPM_OK;
}

spm_status_t spm_job_run_chunk(spm_job_t* job, int32_t* status) {
  if (!job || !status) return SPM_ERR_NULL;
  if (!job->attempt_ready) return SPM_ERR_STATE;
  if (job->cursor < job->cta_tiles) {
    cudaError_t e = cudaSetDevice(0);
    if (e != cudaSuccess) return job->fail(e);
    e = job->enqueue_chunk(0);
    if (e != cudaSuccess) return job->fail(e);
    e = job->finish_chunk(0);
    if (e != cudaSuccess) return job->fail(e);
  }
  *status = job->cursor < job->cta_tiles ? SPM_CHUNK_MORE : SPM_CHUNK_DONE;
  return SPM_OK;
}

spm_status_t spm_job_run_attempt(spm_job_t* job, const uint32_t* abort_flag, int32_t* status) {
  if (!job || !status) return SPM_ERR_NULL;
  if (!job->attempt_ready) return SPM_ERR_STATE;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  auto aborted = [&]() { return abort_flag && __atomic_load_n(abort_flag, __ATOMIC_RELAXED) != 0; };
  // Two chunks in flight: the next one is queued while the previous runs, so the GPU never idles
  // between chunks. The host flag is checked before queuing and polled (every few tens of us)
  // while waiting; once it is seen, the device abort word is raised through the side stream, so
  // every CTA of the chunks in flight that has not reached its first MMA leaves at once. The
  // attempt then stops within about one CTA tile time and is void (hits found so far stay valid
  // and readable); spm_job_set_attempt starts the next one.
  static const uint32_t kOne = 1;
  bool stop = false;    // host flag seen
  bool raised = false;  // device word raised
  auto raise = [&]() -> cudaError_t {
    raised = true;
    return cudaMemcpyAsync(job->abort_dev, &kOne, sizeof(kOne), cudaMemcpyHostToDevice,
                           job->side_stream);
  };
  int pending = -1;  // event slot of the chunk in flight, if any
  int slot = 0;
  for (;;) {
    int queued = -1;
    if (!stop && job->cursor < job->cta_tiles) {
      if (aborted()) {
        stop = true;
      } else {
        e = job->enqueue_chunk(slot);
        if (e != cudaSuccess) return job->fail(e);
        queued = slot;
        slot ^= 1;
      }
    }
    if (pending >= 0) {
      for (;;) {
        e = cudaEventQuery(job->ev[2 * pending + 1]);
        if (e == cudaSuccess) break;
        if (e != cudaErrorNotReady) return job->fail(e);
        if (!stop && aborted()) stop = true;
        if (stop && !raised) {
          e = raise();
          if (e != cudaSuccess) return job->fail(e);
        }
        std::this_thread::sleep_for(std::chrono::microseconds(20));
      }
      e = job->finish_chunk(pending, /*adapt=*/!raised);
      if (e != cudaSuccess) return job->fail(e);
    }
    pending = queued;
    if (pending < 0) break;
  }
  if (raised) {
    // The word must have landed before spm_job_set_attempt clears it on the main stream.
    e = cudaStreamSynchronize(job->side_stream);
    if (e != cudaSuccess) return job->fail(e);
  }
  if (stop) job->attempt_ready = false;
  *status = stop ? SPM_CHUNK_ABORTED : SPM_CHUNK_DONE;
  return SPM_OK;
}

spm_status_t spm_job_read_hits(spm_job_t* job, spm_hit_t* out, uint32_t cap, uint32_t* total) {
  if (!job || !total || (!out && cap)) return SPM_ERR_NULL;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  uint32_t count = 0;
  e = cudaMemcpy(&count, job->hit_count, sizeof(count), cudaMemcpyDeviceToHost);
  if (e != cudaSuccess) return job->fail(e);
  *total = count;
  uint32_t n = count < job->hit_capacity ? count : job->hit_capacity;
  if (n > cap) n = cap;
  if (n == 0) return SPM_OK;
  DeviceHit* tmp = new (std::nothrow) DeviceHit[n];
  if (!tmp) return SPM_ERR_INTERNAL;
  e = cudaMemcpy(tmp, job->hits, static_cast<size_t>(n) * sizeof(DeviceHit), cudaMemcpyDeviceToHost);
  if (e != cudaSuccess) {
    delete[] tmp;
    return job->fail(e);
  }
  for (uint32_t i = 0; i < n; ++i) {
    out[i].t_rows = tmp[i].t_rows;
    out[i].t_cols = tmp[i].t_cols;
    bytes_from_words(tmp[i].digest, out[i].digest);
  }
  delete[] tmp;
  return SPM_OK;
}

spm_status_t spm_job_read_dump(spm_job_t* job, uint8_t* out, uint64_t cap, uint64_t* written) {
  if (!job || !out || !written) return SPM_ERR_NULL;
  *written = 0;
  if (!job->dump) return SPM_ERR_NO_DUMP;
  // Only a completed attempt has a record for every tile (aborted CTAs skip theirs).
  if (!job->attempt_ready || job->cursor < job->cta_tiles) return SPM_ERR_STATE;
  const uint64_t size = job->dump_bytes();
  if (cap < size) return SPM_ERR_RANGE;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaMemcpy(out, job->dump_buf, size, cudaMemcpyDeviceToHost);
  if (e != cudaSuccess) return job->fail(e);
  *written = size;
  return SPM_OK;
}

spm_status_t spm_job_read_buffer(spm_job_t* job, int32_t which, uint64_t offset, void* out, uint64_t len) {
  if (!job || (!out && len)) return SPM_ERR_NULL;
  const void* src = nullptr;
  uint64_t size = 0;
  const uint64_t mk = static_cast<uint64_t>(job->m) * job->k, nk = static_cast<uint64_t>(job->n) * job->k;
  switch (which) {
    case SPM_BUF_A_BASE: src = job->a_base; size = mk; break;
    case SPM_BUF_A_NOISED: src = job->a_noised; size = mk; break;
    case SPM_BUF_BT_NOISED: src = job->bt_noised; size = nk; break;
    case SPM_BUF_A_L: src = job->a_l; size = static_cast<uint64_t>(job->m) * spm::NOISE_RANK; break;
    case SPM_BUF_B_RT: src = job->b_rt; size = static_cast<uint64_t>(job->n) * spm::NOISE_RANK; break;
    case SPM_BUF_A_PAIRS: src = job->a_pairs; size = 2ull * job->k; break;
    case SPM_BUF_B_PAIRS: src = job->b_pairs; size = 2ull * job->k; break;
    default: return SPM_ERR_RANGE;
  }
  if (offset > size || len > size - offset) return SPM_ERR_RANGE;
  if (len == 0) return SPM_OK;
  cudaError_t e = cudaSetDevice(0);
  if (e != cudaSuccess) return job->fail(e);
  e = cudaMemcpy(out, static_cast<const uint8_t*>(src) + offset, len, cudaMemcpyDeviceToHost);
  if (e != cudaSuccess) return job->fail(e);
  return SPM_OK;
}

spm_status_t spm_job_info(const spm_job_t* job, spm_job_info_t* out) {
  if (!job || !out) return SPM_ERR_NULL;
  std::memset(out, 0, sizeof(*out));
  out->m = job->m;
  out->n = job->n;
  out->k = job->k;
  out->block_m = job->geo.block_m;
  out->block_n = job->geo.block_n;
  out->tiles = job->tiles();
  out->cta_tiles = job->cta_tiles;
  out->chunk_ctas = job->chunk_ctas;
  out->chunks = (job->cta_tiles + job->chunk_ctas - 1) / job->chunk_ctas;
  out->ctas_per_sm = job->geo.ctas_per_sm;
  out->sm_count = job->sm_count;
  out->smem_bytes = job->geo.smem_bytes;
  out->hit_capacity = job->hit_capacity;
  out->device_bytes = job->device_bytes;
  out->last_chunk_ms = job->last_chunk_ms;
  out->last_prep_ms = job->last_prep_ms;
  out->create_prep_ms = job->create_prep_ms;
  return SPM_OK;
}

int32_t spm_job_last_cuda_error(const spm_job_t* job) { return job ? job->last_cuda_error : 0; }

void spm_job_destroy(spm_job_t* job) {
  if (!job) return;
  cudaSetDevice(0);
  job->release();
  delete job;
}

int32_t spm_last_cuda_error(void) { return g_last_create_cuda_error; }

const char* spm_status_string(spm_status_t status) {
  switch (status) {
    case SPM_OK: return "ok";
    case SPM_ERR_NULL: return "a required pointer argument is NULL";
    case SPM_ERR_SHAPE: return "unsupported shape (m, n multiples of 64 up to 2^24; 2048 <= k <= 65536, k % 64 == 0)";
    case SPM_ERR_CONFIG: return "config52 is not the configuration this kernel implements (r = 128, int7, 8x16 pattern, no MoE, same k)";
    case SPM_ERR_BUDGET: return "the job would exceed the 2 GiB device-memory budget";
    case SPM_ERR_CUDA: return "CUDA error";
    case SPM_ERR_STATE: return "call out of order (no attempt set up)";
    case SPM_ERR_RANGE: return "value, offset or length out of range";
    case SPM_ERR_NO_DUMP: return "the job was created without SPM_JOB_DUMP";
    case SPM_ERR_INTERNAL: return "internal error";
    case SPM_CHUNK_MORE: return "chunk done, more remain";
    case SPM_CHUNK_DONE: return "attempt done";
    case SPM_CHUNK_ABORTED: return "attempt aborted (void; set up a new attempt)";
    default: return "unknown status";
  }
}

const char* spm_cuda_error_string(int32_t cuda_error) {
  return cudaGetErrorString(static_cast<cudaError_t>(cuda_error));
}

}  // extern "C"

namespace {
__global__ void blake3_keyed64_kernel(Words8 key, const uint32_t* msg, uint32_t* out) {
  uint32_t m[16], k[8], d[8];
  for (int i = 0; i < 16; ++i) m[i] = msg[i];
  for (int i = 0; i < 8; ++i) k[i] = key.w[i];
  spm::blake3::keyed_hash64(k, m, d);
  for (int i = 0; i < 8; ++i) out[i] = d[i];
}
}  // namespace

extern "C" spm_status_t spm_debug_blake3_keyed64(const uint8_t* key32, const uint8_t* msg64, uint8_t* out32) {
  if (!key32 || !msg64 || !out32) return SPM_ERR_NULL;
  uint32_t* d = nullptr;
  cudaError_t e = cudaSetDevice(0);
  if (e == cudaSuccess) e = cudaMalloc(&d, 24 * sizeof(uint32_t));
  if (e != cudaSuccess) {
    g_last_create_cuda_error = static_cast<int32_t>(e);
    return SPM_ERR_CUDA;
  }
  uint32_t msg[16];
  for (int i = 0; i < 2; ++i) {
    const Words8 w = words_from_bytes(msg64 + 32 * i);
    std::memcpy(msg + 8 * i, w.w, sizeof(w.w));
  }
  uint32_t res[8] = {0};
  e = cudaMemcpy(d, msg, sizeof(msg), cudaMemcpyHostToDevice);
  if (e == cudaSuccess) {
    blake3_keyed64_kernel<<<1, 1>>>(words_from_bytes(key32), d, d + 16);
    e = cudaGetLastError();
  }
  if (e == cudaSuccess) e = cudaMemcpy(res, d + 16, sizeof(res), cudaMemcpyDeviceToHost);
  cudaFree(d);
  if (e != cudaSuccess) {
    g_last_create_cuda_error = static_cast<int32_t>(e);
    return SPM_ERR_CUDA;
  }
  bytes_from_words(res, out32);
  return SPM_OK;
}
