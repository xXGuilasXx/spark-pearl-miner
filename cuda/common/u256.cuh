// 256-bit little-endian comparisons for the difficulty check.
//
// A digest is 32 bytes read as U256::from_little_endian, i.e. 8 LE words where word 7 is the most
// significant; the consensus condition is digest <= bound (pow_utils.hpp check_pow_target in the
// official miner does the same word-wise compare from the top).
#pragma once
#include <stdint.h>

#include "defs.cuh"

namespace spm {

/// `a <= b` for 256-bit values given as 8 little-endian u32 words (word 7 most significant).
/// Branch free: the verdict is decided by the most significant differing word.
SPM_HD bool u256_le_leq(const uint32_t a[8], const uint32_t b[8]) {
  bool decided = false, leq = true;
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 7; i >= 0; --i) {
    const bool differ = a[i] != b[i];
    leq = decided ? leq : (differ ? a[i] < b[i] : leq);
    decided = decided || differ;
  }
  return leq;
}

}  // namespace spm
