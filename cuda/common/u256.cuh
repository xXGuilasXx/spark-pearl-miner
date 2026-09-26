// 256-bit little-endian comparison used by the difficulty check.
//
// A digest and a bound are 32 bytes each, read as `U256::from_little_endian`: word 0 (bytes
// 0..4, LE) is the least significant, word 7 the most significant.
#pragma once

#include <cstdint>

#include "blake3.cuh"  // SPM_HD

namespace spm {

/// `a <= b` for two LE U256 given as 8 LE words each. Branch free: scans from the least
/// significant word, the last (most significant) differing word decides.
SPM_HD bool u256_le(const uint32_t (&a)[8], const uint32_t (&b)[8]) {
  bool le = true;  // equal so far
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 0; i < 8; ++i) {
    le = (a[i] < b[i]) || (a[i] == b[i] && le);
  }
  return le;
}

}  // namespace spm
