// BLAKE3 compression of a single 64-byte block, for device and host code.
//
// Everything the PoW path hashes on the GPU is exactly one 64-byte block (the noise hashes and
// the jackpot digest), so only the compression function is needed: counter 0, block_len 64,
// flags CHUNK_START | CHUNK_END | ROOT (plus KEYED_HASH when keyed). The 32-byte output is the
// first 8 words of the compression output, i.e. what `blake3::keyed_hash` / `blake3::hash` return
// for a 64-byte input.
//
// The round layout (G mixing over columns then diagonals, message permutation between rounds)
// follows the BLAKE3 specification and csrc/blake3/blake3.cuh of the pearl-gemm miner in the
// official Pearl monorepo (pinned at 3fe226761a139a9652b8f28a6464a4bbc25986c8), which carries
// this notice:
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
#pragma once

#include <cstdint>

#if defined(__CUDACC__)
#define SPM_HD __host__ __device__ __forceinline__
#else
#define SPM_HD inline
#endif

namespace spm {
namespace blake3 {

constexpr uint32_t CHUNK_START = 1u << 0;
constexpr uint32_t CHUNK_END = 1u << 1;
constexpr uint32_t ROOT = 1u << 3;
constexpr uint32_t KEYED_HASH = 1u << 4;

/// Flags of a keyed hash whose whole input is one 64-byte block.
constexpr uint32_t FLAGS_KEYED_SINGLE_BLOCK = KEYED_HASH | CHUNK_START | CHUNK_END | ROOT;
/// Flags of an unkeyed hash whose whole input is one 64-byte block.
constexpr uint32_t FLAGS_SINGLE_BLOCK = CHUNK_START | CHUNK_END | ROOT;

constexpr uint32_t IV0 = 0x6A09E667u, IV1 = 0xBB67AE85u, IV2 = 0x3C6EF372u, IV3 = 0xA54FF53Au;
constexpr uint32_t IV4 = 0x510E527Fu, IV5 = 0x9B05688Cu, IV6 = 0x1F83D9ABu, IV7 = 0x5BE0CD19u;

SPM_HD uint32_t rotr32(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }

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

/// One round: columns, then diagonals.
SPM_HD void round_fn(uint32_t (&v)[16], const uint32_t (&m)[16]) {
  g(v[0], v[4], v[8], v[12], m[0], m[1]);
  g(v[1], v[5], v[9], v[13], m[2], m[3]);
  g(v[2], v[6], v[10], v[14], m[4], m[5]);
  g(v[3], v[7], v[11], v[15], m[6], m[7]);
  g(v[0], v[5], v[10], v[15], m[8], m[9]);
  g(v[1], v[6], v[11], v[12], m[10], m[11]);
  g(v[2], v[7], v[8], v[13], m[12], m[13]);
  g(v[3], v[4], v[9], v[14], m[14], m[15]);
}

/// The fixed message permutation applied between rounds: m'[i] = m[P[i]] with
/// P = {2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8}. Fully unrolled, it is register
/// renaming only.
SPM_HD void permute(uint32_t (&m)[16]) {
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

/// Compresses the 64-byte block `block` (16 little-endian words) under the chaining value `cv`
/// with counter 0 and block_len 64, and writes the first 8 output words to `out`.
SPM_HD void compress64(const uint32_t (&cv)[8], const uint32_t (&block)[16], uint32_t flags,
                       uint32_t (&out)[8]) {
  uint32_t v[16] = {cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7],
                    IV0,   IV1,   IV2,   IV3,   0u,    0u,    64u,   flags};
  uint32_t m[16];
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 0; i < 16; ++i) m[i] = block[i];
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int r = 0; r < 7; ++r) {
    round_fn(v, m);
    if (r < 6) permute(m);
  }
#if defined(__CUDA_ARCH__)
#pragma unroll
#endif
  for (int i = 0; i < 8; ++i) out[i] = v[i] ^ v[i + 8];
}

/// `blake3::keyed_hash(key, block)` for a 64-byte block; key and output as LE words.
SPM_HD void keyed_hash64(const uint32_t (&key)[8], const uint32_t (&block)[16], uint32_t (&out)[8]) {
  compress64(key, block, FLAGS_KEYED_SINGLE_BLOCK, out);
}

/// `blake3::hash(block)` for a 64-byte block.
SPM_HD void hash64(const uint32_t (&block)[16], uint32_t (&out)[8]) {
  const uint32_t iv[8] = {IV0, IV1, IV2, IV3, IV4, IV5, IV6, IV7};
  compress64(iv, block, FLAGS_SINGLE_BLOCK, out);
}

}  // namespace blake3
}  // namespace spm
