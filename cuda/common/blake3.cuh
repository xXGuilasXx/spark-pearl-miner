// BLAKE3 compression of one 64-byte block, usable from host and device code.
//
// Adapted from miner/pearl-gemm/csrc/blake3/blake3.cuh and blake3_constants.hpp of the official
// Pearl monorepo (https://github.com/pearl-research-labs/pearl, pinned at 3fe2267), rewritten
// without CuTe tensors so the same code runs in the host commitment helpers, the noise kernels
// and the GEMM epilogue. Original notice:
//
//   ISC License
//
//   Copyright (c) 2025-2026 Pearl Research Labs
//   Copyright (c) 2015-2016 The Decred developers
//
//   Permission to use, copy, modify, and distribute this software for any
//   purpose with or without fee is hereby granted, provided that the above
//   copyright notice and this permission notice appear in all copies.
//
//   THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//   WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//   MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//   ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//   WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//   ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//   OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Everything here works on little-endian 32-bit words: a 64-byte message is 16 words, a key or a
// digest is 8 words (byte i of the digest is byte i % 4 of word i / 4).
#pragma once

#include <stdint.h>

#include "spm_hd.cuh"

namespace spm {
namespace blake3 {

constexpr uint32_t kChunkStart = 1u << 0;
constexpr uint32_t kChunkEnd = 1u << 1;
constexpr uint32_t kParent = 1u << 2;
constexpr uint32_t kRoot = 1u << 3;
constexpr uint32_t kKeyedHash = 1u << 4;

constexpr uint32_t kBlockLen = 64;

constexpr uint32_t kIV0 = 0x6A09E667u;
constexpr uint32_t kIV1 = 0xBB67AE85u;
constexpr uint32_t kIV2 = 0x3C6EF372u;
constexpr uint32_t kIV3 = 0xA54FF53Au;
constexpr uint32_t kIV4 = 0x510E527Fu;
constexpr uint32_t kIV5 = 0x9B05688Cu;
constexpr uint32_t kIV6 = 0x1F83D9ABu;
constexpr uint32_t kIV7 = 0x5BE0CD19u;

SPM_HD uint32_t rotr32(uint32_t x, uint32_t n) {
#if defined(__CUDA_ARCH__)
  return __funnelshift_r(x, x, n);
#else
  return (x >> n) | (x << (32u - n));
#endif
}

// The quarter-round G of the BLAKE3 specification (section 2.2).
SPM_HD void g(uint32_t& a, uint32_t& b, uint32_t& c, uint32_t& d, uint32_t mx, uint32_t my) {
  a = a + b + mx;
  d = rotr32(d ^ a, 16);
  c = c + d;
  b = rotr32(b ^ c, 12);
  a = a + b + my;
  d = rotr32(d ^ a, 8);
  c = c + d;
  b = rotr32(b ^ c, 7);
}

// One round: four column G's then four diagonal G's. Every index is a compile-time constant,
// so the state and the message stay in registers.
SPM_HD void round_fn(uint32_t v[16], const uint32_t m[16]) {
  g(v[0], v[4], v[8], v[12], m[0], m[1]);
  g(v[1], v[5], v[9], v[13], m[2], m[3]);
  g(v[2], v[6], v[10], v[14], m[4], m[5]);
  g(v[3], v[7], v[11], v[15], m[6], m[7]);
  g(v[0], v[5], v[10], v[15], m[8], m[9]);
  g(v[1], v[6], v[11], v[12], m[10], m[11]);
  g(v[2], v[7], v[8], v[13], m[12], m[13]);
  g(v[3], v[4], v[9], v[14], m[14], m[15]);
}

// Message word permutation applied between rounds: m'[i] = m[P[i]] with
// P = {2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8}.
SPM_HD void permute(uint32_t m[16]) {
  const uint32_t t0 = m[0], t1 = m[1], t2 = m[2], t3 = m[3], t4 = m[4], t5 = m[5], t6 = m[6],
                 t7 = m[7], t8 = m[8], t9 = m[9], t10 = m[10], t11 = m[11], t12 = m[12],
                 t13 = m[13], t14 = m[14], t15 = m[15];
  m[0] = t2;
  m[1] = t6;
  m[2] = t3;
  m[3] = t10;
  m[4] = t7;
  m[5] = t0;
  m[6] = t4;
  m[7] = t13;
  m[8] = t1;
  m[9] = t11;
  m[10] = t12;
  m[11] = t5;
  m[12] = t9;
  m[13] = t14;
  m[14] = t15;
  m[15] = t8;
}

// Compresses `block` into the chaining value `cv` (in place). Only the first half of the output
// is produced: that is the chaining value, and for a ROOT block the 32-byte hash.
SPM_HD void compress(uint32_t cv[8], const uint32_t block[16], uint64_t counter, uint32_t block_len,
                     uint32_t flags) {
  uint32_t v[16] = {cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7],
                    kIV0,  kIV1,  kIV2,  kIV3,  static_cast<uint32_t>(counter),
                    static_cast<uint32_t>(counter >> 32), block_len, flags};
  uint32_t m[16];
#pragma unroll
  for (int i = 0; i < 16; ++i) m[i] = block[i];
#pragma unroll
  for (int r = 0; r < 6; ++r) {
    round_fn(v, m);
    permute(m);
  }
  round_fn(v, m);
#pragma unroll
  for (int i = 0; i < 8; ++i) cv[i] = v[i] ^ v[i + 8];
}

// blake3::keyed_hash(key, msg) for a message of exactly 64 bytes (one block, one chunk).
SPM_HD void keyed_hash_64(const uint32_t key[8], const uint32_t msg[16], uint32_t out[8]) {
#pragma unroll
  for (int i = 0; i < 8; ++i) out[i] = key[i];
  compress(out, msg, 0, kBlockLen, kKeyedHash | kChunkStart | kChunkEnd | kRoot);
}

// blake3::hash(msg) (unkeyed) for a message of exactly 128 bytes: two blocks of one chunk.
// Used for the job key blake3(header76 || config52).
SPM_HD void hash_128(const uint32_t msg[32], uint32_t out[8]) {
  out[0] = kIV0;
  out[1] = kIV1;
  out[2] = kIV2;
  out[3] = kIV3;
  out[4] = kIV4;
  out[5] = kIV5;
  out[6] = kIV6;
  out[7] = kIV7;
  compress(out, msg, 0, kBlockLen, kChunkStart);
  compress(out, msg + 16, 0, kBlockLen, kChunkEnd | kRoot);
}

// Labels of the noise hashes: "A_tensor" / "B_tensor" zero-padded to 32 bytes, as LE words.
constexpr uint32_t kLabelA0 = 0x65745F41u;  // "A_te"
constexpr uint32_t kLabelB0 = 0x65745F42u;  // "B_te"
constexpr uint32_t kLabel1 = 0x726F736Eu;   // "nsor"

// Message of the noise hash H(index, label, key, slot): 64 bytes, zero except the i32 LE
// `index + 1` at bytes 4*slot .. 4*slot+4 and the 32-byte label at bytes 32..64
// (spm-cpuref README, "Noise"). `slot` is 0 for the uniform factors and 1 for the pairs.
SPM_HD void noise_message(uint32_t msg[16], uint32_t index, uint32_t slot, bool b_side) {
#pragma unroll
  for (int i = 0; i < 16; ++i) msg[i] = 0;
  msg[slot] = index + 1u;
  msg[8] = b_side ? kLabelB0 : kLabelA0;
  msg[9] = kLabel1;
}

// H(index, label, key, slot): keyed BLAKE3 of the noise message.
SPM_HD void noise_hash(const uint32_t key[8], uint32_t index, uint32_t slot, bool b_side,
                       uint32_t out[8]) {
  uint32_t msg[16];
  noise_message(msg, index, slot, b_side);
  keyed_hash_64(key, msg, out);
}

}  // namespace blake3
}  // namespace spm
