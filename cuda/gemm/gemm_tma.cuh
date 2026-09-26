// Fused noised int8 GEMM + per-slice transcript + BLAKE3 digest + bound compare (strategy C):
// a persistent, warp-specialized kernel fed by TMA through an mbarrier ring.
//
// Geometry
//   * CTA tile 128 (m) x 256 (n) x 64 (k). 12 warps: warps 0-7 are the MMA warps (a 2 x 4 grid of
//     64 x 64 warp tiles); warps 8-11 form the producer warpgroup, of which one thread issues the TMA
//     loads. setmaxnreg moves registers from the producer warpgroup (40) to the MMA warps (232):
//     every SM sub-partition then holds 2 MMA warps and 1 producer warp within its 16K registers.
//   * STAGES-deep ring of {A' 128x64, B'ᵀ 256x64} stages (24 KiB each) loaded with
//     cp.async.bulk.tensor (SWIZZLE_64B). A `full` mbarrier per stage completes on 1 arrival plus the
//     transaction bytes; an `empty` mbarrier per stage completes when the 8 MMA warps released it,
//     and the producer refills the stage right away.
//   * Persistent grid (one CTA per SM): the producer takes CTA tiles from a global atomic counter in
//     an L2-friendly band raster (bands of `band` CTA rows, column-major inside a band) and passes
//     the tile id to the MMA warps in the first stage of the tile; -1 is the stop command. The abort
//     flag (host-mapped) is read before every tile, so a cancel takes effect within one tile.
//   * The kernel is bound by load latency (3 stages instead of 4 lose ~10%), not by L2 bandwidth.
//     The first CTA row of each band therefore prefetches its B'ᵀ k-tiles into L2 a few k-tiles
//     ahead, since it is the one that meets the DRAM misses of its column.
//   * mma.sync.m16n8k32 s8·s8 -> s32 from ldmatrix.x4 fragments. In a 64 x 64 warp tile made of
//     4 x 8 m16n8 fragments, lane l holds rows l/4 + {0, 8, ..., 56} and columns
//     2(l%4) + {0, 1, 8, 9, ..., 56, 57}: exactly one hash tile, so the per-slice XOR fold is
//     thread local and the 16-word transcript lives in registers.
//
// Transcript: after every r = 128 slice (2 stages) the thread folds its 128 cumulative
// accumulators into one word and updates slot s mod 16 (rotl 13, then XOR). The slot is kept at
// t[0] by rotating the register array as a queue after each slice, so every index is static; the
// epilogue undoes the leftover rotation (k/128 mod 16) before hashing.
#pragma once
#include <cuda.h>
#include <cuda_runtime.h>
#include <stdint.h>

#include "../common/blake3.cuh"
#include "../common/ptx.cuh"
#include "../common/u256.cuh"

