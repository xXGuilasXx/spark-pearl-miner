// SplitMix64 int7 generator, bit-identical to spm_cpuref::problem::fill_int7.
//
// The stream seeded with `s` is counter based: word w (0-based) is mix(s + (w + 1) * GAMMA), so
// any thread can produce any word directly. Entry e of a matrix is byte e % 8 (little endian) of
// word e / 8, mapped to (byte & 0x7f) - 64, i.e. uniform in [-64, 63].
#pragma once

#include <cstdint>

#include "blake3.cuh"  // SPM_HD

namespace spm {

constexpr uint64_t SPLITMIX_GAMMA = 0x9e3779b97f4a7c15ull;
/// Stream separator of A ("spm-a-01" in ASCII), spm_cpuref::DOMAIN_A.
constexpr uint64_t DOMAIN_A = 0x73706d2d612d3031ull;
/// Stream separator of Bᵀ ("spm-b-01" in ASCII), spm_cpuref::DOMAIN_BT.
constexpr uint64_t DOMAIN_BT = 0x73706d2d622d3031ull;

SPM_HD uint64_t splitmix_mix(uint64_t z) {
  z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
  z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
  return z ^ (z >> 31);
}

/// Word `w` of the stream whose state starts at `stream_seed` (= seed ^ domain).
SPM_HD uint64_t splitmix_word(uint64_t stream_seed, uint64_t w) {
  return splitmix_mix(stream_seed + (w + 1) * SPLITMIX_GAMMA);
}

/// The 8 entries of one word as two's-complement bytes: per byte v = b & 0x7f, v - 64.
/// v >= 64 (bit 6 set): v - 64 = v ^ 0x40. v < 64: v - 64 = v + 0xc0 = v ^ 0x40 | 0x80.
/// Both cases are (v ^ 0x40) | ((~v & 0x40) << 1); bit 6 moves to bit 7 of the same byte, so no
/// carry crosses a byte boundary.
SPM_HD uint64_t int7_bytes(uint64_t word) {
  const uint64_t v = word & 0x7f7f7f7f7f7f7f7full;
  return (v ^ 0x4040404040404040ull) | ((~v & 0x4040404040404040ull) << 1);
}

}  // namespace spm
