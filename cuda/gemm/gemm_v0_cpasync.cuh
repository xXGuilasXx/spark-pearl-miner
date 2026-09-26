// gemm_v0_cpasync — fused PearlHash V3 kernel for sm_121a (strategy A of docs/en/KERNEL.md).
//
// One CTA computes a 128 x 256 block of C' = A'·B'ᵀ with a 3-stage cp.async pipeline over
// k-tiles of 64 and hashes it on the fly:
//
//   * 8 warps in a 2 x 4 grid, each owning a 64 x 64 warp tile made of 4 x 8 m16n8 fragments.
//     In that layout thread `lane` holds accumulator rows {lane/4, lane/4 + 8} and columns
//     {2(lane%4), 2(lane%4)+1} of every fragment: exactly rows t_rows + {0, 8, ..., 56} and
//     columns t_cols + {0, 1, 8, 9, ..., 56, 57} with t_rows = row0 + lane/4 and
//     t_cols = col0 + 2(lane%4). So the 128 accumulators of one thread are one hash tile, and the
//     per-slice fold is thread local (no shuffles, no shared-memory reduction).
//   * Per k32 step a warp issues 4 x 8 = 32 mma.sync.m16n8k32 (4 ldmatrix.x4 for A, 4 for B);
//     per 64-wide k-tile 64 MMAs; per 128-wide transcript slice (r = 128) 128 MMAs per warp,
//     1024 per CTA.
//   * After every slice s the thread XOR-folds its 128 cumulative accumulators into transcript
//     word s % 16 (t = rotl13(t) ^ fold). The 16 words stay in registers as a rotating queue whose
//     head is always the current slot, so no dynamic register indexing (and no local memory) is
//     needed; the queue is rotated back into slot order once, before the digest.
//   * Epilogue: digest = BLAKE3_keyed(a_noise_seed, t[0..16]); if LE-U256(digest) <= bound the
//     tile goes to the hit ring (atomic counter). In dump mode every tile also writes its
//     104-byte record (t_rows, t_cols, t[16], digest) at its reference index.
//
// Shared memory: A 128 x 64 B + B'ᵀ 256 x 64 B = 24 KiB per stage, x 3 stages = 72 KiB (73,728 B).
// Rows are 64 bytes (four 16-byte chunks); chunk c of row r is stored at chunk c ^ ((r >> 1) & 3),
// which makes both the 16-byte cp.async stores and the 8-row ldmatrix phases conflict free.
//
// What bounds it (measured, see crates/spm-gpu/README.md): with the operands already in shared
// memory the loop runs at ~94 T-MAC/s; the L2 -> SM traffic of the 128 x 256 tile (1.5 MiB per
// CTA tile at k = 4096, ~0.95 TB/s at 80 T-MAC/s) costs ~10 % and the DRAM misses ~5 %. Sharing B'ᵀ
// between CTA pairs of a cluster is the next lever.
#pragma once

#include <cstdint>

#include "blake3.cuh"
#include "mma_ops.cuh"
#include "spm_internal.h"
#include "u256.cuh"

