// BLAKE3 compression of a single 64-byte block, device and host side, plus the noise-hash message
// builder of PearlHash V3.
//
// The round/permutation layout (everything in registers, output = state[0..8] ^ state[8..16]) and
// the single-block flag sets follow miner/pearl-gemm/csrc/blake3/blake3.cuh of the official Pearl
// monorepo (pinned at 3fe226761a139a9652b8f28a6464a4bbc25986c8), rewritten here without CuTe:
//
//   Copyright (c) 2025-2026 Pearl Research Labs
//   Copyright (c) 2015-2016 The Decred developers
//
//   Permission to use, copy, modify, and distribute this software for any purpose with or without
//   fee is hereby granted, provided that the above copyright notice and this permission notice
//   appear in all copies.
//
//   THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
//   SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
//   AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//   WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
//   NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
//   PERFORMANCE OF THIS SOFTWARE.
//
// What PearlHash needs from BLAKE3 is always a one-block input (64 bytes, counter 0), so the hash
// is exactly one compression with CHUNK_START | CHUNK_END | ROOT (| KEYED_HASH when keyed):
//   * the noise hash   H(i, label, seed, slot) = blake3_keyed(seed, msg(i, slot, label))
//   * the jackpot hash digest = blake3_keyed(a_noise_seed, transcript[0..16] as LE words)
#pragma once
#include <stdint.h>

#include "defs.cuh"

namespace spm {
namespace b3 {

constexpr uint32_t IV0 = 0x6A09E667u, IV1 = 0xBB67AE85u, IV2 = 0x3C6EF372u, IV3 = 0xA54FF53Au;
constexpr uint32_t IV4 = 0x510E527Fu, IV5 = 0x9B05688Cu, IV6 = 0x1F83D9ABu, IV7 = 0x5BE0CD19u;

constexpr uint32_t CHUNK_START = 1u << 0;
constexpr uint32_t CHUNK_END = 1u << 1;
constexpr uint32_t PARENT = 1u << 2;
constexpr uint32_t ROOT = 1u << 3;
constexpr uint32_t KEYED_HASH = 1u << 4;
/// Flags of a keyed hash whose whole input is one 64-byte block.
constexpr uint32_t FLAGS_KEYED_ONE_BLOCK = KEYED_HASH | CHUNK_START | CHUNK_END | ROOT;
/// Flags of an unkeyed hash whose whole input is one 64-byte block.
constexpr uint32_t FLAGS_ONE_BLOCK = CHUNK_START | CHUNK_END | ROOT;
constexpr uint32_t BLOCK_LEN = 64;

SPM_HD uint32_t rotr32(uint32_t x, uint32_t n) { return (x >> n) | (x << (32u - n)); }

#define SPM_B3_G(a, b, c, d, x, y) \
  do {                             \
    a = a + b + (x);               \
    d = rotr32(d ^ a, 16);         \
    c = c + d;                     \
    b = rotr32(b ^ c, 12);         \
    a = a + b + (y);               \
    d = rotr32(d ^ a, 8);          \
    c = c + d;                     \
    b = rotr32(b ^ c, 7);          \
  } while (0)

/// One compression of a full 64-byte block with counter 0: `out` = the first 8 output words, i.e.
/// the 32-byte hash (LE words) of a one-block input when `flags` contains ROOT. `cv` is the key for
/// a keyed hash and the IV for an unkeyed one. Fully unrolled: every index is static, so the
/// state, the message and its permutations stay in registers.
SPM_HD void compress_block(const uint32_t cv[8], const uint32_t block[16], uint32_t flags,
                           uint32_t out[8]) {
  uint32_t m0 = block[0], m1 = block[1], m2 = block[2], m3 = block[3];
  uint32_t m4 = block[4], m5 = block[5], m6 = block[6], m7 = block[7];
  uint32_t m8 = block[8], m9 = block[9], m10 = block[10], m11 = block[11];
  uint32_t m12 = block[12], m13 = block[13], m14 = block[14], m15 = block[15];
  uint32_t v0 = cv[0], v1 = cv[1], v2 = cv[2], v3 = cv[3];
  uint32_t v4 = cv[4], v5 = cv[5], v6 = cv[6], v7 = cv[7];
  uint32_t v8 = IV0, v9 = IV1, v10 = IV2, v11 = IV3;
  uint32_t v12 = 0, v13 = 0, v14 = BLOCK_LEN, v15 = flags;
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int round = 0; round < 7; ++round) {
    SPM_B3_G(v0, v4, v8, v12, m0, m1);
    SPM_B3_G(v1, v5, v9, v13, m2, m3);
    SPM_B3_G(v2, v6, v10, v14, m4, m5);
    SPM_B3_G(v3, v7, v11, v15, m6, m7);
    SPM_B3_G(v0, v5, v10, v15, m8, m9);
    SPM_B3_G(v1, v6, v11, v12, m10, m11);
    SPM_B3_G(v2, v7, v8, v13, m12, m13);
    SPM_B3_G(v3, v4, v9, v14, m14, m15);
    if (round < 6) {
      // Message permutation {2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8}.
      const uint32_t t0 = m0, t1 = m1, t2 = m2, t3 = m3, t4 = m4, t5 = m5, t6 = m6, t7 = m7;
      const uint32_t t8 = m8, t9 = m9, t10 = m10, t11 = m11, t12 = m12, t13 = m13, t14 = m14, t15 = m15;
      m0 = t2; m1 = t6; m2 = t3; m3 = t10; m4 = t7; m5 = t0; m6 = t4; m7 = t13;
      m8 = t1; m9 = t11; m10 = t12; m11 = t5; m12 = t9; m13 = t14; m14 = t15; m15 = t8;
    }
  }
  out[0] = v0 ^ v8;
  out[1] = v1 ^ v9;
  out[2] = v2 ^ v10;
  out[3] = v3 ^ v11;
  out[4] = v4 ^ v12;
  out[5] = v5 ^ v13;
  out[6] = v6 ^ v14;
  out[7] = v7 ^ v15;
}

#undef SPM_B3_G

/// `blake3_keyed(key, block)` for a 64-byte input.
SPM_HD void keyed_hash_one_block(const uint32_t key[8], const uint32_t block[16], uint32_t out[8]) {
  compress_block(key, block, FLAGS_KEYED_ONE_BLOCK, out);
}

}  // namespace b3

