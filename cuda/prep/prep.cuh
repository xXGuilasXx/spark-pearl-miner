// Noise and operand preparation (spm-cpuref README, sections 3 and 4).
//
//   uniform factor  L (rows x 128):  row i = hashes H(4i .. 4i+3, label, key, slot 0), each byte
//                                    mapped to (byte & 63) - 32
//   pairs           (k entries):     pair l from word l % 8 of H(l / 8, label, key, slot 1):
//                                    p = x & 127, q = p ^ (1 + ((127 * x) >> 32))
//   noised operand  X' (rows x k):   X'[i][l] = X[i][l] + L[i][p(l)] - L[i][q(l)]   (exact s8)
//
// A side: L = A_L, pairs = A_R, key = a_noise_seed, label "A_tensor".
// B side: L = B_Rᵀ, pairs = B_L, key = b_noise_seed, label "B_tensor".
#pragma once

#include <cuda_runtime.h>
#include <stdint.h>

namespace spm {
namespace prep {

struct Key {
  uint32_t w[8];  // 32-byte BLAKE3 key as LE words
};

// Where the committed entries X come from.
struct OperandSource {
  const int8_t* base;      // device rows x k matrix (may alias the output), or nullptr to fill
  uint64_t fill_stream;    // seed ^ domain, used when base == nullptr (fill_int7)
  const int8_t* patch;     // device bytes replacing entries [patch_off, patch_off + patch_len)
  uint64_t patch_off;
  uint32_t patch_len;      // 0 = no patch
};

// out[i * 128 + c] for i < rows (rows x 128 bytes).
cudaError_t launch_uniform_factor(int8_t* out, uint32_t rows, const Key& key, bool b_side,
                                  cudaStream_t stream);

// out[2l] = p(l), out[2l + 1] = q(l) for l < k (k multiple of 8).
cudaError_t launch_pairs(uint8_t* out, uint32_t k, const Key& key, bool b_side,
                         cudaStream_t stream);

// out (rows x k, k multiple of 16) = X + L[., p] - L[., q].
cudaError_t launch_noised_operand(int8_t* out, uint32_t rows, uint32_t k, const int8_t* factor,
                                  const uint8_t* pairs, const OperandSource& src,
                                  cudaStream_t stream);

// out (rows x k) = fill_int7 entries (debug and tests: the committed matrix itself).
cudaError_t launch_fill(int8_t* out, uint64_t entries, uint64_t fill_stream, cudaStream_t stream);

}  // namespace prep
}  // namespace spm
