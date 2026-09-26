// 256-bit little-endian comparisons for the difficulty check.
//
// A digest or bound is 32 bytes read as U256::from_little_endian, held as 8 LE u32 words:
// word 0 is the least significant, word 7 the most significant.
#pragma once

#include <stdint.h>

#include "spm_hd.cuh"

namespace spm {

// a <= b.
SPM_HD bool u256_le_leq(const uint32_t a[8], const uint32_t b[8]) {
  bool decided = false;
  bool leq = true;
#pragma unroll
  for (int i = 7; i >= 0; --i) {
    if (!decided && a[i] != b[i]) {
      decided = true;
      leq = a[i] < b[i];
    }
  }
  return leq;
}

// Bytes (little endian) -> 8 words.
SPM_HD void u256_from_bytes(const uint8_t bytes[32], uint32_t words[8]) {
#pragma unroll
  for (int i = 0; i < 8; ++i) {
    words[i] = static_cast<uint32_t>(bytes[4 * i]) | (static_cast<uint32_t>(bytes[4 * i + 1]) << 8) |
               (static_cast<uint32_t>(bytes[4 * i + 2]) << 16) |
               (static_cast<uint32_t>(bytes[4 * i + 3]) << 24);
  }
}

// 8 words -> bytes (little endian).
SPM_HD void u256_to_bytes(const uint32_t words[8], uint8_t bytes[32]) {
#pragma unroll
  for (int i = 0; i < 8; ++i) {
    bytes[4 * i] = static_cast<uint8_t>(words[i]);
    bytes[4 * i + 1] = static_cast<uint8_t>(words[i] >> 8);
    bytes[4 * i + 2] = static_cast<uint8_t>(words[i] >> 16);
    bytes[4 * i + 3] = static_cast<uint8_t>(words[i] >> 24);
  }
}

}  // namespace spm
