// Operand preparation for one PearlHash V3 job (crates/spm-cpuref/README.md, sections 1-4):
//   * A_base and Bᵀ from the SplitMix64 int7 stream (or uploaded by the host),
//   * the uniform factors A_L (m x 128) and B_Rᵀ (n x 128),
//   * the permutation pairs of A_R and B_L (k pairs each),
//   * the noised s8 operands A' = A + E_A and B'ᵀ = Bᵀ + E_Bᵀ, with
//     E[i][l] = F[i][p(l)] - F[i][q(l)] computed on the fly (E is never stored).
// All integer, no floating point. The kernels are memory bound; the B side runs once per job,
// the A side once per attempt.
#include <cstdint>

#include <cuda_runtime.h>

#include "noise_hash.cuh"
#include "spm_internal.h"
#include "splitmix.cuh"

namespace spm {
namespace {

constexpr int kThreads = 256;

__global__ void __launch_bounds__(kThreads) fill_int7_kernel(uint64_t stream_seed, ulonglong2* out,
                                                             uint64_t n16) {
  const uint64_t stride = static_cast<uint64_t>(gridDim.x) * blockDim.x;
  for (uint64_t t = static_cast<uint64_t>(blockIdx.x) * blockDim.x + threadIdx.x; t < n16;
       t += stride) {
    const uint64_t w0 = int7_bytes(splitmix_word(stream_seed, 2 * t));
    const uint64_t w1 = int7_bytes(splitmix_word(stream_seed, 2 * t + 1));
    out[t] = make_ulonglong2(w0, w1);
  }
}

__global__ void __launch_bounds__(kThreads) uniform_factor_kernel(uint4* out, uint64_t n_hashes,
                                                                  Words8 key, Words8 label) {
  const uint64_t h = static_cast<uint64_t>(blockIdx.x) * blockDim.x + threadIdx.x;
  if (h >= n_hashes) return;
  uint32_t d[8];
  noise_hash(key, label, static_cast<uint32_t>(h), 0, d);
  out[2 * h] = make_uint4(uniform_bytes(d[0]), uniform_bytes(d[1]), uniform_bytes(d[2]),
                          uniform_bytes(d[3]));
  out[2 * h + 1] = make_uint4(uniform_bytes(d[4]), uniform_bytes(d[5]), uniform_bytes(d[6]),
                              uniform_bytes(d[7]));
}

__global__ void __launch_bounds__(kThreads) perm_pairs_kernel(uint32_t* pairs16, uint32_t n_hashes,
                                                              Words8 key, Words8 label) {
  const uint32_t h = blockIdx.x * blockDim.x + threadIdx.x;
  if (h >= n_hashes) return;
  uint32_t d[8];
  noise_hash(key, label, h, 1, d);
  // 8 pairs -> 16 bytes (p0 q0 p1 q1 ...), stored as 4 words.
  uint32_t packed[4];
#pragma unroll
  for (int j = 0; j < 4; ++j) {
    uint32_t p0, q0, p1, q1;
    perm_pair(d[2 * j], p0, q0);
    perm_pair(d[2 * j + 1], p1, q1);
    packed[j] = p0 | (q0 << 8) | (p1 << 16) | (q1 << 24);
  }
  reinterpret_cast<uint4*>(pairs16)[h] = make_uint4(packed[0], packed[1], packed[2], packed[3]);
}

constexpr int kNoiseRowsPerBlock = 32;
constexpr int kNoiseColsPerThread = 16;
constexpr int kNoiseColsPerBlock = kThreads * kNoiseColsPerThread;  // 4096

// Each thread owns 16 consecutive columns (and their 16 pairs, kept in registers) and walks the
// block's rows; the block stages the 32 factor rows it needs in shared memory.
__global__ void __launch_bounds__(kThreads) apply_noise_kernel(const int8_t* in, int8_t* out,
                                                               const int8_t* factor,
                                                               const uint8_t* pairs, uint64_t rows,
                                                               uint32_t k) {
  __shared__ __align__(16) int8_t s_factor[kNoiseRowsPerBlock * NOISE_RANK];
  const uint64_t row0 = static_cast<uint64_t>(blockIdx.x) * kNoiseRowsPerBlock;
  const uint32_t n_rows = static_cast<uint32_t>(
      rows - row0 < static_cast<uint64_t>(kNoiseRowsPerBlock) ? rows - row0 : kNoiseRowsPerBlock);
  // Stage the factor rows: 32 rows x 128 bytes = 256 threads x 16 bytes.
  if (threadIdx.x < n_rows * (NOISE_RANK / 16)) {
    reinterpret_cast<uint4*>(s_factor)[threadIdx.x] =
        reinterpret_cast<const uint4*>(factor + row0 * NOISE_RANK)[threadIdx.x];
  }
  __syncthreads();

  const uint32_t c0 = (blockIdx.y * kThreads + threadIdx.x) * kNoiseColsPerThread;
  if (c0 >= k) return;
  uint32_t pq[kNoiseColsPerThread / 2];  // bytes p0 q0 p1 q1 ... of this thread's 16 columns
  {
    const uint4 lo = reinterpret_cast<const uint4*>(pairs + 2ull * c0)[0];
    const uint4 hi = reinterpret_cast<const uint4*>(pairs + 2ull * c0)[1];
    pq[0] = lo.x; pq[1] = lo.y; pq[2] = lo.z; pq[3] = lo.w;
    pq[4] = hi.x; pq[5] = hi.y; pq[6] = hi.z; pq[7] = hi.w;
  }
  for (uint32_t r = 0; r < n_rows; ++r) {
    const uint64_t off = (row0 + r) * k + c0;
    const uint4 x = *reinterpret_cast<const uint4*>(in + off);
    const uint32_t xw[4] = {x.x, x.y, x.z, x.w};
    const int8_t* f = s_factor + r * NOISE_RANK;
    uint32_t yw[4];
#pragma unroll
    for (int w = 0; w < 4; ++w) {
      uint32_t acc = 0;
#pragma unroll
      for (int b = 0; b < 4; ++b) {
        const int j = 4 * w + b;               // column c0 + j
        const uint32_t word = pq[j >> 1];      // (p, q) of columns 2*(j/2), 2*(j/2)+1
        const uint32_t sh = (j & 1) * 16;
        const uint32_t p = (word >> sh) & 0xffu;
        const uint32_t q = (word >> (sh + 8)) & 0xffu;
        const int32_t xv = static_cast<int8_t>((xw[w] >> (8 * b)) & 0xffu);
        const int32_t y = xv + static_cast<int32_t>(f[p]) - static_cast<int32_t>(f[q]);
        acc |= (static_cast<uint32_t>(y) & 0xffu) << (8 * b);
      }
      yw[w] = acc;
    }
    *reinterpret_cast<uint4*>(out + off) = make_uint4(yw[0], yw[1], yw[2], yw[3]);
  }
}

uint32_t blocks_for(uint64_t n, uint32_t per_block, uint32_t cap) {
  const uint64_t b = (n + per_block - 1) / per_block;
  return static_cast<uint32_t>(b < cap ? (b == 0 ? 1 : b) : cap);
}

}  // namespace

cudaError_t launch_fill_int7(int8_t* out, uint64_t seed, uint64_t domain, uint64_t rows, uint32_t k,
                             cudaStream_t stream) {
  const uint64_t n = rows * k;
  if (n % 16 != 0) return cudaErrorInvalidValue;
  const uint64_t n16 = n / 16;
  fill_int7_kernel<<<blocks_for(n16, kThreads, 1u << 20), kThreads, 0, stream>>>(
      seed ^ domain, reinterpret_cast<ulonglong2*>(out), n16);
  return cudaGetLastError();
}

cudaError_t launch_uniform_factor(int8_t* out, uint64_t rows, const Words8& key, const Words8& label,
                                  cudaStream_t stream) {
  const uint64_t n_hashes = rows * (NOISE_RANK / 32);
  if (n_hashes > 0x7fffffffull) return cudaErrorInvalidValue;
  uniform_factor_kernel<<<blocks_for(n_hashes, kThreads, 0x7fffffffu), kThreads, 0, stream>>>(
      reinterpret_cast<uint4*>(out), n_hashes, key, label);
  return cudaGetLastError();
}

cudaError_t launch_perm_pairs(uint8_t* pairs, uint32_t k, const Words8& key, const Words8& label,
                              cudaStream_t stream) {
  if (k % 8 != 0) return cudaErrorInvalidValue;
  const uint32_t n_hashes = k / 8;
  perm_pairs_kernel<<<blocks_for(n_hashes, kThreads, 0x7fffffffu), kThreads, 0, stream>>>(
      reinterpret_cast<uint32_t*>(pairs), n_hashes, key, label);
  return cudaGetLastError();
}

cudaError_t launch_apply_noise(const int8_t* in, int8_t* out, const int8_t* factor,
                               const uint8_t* pairs, uint64_t rows, uint32_t k,
                               cudaStream_t stream) {
  if (k % kNoiseColsPerThread != 0 || rows == 0) return cudaErrorInvalidValue;
  const uint64_t row_blocks = (rows + kNoiseRowsPerBlock - 1) / kNoiseRowsPerBlock;
  if (row_blocks > 0x7fffffffull) return cudaErrorInvalidValue;
  const dim3 grid(static_cast<uint32_t>(row_blocks), (k + kNoiseColsPerBlock - 1) / kNoiseColsPerBlock);
  apply_noise_kernel<<<grid, kThreads, 0, stream>>>(in, out, factor, pairs, rows, k);
  return cudaGetLastError();
}

}  // namespace spm
