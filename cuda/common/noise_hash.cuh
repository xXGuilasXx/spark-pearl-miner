// Noise hash of the rank-128 PearlHash noise (see crates/spm-cpuref/README.md, section 3).
//
//   H(i, label, key, slot) = blake3_keyed(key, msg), msg = 64 bytes, zero except
//   bytes 4*slot .. 4*slot+4 = (i + 1) as i32 LE and bytes 32..64 = label.
//
// Slot 0 feeds the uniform factors (A_L, B_Rᵀ), slot 1 the permutation pairs (A_R, B_L). The A
// side is keyed with a_noise_seed and labelled "A_tensor", the B side with b_noise_seed and
// "B_tensor" (labels zero-padded to 32 bytes). This matches the data layout of the official
// noise generator (csrc/gemm/noise_generation_kernel.h of pearl-gemm, ISC, and
// zk_pow::circuit::pearl_noise).
#pragma once

#include <cstdint>

#include "blake3.cuh"

namespace spm {

/// Noise rank r. The kernels are written for r = 128 only (the only rank without a penalty).
constexpr uint32_t NOISE_RANK = 128;

/// 8 little-endian words (a BLAKE3 key or a 32-byte label).
struct Words8 {
  uint32_t w[8];
};

/// "A_tensor" zero-padded to 32 bytes, as LE words.
constexpr Words8 LABEL_A = {{0x65745f41u /* "A_te" */, 0x726f736eu /* "nsor" */, 0, 0, 0, 0, 0, 0}};
/// "B_tensor" zero-padded to 32 bytes, as LE words.
constexpr Words8 LABEL_B = {{0x65745f42u /* "B_te" */, 0x726f736eu /* "nsor" */, 0, 0, 0, 0, 0, 0}};

/// H(index, label, key, slot) as 8 LE words.
SPM_HD void noise_hash(const Words8& key, const Words8& label, uint32_t index, int slot,
                       uint32_t (&out)[8]) {
  uint32_t msg[16];
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 0; i < 8; ++i) {
    msg[i] = (i == slot) ? index + 1u : 0u;
    msg[8 + i] = label.w[i];
  }
  const uint32_t (&k)[8] = key.w;
  blake3::keyed_hash64(k, msg, out);
}

/// Uniform factor bytes of one hash word: each byte b -> (b & 63) - 32 in [-32, 31], two's
/// complement. v = b & 63; v >= 32: v - 32 = v ^ 0x20. v < 32: v - 32 = v + 0xe0 = v | 0xe0.
/// Both are (v ^ 0x20) | ((~v & 0x20) ? 0xc0 : 0); bits 6 and 7 come from bit 5 of the same
/// byte, so nothing crosses a byte boundary.
SPM_HD uint32_t uniform_bytes(uint32_t word) {
  const uint32_t v = word & 0x3f3f3f3fu;
  const uint32_t low = ~v & 0x20202020u;  // bit 5 clear -> v < 32
  return (v ^ 0x20202020u) | (low << 1) | (low << 2);
}

/// Permutation pair of one hash word x (r = 128): p = x & 127,
/// q = p ^ (1 + ((127 * x) >> 32)). Always p != q, both < 128.
SPM_HD void perm_pair(uint32_t x, uint32_t& p, uint32_t& q) {
  p = x & (NOISE_RANK - 1);
  const uint32_t hi = static_cast<uint32_t>((static_cast<uint64_t>(NOISE_RANK - 1) * x) >> 32);
  q = p ^ (1u + hi);
}

}  // namespace spm
