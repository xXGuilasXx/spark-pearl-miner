// Operand preparation for PearlHash V3 (rank r = 128): the uniform noise factors, the permutation
// pairs and the noised s8 operands, bit-identical to spm-cpuref (crates/spm-cpuref/README.md §3-4).
//
//   factor row i (A_L or B_Rᵀ, 128 bytes) = H(4i) ‖ H(4i+1) ‖ H(4i+2) ‖ H(4i+3), slot 0, each byte
//                                           mapped to (byte & 63) - 32
//   pair l (A_R or B_L)                   = word l % 8 of H(l / 8), slot 1:
//                                           p = x & 127, q = p ^ (1 + ((127·x) >> 32))
//   E[i][l]  = factor[i][p(l)] - factor[i][q(l)]         in [-63, 63]
//   X'[i][l] = X[i][l] + E[i][l]                         in [-127, 127], exact s8
//
// X is either a stored matrix or the SplitMix64 int7 stream of a seed (never materialized).
#pragma once
#include <cuda_runtime.h>
#include <stdint.h>

namespace spm {
namespace prep {

constexpr uint32_t RANK = 128;

/// 32-byte BLAKE3 key / noise seed as 8 LE words, passed by value to kernels.
struct Seed {
  uint32_t w[8];
};

/// Where the un-noised entries come from.
struct OperandSource {
  const int8_t* stored;   // rows×k row-major, or nullptr to regenerate from `gen_state`
  uint64_t gen_state;     // seed ^ DOMAIN_A / DOMAIN_BT of the SplitMix64 stream
  const uint8_t* prefix;  // optional override of the first `prefix_len` entries (nonce patch)
  uint32_t prefix_len;
};

/// rows×128 uniform factor (A_L with LABEL_A / a_noise_seed, B_Rᵀ with LABEL_B / b_noise_seed).
cudaError_t launch_uniform_factor(int8_t* out, uint32_t rows, Seed seed, uint32_t label_w0,
                                  cudaStream_t stream);

/// k permutation pairs as bytes {p, q} (k×2).
cudaError_t launch_pairs(uint8_t* pairs, uint32_t k, Seed seed, uint32_t label_w0,
                         cudaStream_t stream);

/// out = source + E (or E alone when `noise_only`), rows×k row-major s8. `out` may alias
/// `source.stored` (each thread reads its 16 bytes before writing them).
cudaError_t launch_noised_operand(int8_t* out, OperandSource source, const int8_t* factor,
                                  const uint8_t* pairs, uint32_t rows, uint32_t k, bool noise_only,
                                  cudaStream_t stream);

/// The raw int7 stream: out[i] = fill_int7(seed, domain)[i] for i < len (len a multiple of 16).
cudaError_t launch_fill_int7(int8_t* out, uint64_t len, uint64_t gen_state, cudaStream_t stream);

}  // namespace prep
}  // namespace spm