/// Noise-hash labels as LE words: "A_tensor" / "B_tensor" zero-padded to 32 bytes.
constexpr uint32_t LABEL_A_W0 = 0x65745f41u;  // "A_te"
constexpr uint32_t LABEL_B_W0 = 0x65745f42u;  // "B_te"
constexpr uint32_t LABEL_W1 = 0x726f736eu;    // "nsor"

/// The PearlHash V3 noise hash `H(i, label, seed, slot)`: BLAKE3 keyed with the noise seed over a
/// 64-byte message that is zero except word `slot` = i + 1 (i32 LE) and bytes 32..64 = label.
/// `label_w0` is LABEL_A_W0 or LABEL_B_W0; slot 0 feeds the uniform factors (A_L, B_Rᵀ), slot 1 the
/// permutation pairs (A_R, B_L).
template <int kSlot>
SPM_HD void noise_hash(const uint32_t seed[8], uint32_t label_w0, uint32_t index, uint32_t out[8]) {
  static_assert(kSlot >= 0 && kSlot < 8, "slot must address one of the first 8 message words");
  uint32_t msg[16];
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 0; i < 16; ++i) msg[i] = 0;
  msg[kSlot] = index + 1u;
  msg[8] = label_w0;
  msg[9] = LABEL_W1;
  b3::keyed_hash_one_block(seed, msg, out);
}

/// Loads 32 little-endian bytes as 8 words (host side helper for keys, seeds and bounds).
inline void le_words_from_bytes(const uint8_t bytes[32], uint32_t words[8]) {
  for (int i = 0; i < 8; ++i) {
    words[i] = (uint32_t)bytes[4 * i] | ((uint32_t)bytes[4 * i + 1] << 8) |
               ((uint32_t)bytes[4 * i + 2] << 16) | ((uint32_t)bytes[4 * i + 3] << 24);
  }
}

}  // namespace spm
