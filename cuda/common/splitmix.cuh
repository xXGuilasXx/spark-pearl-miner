// SplitMix64 fill of the committed matrices, identical to spm_cpuref::problem::fill_int7.
//
// fill_int7(seed, domain, len) seeds SplitMix64 with `seed ^ domain`; output word w (0-based) is
// mix(s + (w + 1) * GAMMA) and entry e is byte e % 8 (little endian) of word e / 8, mapped to
// (byte & 0x7f) - 64. Being counter based, any entry can be produced directly on the GPU.
#pragma once

#include <stdint.h>

#include "spm_hd.cuh"

namespace spm {

constexpr uint64_t kSplitMixGamma = 0x9E3779B97F4A7C15ull;
// Stream separators of Problem::generate: "spm-a-01" and "spm-b-01" in ASCII.
constexpr uint64_t kDomainA = 0x73706D2D612D3031ull;
constexpr uint64_t kDomainBt = 0x73706D2D622D3031ull;

SPM_HD uint64_t splitmix_mix(uint64_t z) {
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
  return z ^ (z >> 31);
}

// Word `w` of the stream whose state starts at `stream` (= seed ^ domain).
SPM_HD uint64_t splitmix_word(uint64_t stream, uint64_t w) {
  return splitmix_mix(stream + (w + 1u) * kSplitMixGamma);
}

// The 8 entries of word `w` packed in a u64 (entry e % 8 in byte e % 8), each byte already
// mapped to (byte & 0x7f) - 64 as a two's-complement byte. Bytewise, with x = byte & 0x7f in
// [0, 127]: x - 64 equals (x ^ 0x40) with bit 7 set exactly when x < 64, so no borrow ever
// crosses a byte.
SPM_HD uint64_t fill_int7_word(uint64_t stream, uint64_t w) {
  const uint64_t b = splitmix_word(stream, w) & 0x7F7F7F7F7F7F7F7Full;
  const uint64_t below = ~b & 0x4040404040404040ull;  // bit 6 clear  <=>  x < 64
  return (b ^ 0x4040404040404040ull) | (below << 1);
}

// Entry `e` of fill_int7(seed, domain, ...), with stream = seed ^ domain.
SPM_HD int8_t fill_int7_entry(uint64_t stream, uint64_t e) {
  const uint8_t byte = static_cast<uint8_t>(splitmix_word(stream, e >> 3) >> (8 * (e & 7)));
  return static_cast<int8_t>(static_cast<int>(byte & 0x7F) - 64);
}

}  // namespace spm