namespace spm {
namespace gemm_v0 {

constexpr int kBM = 128;
constexpr int kBN = 256;
constexpr int kBK = 64;
constexpr int kStages = 3;
constexpr int kWarpsM = 2;
constexpr int kWarpsN = 4;
constexpr int kThreads = 32 * kWarpsM * kWarpsN;  // 256
constexpr int kWM = kBM / kWarpsM;                // 64
constexpr int kWN = kBN / kWarpsN;                // 64
constexpr int kFragM = kWM / 16;                  // 4 m16 fragments per warp tile
constexpr int kFragN = kWN / 8;                   // 8 n8 fragments per warp tile
constexpr int kSlice = 128;                       // transcript slice width (noise rank r)
constexpr int kTilesPerSlice = kSlice / kBK;      // 2
constexpr int kTranscriptWords = 16;
constexpr int kRotl = 13;
constexpr int kStageA = kBM * kBK;                // 8192
constexpr int kStageB = kBN * kBK;                // 16384
constexpr int kStageBytes = kStageA + kStageB;    // 24576
constexpr int kSmemBytes = kStages * kStageBytes;  // 73728
constexpr int kDumpRecordBytes = 104;
constexpr int kRowsPerCopy = kThreads / (kBK / 16);  // 64 rows per pass of 16-byte copies
constexpr int kGroupM = 8;  // CTA-tile rows swept together (L2 reuse of A' and B'ᵀ)

static_assert(kWM == 64 && kWN == 64, "the hash tile <-> fragment mapping needs 64 x 64 warp tiles");
static_assert(kSlice % kBK == 0, "a transcript slice must be a whole number of k-tiles");
static_assert(kBM % kRowsPerCopy == 0 && kBN % kRowsPerCopy == 0, "copy passes must tile the stage");
static_assert(kTranscriptWords == 16, "the queue rotation below assumes 16 words");

/// Byte offset of 16-byte chunk `chunk` of row `row` inside a swizzled [rows][64 B] stage.
__device__ __forceinline__ uint32_t swizzle(uint32_t row, uint32_t chunk) {
  return row * kBK + ((chunk ^ ((row >> 1) & 3u)) << 4);
}

__device__ __forceinline__ void cp_async_16(uint32_t dst, const void* src, uint32_t src_bytes) {
  // src_bytes = 0 zero-fills the destination without reading `src`.
  asm volatile("cp.async.cg.shared.global [%0], [%1], 16, %2;\n" ::"r"(dst), "l"(src),
               "r"(src_bytes));
}

__device__ __forceinline__ void cp_async_commit() { asm volatile("cp.async.commit_group;\n" ::); }

template <int N>
__device__ __forceinline__ void cp_async_wait() {
  asm volatile("cp.async.wait_group %0;\n" ::"n"(N));
}

__device__ __forceinline__ void ldmatrix_x4(uint32_t (&r)[4], uint32_t addr) {
  asm volatile("ldmatrix.sync.aligned.m8n8.x4.shared.b16 {%0,%1,%2,%3}, [%4];\n"
               : "=r"(r[0]), "=r"(r[1]), "=r"(r[2]), "=r"(r[3])
               : "r"(addr));
}

__device__ __forceinline__ uint32_t rotl32(uint32_t x, int n) { return __funnelshift_l(x, x, n); }

__device__ __forceinline__ uint32_t ld_volatile_u32(const uint32_t* p) {
  uint32_t v;
  asm volatile("ld.volatile.global.u32 %0, [%1];" : "=r"(v) : "l"(p));
  return v;
}

/// XOR of the 128 accumulators as a balanced tree (short dependency chains).
template <class Mma>
__device__ __forceinline__ uint32_t fold_tile(const typename Mma::Acc (&acc)[kFragM][kFragN][4]) {
  uint32_t lvl[32];
#pragma unroll
  for (int i = 0; i < 32; ++i) {
    const int fm = i / 8, fn = i % 8;
    lvl[i] = Mma::bits(acc[fm][fn][0]) ^ Mma::bits(acc[fm][fn][1]) ^ Mma::bits(acc[fm][fn][2]) ^
             Mma::bits(acc[fm][fn][3]);
  }
#pragma unroll
  for (int w = 16; w >= 1; w /= 2) {
#pragma unroll
    for (int i = 0; i < w; ++i) lvl[i] ^= lvl[i + w];
  }
  return lvl[0];
}

template <class Mma, bool kDump>
__global__ void __launch_bounds__(kThreads, 1)
    fused_kernel(const GemmArgs args, uint32_t tile_begin, uint32_t tiles_m, uint32_t tiles_n) {
  static_assert(Mma::kImplemented, "this MMA policy has no transcript semantics yet");
  extern __shared__ __align__(128) uint8_t smem[];

  const uint32_t tid = threadIdx.x;
  const uint32_t lane = tid & 31u;
  const uint32_t warp = tid >> 5;
  const uint32_t wm = warp / kWarpsN;
  const uint32_t wn = warp % kWarpsN;

  // ---- CTA tile, grouped raster: kGroupM tile rows are swept column by column. ------------------
  const uint32_t lin = tile_begin + blockIdx.x;
  const uint32_t per_group = kGroupM * tiles_n;
  const uint32_t group = lin / per_group;
  const uint32_t first_m = group * kGroupM;
  const uint32_t group_rows = min(tiles_m - first_m, static_cast<uint32_t>(kGroupM));
  const uint32_t in_group = lin - group * per_group;
  const uint32_t m0 = (first_m + in_group % group_rows) * kBM;
  const uint32_t n0 = (in_group / group_rows) * kBN;
  const uint32_t k = args.k;
  const uint32_t smem_base = static_cast<uint32_t>(__cvta_generic_to_shared(smem));
  // Abort word: read now, tested once k-tile 0 has landed, so its latency hides behind the first
  // cp.async wait; a CTA that sees it set leaves before its first MMA (the host then voids the
  // attempt). This is what bounds the cancel latency to about one CTA tile instead of a chunk.
  const uint32_t abort_word = (tid == 0 && args.abort_flag != nullptr) ? ld_volatile_u32(args.abort_flag) : 0u;

  // ---- global -> shared: thread copies chunk (tid & 3) of rows (tid >> 2) + 64 j. -----------------
  // m and n are multiples of 64, so a 64-row pass is either wholly inside the matrix or wholly
  // outside it (edge CTA tiles when m % 128 or n % 256 is 64); outside passes are zero-filled.
  const uint32_t cp_row = tid >> 2;
  const uint32_t cp_dst = swizzle(cp_row, tid & 3u);  // + 4096 per pass: same swizzle phase
  const int8_t* const a_src = args.a + static_cast<size_t>(m0 + cp_row) * k + (tid & 3u) * 16u;
  const int8_t* const b_src = args.bt + static_cast<size_t>(n0 + cp_row) * k + (tid & 3u) * 16u;
  const size_t pass_stride = static_cast<size_t>(kRowsPerCopy) * k;
  const uint32_t a_rows = args.m - m0;  // >= 64
  const uint32_t b_rows = args.n - n0;  // >= 64

  auto load_stage = [&](uint32_t stage, uint32_t kt) {
    const uint32_t sa = smem_base + stage * kStageBytes;
    const uint32_t sb = sa + kStageA;
    const size_t ko = static_cast<size_t>(kt) * kBK;
#pragma unroll
    for (int j = 0; j < kBM / kRowsPerCopy; ++j) {
      const bool ok = static_cast<uint32_t>(j * kRowsPerCopy) < a_rows;
      cp_async_16(sa + cp_dst + j * (kRowsPerCopy * kBK), a_src + (ok ? j * pass_stride : 0) + ko,
                  ok ? 16u : 0u);
    }
#pragma unroll
    for (int j = 0; j < kBN / kRowsPerCopy; ++j) {
      const bool ok = static_cast<uint32_t>(j * kRowsPerCopy) < b_rows;
      cp_async_16(sb + cp_dst + j * (kRowsPerCopy * kBK), b_src + (ok ? j * pass_stride : 0) + ko,
                  ok ? 16u : 0u);
    }
  };

  // ---- ldmatrix addressing. -------------------------------------------------------------------
  // A (row major, m16 x k32 per fragment): lanes 0-7 / 8-15 / 16-23 / 24-31 address rows 0-7 /
  // 8-15 / 0-7 / 8-15 at k-bytes 0-15 / 0-15 / 16-31 / 16-31 -> registers a0 a1 a2 a3.
  // B'ᵀ (n x k, i.e. B "col"): one x4 covers two n8 fragments; lanes 0-7 / 8-15 / 16-23 / 24-31
  // address n-rows 0-7 / 0-7 / 8-15 / 8-15 at k-bytes 0-15 / 16-31 / 0-15 / 16-31
  // -> b0, b1 of fragment 2p and b0, b1 of fragment 2p + 1.
  // Row offsets of +16 (next fragment) keep the swizzle phase, hence the +1024 immediates.
  const uint32_t a_row = wm * kWM + (lane & 7u) + ((lane >> 3) & 1u) * 8u;
  const uint32_t a_off[2] = {swizzle(a_row, 0u + (lane >> 4)), swizzle(a_row, 2u + (lane >> 4))};
  const uint32_t b_row = wn * kWN + (lane & 7u) + (lane >> 4) * 8u;
  const uint32_t b_off[2] = {swizzle(b_row, 0u + ((lane >> 3) & 1u)),
                             swizzle(b_row, 2u + ((lane >> 3) & 1u))};

  typename Mma::Acc acc[kFragM][kFragN][4];
#pragma unroll
  for (int fm = 0; fm < kFragM; ++fm)
#pragma unroll
    for (int fn = 0; fn < kFragN; ++fn)
#pragma unroll
      for (int i = 0; i < 4; ++i) acc[fm][fn][i] = 0;

  auto compute_stage = [&](uint32_t stage) {
    const uint32_t sa = smem_base + stage * kStageBytes;
    const uint32_t sb = sa + kStageA;
#pragma unroll
    for (int kk = 0; kk < kBK / 32; ++kk) {
      uint32_t af[kFragM][4];
      uint32_t bf[kFragN][2];
#pragma unroll
      for (int fm = 0; fm < kFragM; ++fm) ldmatrix_x4(af[fm], sa + a_off[kk] + fm * (16 * kBK));
#pragma unroll
      for (int p = 0; p < kFragN / 2; ++p) {
        uint32_t r[4];
        ldmatrix_x4(r, sb + b_off[kk] + p * (16 * kBK));
        bf[2 * p][0] = r[0];
        bf[2 * p][1] = r[1];
        bf[2 * p + 1][0] = r[2];
        bf[2 * p + 1][1] = r[3];
      }
#pragma unroll
      for (int fm = 0; fm < kFragM; ++fm)
#pragma unroll
        for (int fn = 0; fn < kFragN; ++fn) Mma::mma(acc[fm][fn], af[fm], bf[fn]);
    }
  };

  // ---- mainloop: only full slices enter the transcript, so only they are computed. -------------
  // Transcript queue invariant: at the start of slice s, q[i] holds word (s + i) % 16.
  uint32_t q[kTranscriptWords];
#pragma unroll
  for (int i = 0; i < kTranscriptWords; ++i) q[i] = 0u;
  const uint32_t slices = k / kSlice;
  const uint32_t kt_total = slices * kTilesPerSlice;
#pragma unroll
  for (int s = 0; s < kStages - 1; ++s) {
    load_stage(s, s);
    cp_async_commit();
  }
  cp_async_wait<kStages - 2>();
  if (__syncthreads_or(abort_word != 0u)) {
    cp_async_wait<0>();
    return;
  }
  uint32_t st_compute = 0;
  uint32_t st_load = kStages - 1;
  uint32_t kt = 0;
  for (uint32_t s = 0; s < slices; ++s) {
#pragma unroll
    for (int h = 0; h < kTilesPerSlice; ++h) {
      cp_async_wait<kStages - 2>();  // k-tile kt has landed (for this thread's copies)
      __syncthreads();               // ... for everyone's, and stage st_load is free again
      if (kt + (kStages - 1) < kt_total) load_stage(st_load, kt + (kStages - 1));
      cp_async_commit();
      compute_stage(st_compute);
      st_compute = (st_compute == kStages - 1) ? 0 : st_compute + 1;
      st_load = (st_load == kStages - 1) ? 0 : st_load + 1;
      ++kt;
    }
    const uint32_t word = rotl32(q[0], kRotl) ^ fold_tile<Mma>(acc);
#pragma unroll
    for (int i = 0; i < kTranscriptWords - 1; ++i) q[i] = q[i + 1];
    q[kTranscriptWords - 1] = word;
  }
  cp_async_wait<0>();

  // ---- epilogue: digest, difficulty compare, hit ring, dump. ---------------------------------------
  // After `slices` updates q[i] holds word (slices + i) % 16: rotate right by slices % 16 (four
  // conditional power-of-two rotations with a uniform predicate) to get t[0..16] in order.
  const uint32_t rot = slices % kTranscriptWords;
#pragma unroll
  for (int b = 1; b < kTranscriptWords; b <<= 1) {
    uint32_t t[kTranscriptWords];
#pragma unroll
    for (int i = 0; i < kTranscriptWords; ++i) t[i] = q[(i - b) & (kTranscriptWords - 1)];
#pragma unroll
    for (int i = 0; i < kTranscriptWords; ++i) q[i] = (rot & b) ? t[i] : q[i];
  }
  uint32_t key[8], bound[8], digest[8];
#pragma unroll
  for (int i = 0; i < 8; ++i) {
    key[i] = args.key[i];
    bound[i] = args.bound[i];
  }
  blake3::keyed_hash64(key, q, digest);

  const bool warp_in_range = (m0 + wm * kWM < args.m) && (n0 + wn * kWN < args.n);
  if (!warp_in_range) return;
  const uint32_t t_rows = m0 + wm * kWM + (lane >> 2);
  const uint32_t t_cols = n0 + wn * kWN + 2u * (lane & 3u);

  if (kDump) {
    // Reference order: t_rows ascending outer, t_cols ascending inner. Valid bases are
    // t_rows = 64a + g (g < 8) and t_cols = 64b + 2c (c < 4).
    const uint64_t row_idx = (t_rows >> 6) * 8u + (t_rows & 63u);
    const uint64_t col_idx = (t_cols >> 6) * 4u + ((t_cols & 63u) >> 1);
    const uint64_t tile = row_idx * (args.n / 16u) + col_idx;
    uint2* rec = reinterpret_cast<uint2*>(args.dump + tile * kDumpRecordBytes);
    rec[0] = make_uint2(t_rows, t_cols);
#pragma unroll
    for (int i = 0; i < kTranscriptWords / 2; ++i) rec[1 + i] = make_uint2(q[2 * i], q[2 * i + 1]);
#pragma unroll
    for (int i = 0; i < 4; ++i) rec[9 + i] = make_uint2(digest[2 * i], digest[2 * i + 1]);
  }

  if (u256_le(digest, bound)) {
    const uint32_t slot = atomicAdd(args.hit_count, 1u);
    if (slot < args.hit_capacity) {
      DeviceHit* h = args.hits + slot;
      h->t_rows = t_rows;
      h->t_cols = t_cols;
#pragma unroll
      for (int i = 0; i < 8; ++i) h->digest[i] = digest[i];
    }
  }
}

}  // namespace gemm_v0
}  // namespace spm
