// Operand preparation kernels (see prep.cuh for the exact definitions they implement).
#include "prep.cuh"

#include "../common/blake3.cuh"
#include "../common/splitmix.cuh"

namespace spm {
namespace prep {
namespace {

constexpr int kThreads = 256;
constexpr uint32_t kRowsPerCta = 16;

// One thread per 32-byte slot-0 hash: hash h fills bytes [32h, 32h + 32) of the factor, i.e. row
// h / 4, columns 32 (h % 4) ... + 31.
__global__ void __launch_bounds__(kThreads) uniform_factor_kernel(int8_t* __restrict__ out,
                                                                   uint32_t hashes, Seed seed,
                                                                   uint32_t label_w0) {
  const uint32_t h = blockIdx.x * kThreads + threadIdx.x;
  if (h >= hashes) return;
  uint32_t d[8];
  noise_hash<0>(seed.w, label_w0, h, d);
  // (byte & 63) - 32 per byte, no borrows between lanes.
  uint4 lo, hi;
  lo.x = __vsub4(d[0] & 0x3f3f3f3fu, 0x20202020u);
  lo.y = __vsub4(d[1] & 0x3f3f3f3fu, 0x20202020u);
  lo.z = __vsub4(d[2] & 0x3f3f3f3fu, 0x20202020u);
  lo.w = __vsub4(d[3] & 0x3f3f3f3fu, 0x20202020u);
  hi.x = __vsub4(d[4] & 0x3f3f3f3fu, 0x20202020u);
  hi.y = __vsub4(d[5] & 0x3f3f3f3fu, 0x20202020u);
  hi.z = __vsub4(d[6] & 0x3f3f3f3fu, 0x20202020u);
  hi.w = __vsub4(d[7] & 0x3f3f3f3fu, 0x20202020u);
  uint4* dst = reinterpret_cast<uint4*>(out + 32ull * h);
  dst[0] = lo;
  dst[1] = hi;
}

// One thread per slot-1 hash: hash t gives pairs 8t .. 8t + 7 (word w -> pair 8t + w).
__global__ void __launch_bounds__(kThreads) pairs_kernel(uint8_t* __restrict__ pairs,
                                                          uint32_t hashes, Seed seed,
                                                          uint32_t label_w0) {
  const uint32_t t = blockIdx.x * kThreads + threadIdx.x;
  if (t >= hashes) return;
  uint32_t d[8];
  noise_hash<1>(seed.w, label_w0, t, d);
  uint32_t packed[4];
#pragma unroll
  for (int w = 0; w < 8; w += 2) {
    const uint32_t x0 = d[w], x1 = d[w + 1];
    const uint32_t p0 = x0 & (RANK - 1);
    const uint32_t q0 = p0 ^ (1u + (uint32_t)(((uint64_t)(RANK - 1) * x0) >> 32));
    const uint32_t p1 = x1 & (RANK - 1);
    const uint32_t q1 = p1 ^ (1u + (uint32_t)(((uint64_t)(RANK - 1) * x1) >> 32));
    packed[w / 2] = p0 | (q0 << 8) | (p1 << 16) | (q1 << 24);
  }
  *reinterpret_cast<uint4*>(pairs + 16ull * t) = make_uint4(packed[0], packed[1], packed[2], packed[3]);
}

// A CTA owns kRowsPerCta rows: their factor rows go to shared memory, then every thread produces
// 16 consecutive output entries at a time (coalesced 16-byte loads/stores).
template <bool kGenerated>
__global__ void __launch_bounds__(kThreads) noised_operand_kernel(
    int8_t* out, OperandSource src, const int8_t* __restrict__ factor,
    const uint8_t* __restrict__ pairs, uint32_t rows, uint32_t k, int noise_only) {
  __shared__ __align__(16) int8_t s_factor[kRowsPerCta * RANK];
  const uint32_t row0 = blockIdx.x * kRowsPerCta;
  const uint32_t nrows = min(kRowsPerCta, rows - row0);
  for (uint32_t i = threadIdx.x; i < nrows * (RANK / 16); i += kThreads) {
    reinterpret_cast<uint4*>(s_factor)[i] =
        reinterpret_cast<const uint4*>(factor + (size_t)row0 * RANK)[i];
  }
  __syncthreads();
  const uint32_t chunks_per_row = k / 16;
  const uint32_t total = nrows * chunks_per_row;
  for (uint32_t c = threadIdx.x; c < total; c += kThreads) {
    const uint32_t r = c / chunks_per_row;
    const uint32_t l0 = (c - r * chunks_per_row) * 16;
    const int8_t* f = s_factor + r * RANK;
    const uint4 pq0 = *reinterpret_cast<const uint4*>(pairs + 2 * l0);
    const uint4 pq1 = *reinterpret_cast<const uint4*>(pairs + 2 * l0 + 16);
    const uint32_t pw[8] = {pq0.x, pq0.y, pq0.z, pq0.w, pq1.x, pq1.y, pq1.z, pq1.w};
    uint32_t e[4];
#pragma unroll
    for (int g = 0; g < 4; ++g) {
      uint32_t plus = 0, minus = 0;
#pragma unroll
      for (int j = 0; j < 4; ++j) {
        const uint32_t word = pw[(4 * g + j) >> 1];
        const uint32_t shift = ((4 * g + j) & 1) * 16;
        const uint32_t p = (word >> shift) & 0x7fu;
        const uint32_t q = (word >> (shift + 8)) & 0x7fu;
        plus |= (uint32_t)(uint8_t)f[p] << (8 * j);
        minus |= (uint32_t)(uint8_t)f[q] << (8 * j);
      }
      // Entries are in [-32, 31]: the byte-wise difference is the exact s8 in [-63, 63].
      e[g] = __vsub4(plus, minus);
    }
    const uint64_t idx = (uint64_t)(row0 + r) * k + l0;
    uint32_t x[4] = {0u, 0u, 0u, 0u};
    if (!noise_only) {
      if (kGenerated) {
        const uint64_t w0 = splitmix64_word(src.gen_state, idx / 8);
        const uint64_t w1 = splitmix64_word(src.gen_state, idx / 8 + 1);
        x[0] = int7x4((uint32_t)w0);
        x[1] = int7x4((uint32_t)(w0 >> 32));
        x[2] = int7x4((uint32_t)w1);
        x[3] = int7x4((uint32_t)(w1 >> 32));
      } else {
        const uint4 v = *reinterpret_cast<const uint4*>(src.stored + idx);
        x[0] = v.x;
        x[1] = v.y;
        x[2] = v.z;
        x[3] = v.w;
      }
      if (idx < src.prefix_len) {
#pragma unroll
        for (int b = 0; b < 16; ++b) {
          if (idx + b < src.prefix_len) {
            const uint32_t sh = 8 * (b & 3);
            x[b >> 2] = (x[b >> 2] & ~(0xffu << sh)) | ((uint32_t)src.prefix[idx + b] << sh);
          }
        }
      }
    }
    // |x| <= 64 and |e| <= 63: the byte-wise sum is the exact s8 in [-127, 127].
    uint4 o;
    o.x = __vadd4(x[0], e[0]);
    o.y = __vadd4(x[1], e[1]);
    o.z = __vadd4(x[2], e[2]);
    o.w = __vadd4(x[3], e[3]);
    *reinterpret_cast<uint4*>(out + idx) = o;
  }
}

__global__ void __launch_bounds__(kThreads) fill_int7_kernel(int8_t* __restrict__ out,
                                                              uint64_t chunks, uint64_t state) {
  for (uint64_t c = blockIdx.x * (uint64_t)kThreads + threadIdx.x; c < chunks;
       c += (uint64_t)gridDim.x * kThreads) {
    const uint64_t w0 = splitmix64_word(state, 2 * c);
    const uint64_t w1 = splitmix64_word(state, 2 * c + 1);
    reinterpret_cast<uint4*>(out)[c] =
        make_uint4(int7x4((uint32_t)w0), int7x4((uint32_t)(w0 >> 32)), int7x4((uint32_t)w1),
                   int7x4((uint32_t)(w1 >> 32)));
  }
}

inline uint32_t blocks_for(uint64_t items) { return (uint32_t)((items + kThreads - 1) / kThreads); }

}  // namespace

cudaError_t launch_uniform_factor(int8_t* out, uint32_t rows, Seed seed, uint32_t label_w0,
                                  cudaStream_t stream) {
  if (rows == 0) return cudaSuccess;
  const uint32_t hashes = rows * (RANK / 32);
  uniform_factor_kernel<<<blocks_for(hashes), kThreads, 0, stream>>>(out, hashes, seed, label_w0);
  return cudaGetLastError();
}

cudaError_t launch_pairs(uint8_t* pairs, uint32_t k, Seed seed, uint32_t label_w0,
                         cudaStream_t stream) {
  if (k == 0) return cudaSuccess;
  const uint32_t hashes = k / 8;
  pairs_kernel<<<blocks_for(hashes), kThreads, 0, stream>>>(pairs, hashes, seed, label_w0);
  return cudaGetLastError();
}

cudaError_t launch_noised_operand(int8_t* out, OperandSource source, const int8_t* factor,
                                  const uint8_t* pairs, uint32_t rows, uint32_t k, bool noise_only,
                                  cudaStream_t stream) {
  if (rows == 0) return cudaSuccess;
  const uint32_t grid = (rows + kRowsPerCta - 1) / kRowsPerCta;
  if (source.stored) {
    noised_operand_kernel<false><<<grid, kThreads, 0, stream>>>(out, source, factor, pairs, rows, k,
                                                                noise_only ? 1 : 0);
  } else {
    noised_operand_kernel<true><<<grid, kThreads, 0, stream>>>(out, source, factor, pairs, rows, k,
                                                               noise_only ? 1 : 0);
  }
  return cudaGetLastError();
}

cudaError_t launch_fill_int7(int8_t* out, uint64_t len, uint64_t gen_state, cudaStream_t stream) {
  const uint64_t chunks = len / 16;
  if (chunks == 0) return cudaSuccess;
  const uint64_t want = (chunks + kThreads - 1) / kThreads;
  const uint32_t grid = (uint32_t)(want < 4096 ? want : 4096);
  fill_int7_kernel<<<grid, kThreads, 0, stream>>>(out, chunks, gen_state);
  return cudaGetLastError();
}

}  // namespace prep
}  // namespace spm
