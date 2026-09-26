// Noise factors, permutation pairs and noised operands on the GPU. Bit-exact with
// spm_cpuref::noise_factors / NoiseFactors::expand / add_noise (checked by crates/spm-gpu/tests).
#include "prep.cuh"

#include "../common/blake3.cuh"
#include "../common/splitmix.cuh"

namespace spm {
namespace prep {
namespace {

constexpr int kThreads = 256;
constexpr int kRowsPerCta = 16;  // noised-operand kernel: factor rows staged in smem per CTA
constexpr int kRank = 128;

// Four bytes at once: (byte & 63) - 32 as two's complement. With x = byte & 63 in [0, 63],
// x - 32 is x ^ 0x20 with bits 6 and 7 set exactly when x < 32.
__device__ __forceinline__ uint32_t uniform_bytes(uint32_t w) {
  const uint32_t x = w & 0x3F3F3F3Fu;
  const uint32_t below = ~x & 0x20202020u;  // bit 5 clear  <=>  x < 32
  return (x ^ 0x20202020u) | (below << 1) | (below << 2);
}

__global__ void __launch_bounds__(kThreads) uniform_kernel(int8_t* __restrict__ out,
                                                           uint32_t n_hashes, Key key,
                                                           bool b_side) {
  const uint32_t h = blockIdx.x * blockDim.x + threadIdx.x;
  if (h >= n_hashes) return;
  uint32_t d[8];
  blake3::noise_hash(key.w, h, 0, b_side, d);
  uint4* dst = reinterpret_cast<uint4*>(out + 32ull * h);
  dst[0] = make_uint4(uniform_bytes(d[0]), uniform_bytes(d[1]), uniform_bytes(d[2]),
                      uniform_bytes(d[3]));
  dst[1] = make_uint4(uniform_bytes(d[4]), uniform_bytes(d[5]), uniform_bytes(d[6]),
                      uniform_bytes(d[7]));
}

__device__ __forceinline__ uint32_t pair_word(uint32_t x) {
  const uint32_t p = x & (kRank - 1);
  const uint32_t q = p ^ (1u + static_cast<uint32_t>((static_cast<uint64_t>(kRank - 1) * x) >> 32));
  return p | (q << 8);
}

__global__ void __launch_bounds__(kThreads) pairs_kernel(uint8_t* __restrict__ out,
                                                         uint32_t n_hashes, Key key,
                                                         bool b_side) {
  const uint32_t h = blockIdx.x * blockDim.x + threadIdx.x;
  if (h >= n_hashes) return;
  uint32_t d[8];
  blake3::noise_hash(key.w, h, 1, b_side, d);
  // 8 pairs = 16 bytes: (p0, q0, p1, q1, ...).
  uint4 v;
  v.x = pair_word(d[0]) | (pair_word(d[1]) << 16);
  v.y = pair_word(d[2]) | (pair_word(d[3]) << 16);
  v.z = pair_word(d[4]) | (pair_word(d[5]) << 16);
  v.w = pair_word(d[6]) | (pair_word(d[7]) << 16);
  reinterpret_cast<uint4*>(out)[h] = v;
}

__device__ __forceinline__ int8_t byte_of(const uint4& v, int i) {
  const uint32_t w = i < 4 ? v.x : i < 8 ? v.y : i < 12 ? v.z : v.w;
  return static_cast<int8_t>(w >> (8 * (i & 3)));
}

// One CTA handles kRowsPerCta rows; thread items are 16-entry chunks, consecutive threads on
// consecutive chunks of a row so loads and stores coalesce.
__global__ void __launch_bounds__(kThreads) noised_kernel(int8_t* out, uint32_t rows, uint32_t k,
                                                          const int8_t* __restrict__ factor,
                                                          const uint8_t* __restrict__ pairs,
                                                          OperandSource src) {
  __shared__ int8_t f[kRowsPerCta][kRank];
  const uint32_t row0 = blockIdx.x * kRowsPerCta;
  const uint32_t nrows = min(static_cast<uint32_t>(kRowsPerCta), rows - row0);
  for (uint32_t t = threadIdx.x; t < nrows * (kRank / 16); t += blockDim.x) {
    reinterpret_cast<uint4*>(&f[0][0])[t] =
        reinterpret_cast<const uint4*>(factor + static_cast<size_t>(row0) * kRank)[t];
  }
  __syncthreads();

  const uint32_t chunks_per_row = k / 16;
  const uint32_t items = nrows * chunks_per_row;
  for (uint32_t item = threadIdx.x; item < items; item += blockDim.x) {
    const uint32_t rl = item / chunks_per_row;
    const uint32_t chunk = item - rl * chunks_per_row;
    const uint64_t e0 = static_cast<uint64_t>(row0 + rl) * k + 16ull * chunk;

    uint4 x;
    if (src.base != nullptr) {
      x = *reinterpret_cast<const uint4*>(src.base + e0);
    } else {
      const uint64_t w0 = fill_int7_word(src.fill_stream, e0 >> 3);
      const uint64_t w1 = fill_int7_word(src.fill_stream, (e0 >> 3) + 1);
      x = make_uint4(static_cast<uint32_t>(w0), static_cast<uint32_t>(w0 >> 32),
                     static_cast<uint32_t>(w1), static_cast<uint32_t>(w1 >> 32));
    }
    const uint4 pq0 = __ldg(reinterpret_cast<const uint4*>(pairs) + 2 * chunk);
    const uint4 pq1 = __ldg(reinterpret_cast<const uint4*>(pairs) + 2 * chunk + 1);
    const bool patched = src.patch_len != 0 && e0 < src.patch_off + src.patch_len &&
                         src.patch_off < e0 + 16;

    uint32_t o[4];
#pragma unroll
    for (int wi = 0; wi < 4; ++wi) {
      uint32_t word = 0;
#pragma unroll
      for (int bi = 0; bi < 4; ++bi) {
        const int i = 4 * wi + bi;  // entry within the chunk
        int base = byte_of(x, i);
        if (patched) {
          const uint64_t e = e0 + i;
          if (e >= src.patch_off && e < src.patch_off + src.patch_len)
            base = src.patch[e - src.patch_off];
        }
        const uint4& pq = i < 8 ? pq0 : pq1;
        const int j = (2 * i) & 15;  // byte of p(l) inside pq
        const uint32_t p = static_cast<uint8_t>(byte_of(pq, j));
        const uint32_t q = static_cast<uint8_t>(byte_of(pq, j + 1));
        const int v = base + static_cast<int>(f[rl][p]) - static_cast<int>(f[rl][q]);
        word |= (static_cast<uint32_t>(v) & 0xFFu) << (8 * bi);
      }
      o[wi] = word;
    }
    *reinterpret_cast<uint4*>(out + e0) = make_uint4(o[0], o[1], o[2], o[3]);
  }
}

__global__ void __launch_bounds__(kThreads) fill_kernel(int8_t* __restrict__ out, uint64_t words,
                                                        uint64_t stream) {
  const uint64_t w = static_cast<uint64_t>(blockIdx.x) * blockDim.x + threadIdx.x;
  if (w >= words) return;
  reinterpret_cast<uint64_t*>(out)[w] = fill_int7_word(stream, w);
}

}  // namespace

cudaError_t launch_uniform_factor(int8_t* out, uint32_t rows, const Key& key, bool b_side,
                                  cudaStream_t stream) {
  const uint32_t n_hashes = rows * (kRank / 32);
  if (n_hashes == 0) return cudaSuccess;
  uniform_kernel<<<(n_hashes + kThreads - 1) / kThreads, kThreads, 0, stream>>>(out, n_hashes, key,
                                                                                 b_side);
  return cudaGetLastError();
}

cudaError_t launch_pairs(uint8_t* out, uint32_t k, const Key& key, bool b_side,
                         cudaStream_t stream) {
  if (k % 8 != 0) return cudaErrorInvalidValue;
  const uint32_t n_hashes = k / 8;
  if (n_hashes == 0) return cudaSuccess;
  pairs_kernel<<<(n_hashes + kThreads - 1) / kThreads, kThreads, 0, stream>>>(out, n_hashes, key,
                                                                               b_side);
  return cudaGetLastError();
}

cudaError_t launch_noised_operand(int8_t* out, uint32_t rows, uint32_t k, const int8_t* factor,
                                  const uint8_t* pairs, const OperandSource& src,
                                  cudaStream_t stream) {
  if (k % 16 != 0) return cudaErrorInvalidValue;
  if (rows == 0 || k == 0) return cudaSuccess;
  const uint32_t ctas = (rows + kRowsPerCta - 1) / kRowsPerCta;
  noised_kernel<<<ctas, kThreads, 0, stream>>>(out, rows, k, factor, pairs, src);
  return cudaGetLastError();
}

cudaError_t launch_fill(int8_t* out, uint64_t entries, uint64_t fill_stream, cudaStream_t stream) {
  if (entries % 8 != 0) return cudaErrorInvalidValue;
  const uint64_t words = entries / 8;
  if (words == 0) return cudaSuccess;
  const uint64_t ctas = (words + kThreads - 1) / kThreads;
  if (ctas > 0x7FFFFFFFull) return cudaErrorInvalidValue;
  fill_kernel<<<static_cast<unsigned>(ctas), kThreads, 0, stream>>>(out, words, fill_stream);
  return cudaGetLastError();
}

}  // namespace prep
}  // namespace spm