namespace spm {
namespace gemm {

// ---- MMA policies -------------------------------------------------------------------------

/// PearlHash V3: mma.sync.m16n8k32.row.col.s32.s8.s8.s32 (SASS IMMA.16832.S8.S8).
struct MmaS8 {
  using Acc = int32_t;
  static __device__ __forceinline__ void mma(Acc (&d)[4], const uint32_t (&a)[4], uint32_t b0,
                                             uint32_t b1) {
    asm volatile(
        "mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0, %1, %2, %3}, {%4, %5, %6, %7}, "
        "{%8, %9}, {%0, %1, %2, %3};"
        : "+r"(d[0]), "+r"(d[1]), "+r"(d[2]), "+r"(d[3])
        : "r"(a[0]), "r"(a[1]), "r"(a[2]), "r"(a[3]), "r"(b0), "r"(b1));
  }
  /// The bits the transcript folds (two's complement, like the reference `x as u32`).
  static __device__ __forceinline__ uint32_t bits(Acc x) { return static_cast<uint32_t>(x); }
};

/// Certificate-v4 fork (M12): same ldmatrix fragments, e4m3 inputs, one f32 chain per output in
/// ascending k (SASS QMMA.16832). The v4 fold is not specified yet, so no kernel is instantiated
/// with it; it documents the shape of the policy the mainloop is templated on.
struct MmaE4M3 {
  using Acc = float;
  static __device__ __forceinline__ void mma(Acc (&d)[4], const uint32_t (&a)[4], uint32_t b0,
                                             uint32_t b1) {
    asm volatile(
        "mma.sync.aligned.m16n8k32.row.col.kind::f8f6f4.f32.e4m3.e4m3.f32 {%0, %1, %2, %3}, "
        "{%4, %5, %6, %7}, {%8, %9}, {%0, %1, %2, %3};"
        : "+f"(d[0]), "+f"(d[1]), "+f"(d[2]), "+f"(d[3])
        : "r"(a[0]), "r"(a[1]), "r"(a[2]), "r"(a[3]), "r"(b0), "r"(b1));
  }
  static __device__ __forceinline__ uint32_t bits(Acc x) { return __float_as_uint(x); }
};

// ---- geometry -----------------------------------------------------------------------------

constexpr uint32_t BM = 128;
constexpr uint32_t BN = 256;
constexpr uint32_t BK = 64;
constexpr uint32_t SLICE_K = 128;  // noise rank r: one transcript update per 128 k
constexpr uint32_t KTILES_PER_SLICE = SLICE_K / BK;
constexpr int MMA_WARPS = 8;
constexpr int PRODUCER_WARPS = 4;  // a whole warpgroup: setmaxnreg acts per warpgroup
constexpr int THREADS = (MMA_WARPS + PRODUCER_WARPS) * 32;
constexpr uint32_t MMA_REGS = 232;
constexpr uint32_t PRODUCER_REGS = 40;
constexpr uint32_t STAGES = 4;
// L2 prefetch distance (k-tiles) of the band leader's B'ᵀ loads. Measured on the GB10 at
// 16384^2 x 4096: 4 -> +1.6 points of peak, 8 -> +0.9; prefetching from every CTA (both operands)
// costs 5-12 points, because the column's other 15 CTAs already hit L2.
constexpr uint32_t B_PREFETCH_KTILES = 4;
/// Shared-memory layout the TMA descriptors of both operands use (boxes of BK = 64 bytes per row);
/// the ldmatrix addressing below assumes it.
constexpr CUtensorMapSwizzle OPERAND_SWIZZLE = CU_TENSOR_MAP_SWIZZLE_64B;
constexpr uint32_t A_STAGE_BYTES = BM * BK;
constexpr uint32_t B_STAGE_BYTES = BN * BK;
constexpr uint32_t STAGE_BYTES = A_STAGE_BYTES + B_STAGE_BYTES;
// Stage buffers are 1024-byte aligned inside the dynamic window (swizzle atoms), then the barriers
// (2 x STAGES x 8 bytes) and the per-stage command words.
constexpr uint32_t SMEM_BYTES = STAGES * STAGE_BYTES + 1024 + 2 * STAGES * 8 + STAGES * 4;
constexpr uint32_t TRANSCRIPT_ROTL = 13;
constexpr uint32_t STATUS_ABORTED = 1u;
/// 32-bit words in a dump record: t_rows, t_cols, transcript[16], digest[8] (104 bytes).
constexpr uint32_t DUMP_WORDS = 26;
constexpr int32_t CMD_STOP = -1;

static_assert(STAGE_BYTES % 1024 == 0, "stage buffers must keep the swizzle alignment");
static_assert(SMEM_BYTES <= 101376, "GB10 allows at most 99 KiB of shared memory per block");
// Per SM sub-partition: 2 MMA warps + 1 producer warp must fit in 16384 registers.
static_assert((2 * MMA_REGS + PRODUCER_REGS) * 32 <= 16384, "register split exceeds a sub-partition");

struct HitRecord {
  uint32_t t_rows;
  uint32_t t_cols;
  uint32_t digest[8];  // LE words of the 32-byte digest
};

struct Params {
  uint32_t m, n;
  uint32_t k_slices;          // floor(k / 128): the kernel reads k columns [0, 128 * k_slices)
  uint32_t tiles_m, tiles_n;  // CTA tiles: ceil(m / 128) x ceil(n / 256)
  uint32_t band;              // raster band height in CTA rows
  uint32_t tile_begin, tile_end;  // this launch's range of the raster order
  uint32_t* tile_counter;     // zeroed before every launch
  uint32_t* status;           // STATUS_* bits, zeroed before every launch
  const volatile uint32_t* abort_flag;  // host-mapped; nonzero stops at the next tile
  uint32_t key[8];            // a_noise_seed (jackpot hash key)
  uint32_t bound[8];          // difficulty bound, LE words
  uint32_t* dump;             // DUMP_WORDS per hash tile in reference order, or nullptr
  uint32_t dump_row_stride;   // hash tiles per tile row = n / 16
  HitRecord* hits;
  uint32_t* hit_count;
  uint32_t hit_capacity;
};

/// Raster order: bands of `band` CTA rows; inside a band the column index is the outer loop, so
/// the CTAs running at the same time share both their B'ᵀ column blocks and the band's A' rows.
__host__ __device__ __forceinline__ void tile_coords(const Params& p, uint32_t t, uint32_t& tm,
                                                     uint32_t& tn) {
  const uint32_t per_band = p.band * p.tiles_n;
  const uint32_t b = t / per_band;
  const uint32_t r = t - b * per_band;
  const uint32_t first = b * p.band;
  const uint32_t rows = min(p.band, p.tiles_m - first);
  tn = r / rows;
  tm = first + (r - tn * rows);
}

// ---- lane geometry of the MMA warps ---------------------------------------------------------
// Host + device, so cuda/tests/layout_check.cu proves the fragment mapping on exactly these
// expressions (ldmatrix addressing through the TMA swizzle, the MMA fragment layouts of MmaS8 /
// MmaE4M3, the hash tile each lane reports and its dump index).

/// Fragments of a 64 x 64 warp tile: 4 m16 blocks x 8 n8 blocks of m16n8k32 accumulators.
constexpr uint32_t WARP_TILE = 64;
constexpr uint32_t FRAGS_M = 4;
constexpr uint32_t FRAGS_N = 8;

/// Warp tile of MMA warp `warp` inside the 128 x 256 CTA tile: row block 0..1, column block 0..3.
__host__ __device__ __forceinline__ constexpr uint32_t warp_m(uint32_t warp) { return warp >> 2; }
__host__ __device__ __forceinline__ constexpr uint32_t warp_n(uint32_t warp) { return warp & 3u; }

/// ldmatrix.x4 row addresses of one lane, as byte offsets inside a stage, for the two k32 steps
/// of the stage: a[ks] for the A' fragments (m16 block i adds i * 16 * BK; matrices {rows 0-7,
/// 8-15} x {k 0-15, 16-31}) and b[ks] for the B'ᵀ fragments (n16 pair jp adds jp * 16 * BK;
/// matrices {n 0-7 k 0-15, n 0-7 k 16-31, n 8-15 k 0-15, n 8-15 k 16-31}).
struct LdsmOffsets {
  uint32_t a[2];
  uint32_t b[2];
};

/// Offsets of lane `lane` in the warp tile (wm, wn). SWIZZLE_64B: 16-byte chunk c of stage row r
/// sits at chunk c ^ ((r >> 1) & 3); bits 1..2 of every row a lane addresses come from (lane & 7).
__host__ __device__ __forceinline__ LdsmOffsets ldsm_offsets(uint32_t wm, uint32_t wn,
                                                             uint32_t lane) {
  const uint32_t swz = (lane & 7u) >> 1;
  const uint32_t a_row = wm * 64u + (lane & 7u) + ((lane >> 3) & 1u) * 8u;
  const uint32_t b_row = wn * 64u + (lane & 7u) + (lane >> 4) * 8u;
  LdsmOffsets o;
  o.a[0] = a_row * BK + ((((lane >> 4) + 0u) ^ swz) << 4);
  o.a[1] = a_row * BK + ((((lane >> 4) + 2u) ^ swz) << 4);
  o.b[0] = A_STAGE_BYTES + b_row * BK + (((((lane >> 3) & 1u) + 0u) ^ swz) << 4);
  o.b[1] = A_STAGE_BYTES + b_row * BK + (((((lane >> 3) & 1u) + 2u) ^ swz) << 4);
  return o;
}

/// The hash tile whose 128 accumulators lane `lane` holds, relative to its warp tile: rows
/// lane_row + {0, 8, ..., 56} x cols lane_col + {0, 1, 8, 9, ..., 56, 57}.
__host__ __device__ __forceinline__ constexpr uint32_t lane_row(uint32_t lane) { return lane >> 2; }
__host__ __device__ __forceinline__ constexpr uint32_t lane_col(uint32_t lane) {
  return 2u * (lane & 3u);
}

/// Reference-order index (t_rows ascending, then t_cols) of the hash tile of `lane` in the warp tile
/// at (row0, col0); `row_stride` = hash tiles per tile row = n / 16.
__host__ __device__ __forceinline__ constexpr uint64_t dump_index(uint32_t row0, uint32_t col0,
                                                                  uint32_t lane,
                                                                  uint32_t row_stride) {
  return (uint64_t)((row0 >> 6) * 8u + lane_row(lane)) * row_stride + (col0 >> 6) * 4u +
         (lane & 3u);
}

// ---- producer ------------------------------------------------------------------------------

/// The producer thread: fills every free stage, fetching a new CTA tile at each tile boundary,
/// until the tile range (or the abort flag) runs out; then posts the stop command.
__device__ __forceinline__ void produce(const CUtensorMap* tmap_a, const CUtensorMap* tmap_b,
                                        const Params& p, uint32_t smem, uint32_t full0,
                                        uint32_t empty0, volatile int32_t* cmd) {
  const uint32_t ktiles = p.k_slices * KTILES_PER_SLICE;
  uint32_t stage = 0, phase = 0;
  for (;;) {
    int32_t tile = CMD_STOP;
    if (*p.abort_flag == 0u) {
      const uint32_t t = p.tile_begin + atomicAdd(p.tile_counter, 1u);
      if (t < p.tile_end) tile = static_cast<int32_t>(t);
    } else {
      atomicOr(p.status, STATUS_ABORTED);
    }
    ptx::mbar_wait(empty0 + 8 * stage, phase ^ 1u);
    cmd[stage] = tile;
    if (tile == CMD_STOP) {
      ptx::mbar_arrive(full0 + 8 * stage);  // completes the phase with the stop command only
      return;
    }
    uint32_t tm, tn;
    tile_coords(p, static_cast<uint32_t>(tile), tm, tn);
    const int32_t row_a = static_cast<int32_t>(tm * BM);
    const int32_t row_b = static_cast<int32_t>(tn * BN);
    // The first CTA row of a band is the first to touch each B'ᵀ k-tile of its column (usually a
    // DRAM miss); it pulls its own B k-tile B_PREFETCH_KTILES ahead into L2, which also serves the
    // other CTAs of the column right behind it.
    const bool leader = tm % p.band == 0;
    for (uint32_t kt = 0; kt < ktiles; ++kt) {
      if (kt != 0) ptx::mbar_wait(empty0 + 8 * stage, phase ^ 1u);
      const uint32_t full = full0 + 8 * stage;
      const uint32_t dst = smem + stage * STAGE_BYTES;
      const int32_t k0 = static_cast<int32_t>(kt * BK);
      ptx::mbar_arrive_expect_tx(full, STAGE_BYTES);
      ptx::tma_load_2d(dst, tmap_a, k0, row_a, full);
      ptx::tma_load_2d(dst + A_STAGE_BYTES, tmap_b, k0, row_b, full);
      if (leader && kt + B_PREFETCH_KTILES < ktiles)
        ptx::tma_prefetch_l2_2d(tmap_b, static_cast<int32_t>((kt + B_PREFETCH_KTILES) * BK), row_b);
      if (++stage == STAGES) {
        stage = 0;
        phase ^= 1u;
      }
    }
  }
}

// ---- MMA warps -----------------------------------------------------------------------------

/// t <- t rotated right by kBy positions when `apply` (static indices only, stays in registers).
template <uint32_t kBy>
__host__ __device__ __forceinline__ void rotate_right_if(uint32_t (&t)[16], bool apply) {
  uint32_t u[16];
#pragma unroll
  for (uint32_t j = 0; j < 16; ++j) u[j] = t[(j + 16u - kBy) & 15u];
#pragma unroll
  for (uint32_t j = 0; j < 16; ++j) t[j] = apply ? u[j] : t[j];
}

template <class Mma>
__device__ __forceinline__ void consume(const Params& p, uint32_t smem, uint32_t full0,
                                        uint32_t empty0, const volatile int32_t* cmd,
                                        uint32_t warp, uint32_t lane) {
  using Acc = typename Mma::Acc;
  const uint32_t wm = warp_m(warp);  // 0..1
  const uint32_t wn = warp_n(warp);  // 0..3
  // ldmatrix.x4 lane addresses for the two k32 steps of a stage.
  const LdsmOffsets off = ldsm_offsets(wm, wn, lane);
  const uint32_t a_off0 = off.a[0], a_off1 = off.a[1];
  const uint32_t b_off0 = off.b[0], b_off1 = off.b[1];

  uint32_t stage = 0, phase = 0;
  for (;;) {
    ptx::mbar_wait(full0 + 8 * stage, phase);
    const int32_t tile = cmd[stage];
    if (tile == CMD_STOP) return;
    uint32_t tm, tn;
    tile_coords(p, static_cast<uint32_t>(tile), tm, tn);
    const uint32_t row0 = tm * BM + wm * WARP_TILE;
    const uint32_t col0 = tn * BN + wn * WARP_TILE;
    const bool active = row0 < p.m && col0 < p.n;

    Acc acc[FRAGS_M][FRAGS_N][4];
#pragma unroll
    for (int i = 0; i < 4; ++i)
#pragma unroll
      for (int j = 0; j < 8; ++j)
#pragma unroll
        for (int v = 0; v < 4; ++v) acc[i][j][v] = Acc(0);
    uint32_t t[16];
#pragma unroll
    for (int i = 0; i < 16; ++i) t[i] = 0u;

    for (uint32_t s = 0; s < p.k_slices; ++s) {
#pragma unroll
      for (uint32_t h = 0; h < KTILES_PER_SLICE; ++h) {
        if (s != 0 || h != 0) ptx::mbar_wait(full0 + 8 * stage, phase);
        const uint32_t sb = smem + stage * STAGE_BYTES;
        if (active) {
#pragma unroll
          for (int ks = 0; ks < 2; ++ks) {
            uint32_t a[4][4];
            uint32_t b[8][2];
            const uint32_t ao = sb + (ks ? a_off1 : a_off0);
            const uint32_t bo = sb + (ks ? b_off1 : b_off0);
#pragma unroll
            for (int i = 0; i < 4; ++i)
              ptx::ldmatrix_x4(a[i][0], a[i][1], a[i][2], a[i][3], ao + i * 16 * BK);
#pragma unroll
            for (int jp = 0; jp < 4; ++jp)
              ptx::ldmatrix_x4(b[2 * jp][0], b[2 * jp][1], b[2 * jp + 1][0], b[2 * jp + 1][1],
                               bo + jp * 16 * BK);
#pragma unroll
            for (int i = 0; i < 4; ++i)
#pragma unroll
              for (int j = 0; j < 8; ++j) Mma::mma(acc[i][j], a[i], b[j][0], b[j][1]);
          }
        }
        // The MMAs above already consumed every lane's ldmatrix results: release the stage.
        __syncwarp();
        if (lane == 0) ptx::mbar_arrive(empty0 + 8 * stage);
        if (++stage == STAGES) {
          stage = 0;
          phase ^= 1u;
        }
      }
      // Fold the cumulative accumulators of slice s into slot s mod 16 (kept at t[0]).
      uint32_t f[8];
#pragma unroll
      for (int j = 0; j < 8; ++j) {
        uint32_t x = ptx::xor3(Mma::bits(acc[0][j][0]), Mma::bits(acc[0][j][1]), Mma::bits(acc[0][j][2]));
        x = ptx::xor3(x, Mma::bits(acc[0][j][3]), Mma::bits(acc[1][j][0]));
        x = ptx::xor3(x, Mma::bits(acc[1][j][1]), Mma::bits(acc[1][j][2]));
        x = ptx::xor3(x, Mma::bits(acc[1][j][3]), Mma::bits(acc[2][j][0]));
        x = ptx::xor3(x, Mma::bits(acc[2][j][1]), Mma::bits(acc[2][j][2]));
        x = ptx::xor3(x, Mma::bits(acc[2][j][3]), Mma::bits(acc[3][j][0]));
        x = ptx::xor3(x, Mma::bits(acc[3][j][1]), Mma::bits(acc[3][j][2]));
        f[j] = x ^ Mma::bits(acc[3][j][3]);
      }
      const uint32_t fold =
          ptx::xor3(ptx::xor3(f[0], f[1], f[2]), ptx::xor3(f[3], f[4], f[5]), f[6] ^ f[7]);
      const uint32_t head = ptx::rotl32(t[0], TRANSCRIPT_ROTL) ^ fold;
#pragma unroll
      for (int i = 0; i < 15; ++i) t[i] = t[i + 1];
      t[15] = head;
    }
    if (!active) continue;

    // After S slices t[i] holds slot (i + S) mod 16: rotate right by S mod 16.
    const uint32_t rot = p.k_slices & 15u;
    rotate_right_if<1>(t, rot & 1u);
    rotate_right_if<2>(t, rot & 2u);
    rotate_right_if<4>(t, rot & 4u);
    rotate_right_if<8>(t, rot & 8u);

    uint32_t key[8], bound[8], digest[8];
#pragma unroll
    for (int i = 0; i < 8; ++i) {
      key[i] = p.key[i];
      bound[i] = p.bound[i];
    }
    b3::keyed_hash_one_block(key, t, digest);
    const uint32_t t_rows = row0 + lane_row(lane);
    const uint32_t t_cols = col0 + lane_col(lane);
    if (p.dump != nullptr) {
      const uint64_t idx = dump_index(row0, col0, lane, p.dump_row_stride);
      uint2* rec = reinterpret_cast<uint2*>(p.dump + idx * DUMP_WORDS);
      rec[0] = make_uint2(t_rows, t_cols);
#pragma unroll
      for (int i = 0; i < 8; ++i) rec[1 + i] = make_uint2(t[2 * i], t[2 * i + 1]);
#pragma unroll
      for (int i = 0; i < 4; ++i) rec[9 + i] = make_uint2(digest[2 * i], digest[2 * i + 1]);
    }
    if (u256_le_leq(digest, bound)) {
      const uint32_t slot = atomicAdd(p.hit_count, 1u);
      if (slot < p.hit_capacity) {
        HitRecord& hit = p.hits[slot];
        hit.t_rows = t_rows;
        hit.t_cols = t_cols;
#pragma unroll
        for (int i = 0; i < 8; ++i) hit.digest[i] = digest[i];
      }
    }
  }
}

// ---- kernel -------------------------------------------------------------------------------

template <class Mma>
__global__ void __launch_bounds__(THREADS, 1)
    gemm_hash_kernel(const __grid_constant__ CUtensorMap tmap_a,
                     const __grid_constant__ CUtensorMap tmap_b, const __grid_constant__ Params p) {
  extern __shared__ __align__(1024) uint8_t smem_raw[];
  const uint32_t raw = ptx::smem_addr(smem_raw);
  const uint32_t smem = (raw + 1023u) & ~1023u;
  uint8_t* aligned = smem_raw + (smem - raw);
  const uint32_t full0 = smem + STAGES * STAGE_BYTES;
  const uint32_t empty0 = full0 + 8 * STAGES;
  volatile int32_t* cmd =
      reinterpret_cast<volatile int32_t*>(aligned + STAGES * STAGE_BYTES + 16 * STAGES);
  const uint32_t warp = threadIdx.x >> 5;
  const uint32_t lane = threadIdx.x & 31u;

  if (threadIdx.x == 0) {
    for (uint32_t s = 0; s < STAGES; ++s) {
      ptx::mbar_init(full0 + 8 * s, 1);
      ptx::mbar_init(empty0 + 8 * s, MMA_WARPS);
    }
    ptx::mbar_fence_init();
  }
  __syncthreads();

  if (warp >= static_cast<uint32_t>(MMA_WARPS)) {
    ptx::setmaxnreg_dec<PRODUCER_REGS>();
    if (warp == static_cast<uint32_t>(MMA_WARPS) && lane == 0) {
      ptx::tma_prefetch_descriptor(&tmap_a);
      ptx::tma_prefetch_descriptor(&tmap_b);
      produce(&tmap_a, &tmap_b, p, smem, full0, empty0, cmd);
    }
    return;
  }
  ptx::setmaxnreg_inc<MMA_REGS>();
  consume<Mma>(p, smem, full0, empty0, cmd, warp, lane);
}

/// Sets the dynamic shared-memory limit of the int8 kernel (idempotent; also loads the module).
cudaError_t configure_gemm_hash_s8();

/// Launches the int8 instantiation: `grid` persistent CTAs over tiles [tile_begin, tile_end).
cudaError_t launch_gemm_hash_s8(const CUtensorMap& tmap_a, const CUtensorMap& tmap_b,
                                const Params& p, uint32_t grid, cudaStream_t stream);

/// Register / shared-memory / spill figures of the int8 kernel (cudaFuncGetAttributes).
cudaError_t gemm_hash_s8_attributes(cudaFuncAttributes* out);

}  // namespace gemm
}  // namespace spm
