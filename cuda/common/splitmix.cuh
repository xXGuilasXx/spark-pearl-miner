// SplitMix64 fill of the int7 matrices, bit-identical to spm_cpuref::problem::fill_int7.
//
// The stream seeded with s is counter based: word w (0-based) = mix(s + (w + 1) * GAMMA), and entry
// i of the matrix is byte (i % 8) of word (i / 8), mapped to (byte & 0x7f) - 64 in [-64, 63]. A
// seeded matrix is therefore never stored: any 8-aligned group of entries is regenerated from its
// index, which is what the noised-operand kernels do.
#pragma once
#include <stdint.h>

#include "defs.cuh"

namespace spm {

/// "spm-a-01" in ASCII: stream separator of A (spm_cpuref::DOMAIN_A).
constexpr uint64_t DOMAIN_A = 0x73706d2d612d3031ull;
/// "spm-b-01" in ASCII: stream separator of Bᵀ (spm_cpuref::DOMAIN_BT).
constexpr uint64_t DOMAIN_BT = 0x73706d2d622d3031ull;
constexpr uint64_t SPLITMIX_GAMMA = 0x9e3779b97f4a7c15ull;

SPM_HD uint64_t splitmix64_mix(uint64_t z) {
  z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
  z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
  return z ^ (z >> 31);
}

/// Word `w` (0-based) of the SplitMix64 stream seeded with `state`.
SPM_HD uint64_t splitmix64_word(uint64_t state, uint64_t w) {
  return splitmix64_mix(state + (w + 1) * SPLITMIX_GAMMA);
}

/// Four packed entries (x & 0x7f) - 64 from four random bytes: per-byte, no carries between lanes.
/// With y = (x & 0x7f) ^ 0x40 the result is y with bit 7 set to bit 6 of y (so 0..63 -> -64..-1 and
/// 64..127 -> 0..63), which is exactly the two's-complement byte of x - 64.
SPM_HD uint32_t int7x4(uint32_t bytes) {
  const uint32_t y = (bytes & 0x7f7f7f7fu) ^ 0x40404040u;
  return y | ((y & 0x40404040u) << 1);
}

}  // namespace spm
