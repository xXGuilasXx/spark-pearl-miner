// CuTe building blocks of the fused GEMM + transcript kernel (strategy B).
//
// CTA tile 128 x 128 x 64, 4 warps of 64 x 64 each, a cp.async multistage pipeline, LDSM
// shared-to-register copies and mma.sync.m16n8k32 atoms. The TiledMMA is permuted so every warp
// owns one contiguous, 64-aligned 64 x 64 block of C; with the m16n8 accumulator layout a lane
// then holds rows {lane/4 + 8i} x cols {2(lane%4) + {0,1} + 8j}: exactly one 8 x 16 hash tile
// (rows t_rows + {0, 8, .., 56}, cols t_cols + {0, 1, 8, 9, .., 56, 57}). The transcript fold is
// therefore a thread-local XOR over the thread's 128 accumulators. cuda/tests/layout_check.cu
// proves this mapping for every thread before the kernel relies on it.
#pragma once

#include <cstring>

#include <cute/tensor.hpp>
#include <cute/atom/copy_atom.hpp>
#include <cute/atom/mma_atom.hpp>

namespace spm {
namespace gemm {

using namespace cute;

// MMA policies. The mainloop, the fragment layouts and the transcript are shared; a policy picks
// the tensor-core operation, the operand element type and how an accumulator is read as the 32
// bits the transcript folds.
struct Int8Policy {
  using ElementA = int8_t;
  using ElementB = int8_t;
  using ElementAcc = int32_t;
  // mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 -> SASS IMMA.16832.S8.S8
  using MmaOp = SM80_16x8x32_S32S8S8S32_TN;
  CUTE_HOST_DEVICE static uint32_t acc_bits(int32_t x) { return static_cast<uint32_t>(x); }
};

#if defined(SPM_ENABLE_FP8_POLICY)
// Certificate-v4 (M12) preview: mma.sync.m16n8k32.kind::f8f6f4.f32.e4m3.e4m3.f32 (QMMA.16832)
// shares the S8 atom's fragment layouts, so only the policy changes (cuda/tests/layout_check.cu
// verifies the hash-tile mapping for it too). The kernel is not instantiated with it in v0.
struct Fp8E4M3Policy {
  using ElementA = cutlass::float_e4m3_t;
  using ElementB = cutlass::float_e4m3_t;
  using ElementAcc = float;
  using MmaOp = SM120_16x8x32_TN<cutlass::float_e4m3_t, cutlass::float_e4m3_t, float>;
  CUTE_HOST_DEVICE static uint32_t acc_bits(float x) {
#if defined(__CUDA_ARCH__)
    return __float_as_uint(x);
#else
    uint32_t u;
    std::memcpy(&u, &x, 4);
    return u;
#endif
  }
};
#endif

template <class Policy_, int Stages_>
struct HashGemmConfig {
  using Policy = Policy_;
  using ElementA = typename Policy::ElementA;
  using ElementB = typename Policy::ElementB;
  using ElementAcc = typename Policy::ElementAcc;

  static constexpr int kBM = 128;
  static constexpr int kBN = 128;
  static constexpr int kBK = 64;
  static constexpr int kStages = Stages_;
  static constexpr int kThreads = 128;
  static constexpr int kRank = 128;                 // transcript slice width r
  static constexpr int kTilesPerSlice = kRank / kBK;  // 2 k-tiles per fold
  static constexpr int kWords = 16;                 // transcript words

  using BM = Int<kBM>;
  using BN = Int<kBN>;
  using BK = Int<kBK>;

  // 2 x 2 warps. Logical M index i = a + 16*w + 32*r (atom row a, warp row w, repeat r) is
  // placed at physical row a + 64*w + 16*r; logical N index j = b + 8*w + 16*r at physical column
  // b + 64*w + 8*r. Warp (wm, wn) = (warp % 2, warp / 2) thus covers rows [64wm, 64wm + 64) and
  // cols [64wn, 64wn + 64) of the CTA tile.
  using PermM = Layout<Shape<_16, _2, _4>, Stride<_1, _64, _16>>;
  using PermN = Layout<Shape<_8, _2, _8>, Stride<_1, _64, _8>>;
  using TiledMma = TiledMMA<MMA_Atom<typename Policy::MmaOp>, Layout<Shape<_2, _2, _1>>,
                            Tile<PermM, PermN, _32>>;

  // K-major 64-byte rows; Swizzle<2,4,3> XORs the 16-byte chunk index with row bits 1..2 so the
  // 8 row addresses of every LDSM phase hit 8 distinct 16-byte bank groups.
  using SmemLayoutAtom =
      decltype(composition(Swizzle<2, 4, 3>{}, Layout<Shape<_8, _64>, Stride<_64, _1>>{}));
  using SmemLayoutA =
      decltype(tile_to_shape(SmemLayoutAtom{}, make_shape(BM{}, BK{}, Int<kStages>{})));
  using SmemLayoutB =
      decltype(tile_to_shape(SmemLayoutAtom{}, make_shape(BN{}, BK{}, Int<kStages>{})));

  // gmem -> smem: 128 threads as 32 rows x 4 threads, 16 bytes each (cp.async.cg, L1 bypass).
  using GmemCopyAtom = Copy_Atom<SM80_CP_ASYNC_CACHEGLOBAL<cute::uint128_t>, ElementA>;
  using GmemTiledCopy = decltype(make_tiled_copy(GmemCopyAtom{},
                                                 Layout<Shape<_32, _4>, Stride<_4, _1>>{},
                                                 Layout<Shape<_1, _16>>{}));

  // smem -> registers: ldmatrix.x4 for both operands.
  using S2RAtomA = Copy_Atom<SM75_U32x4_LDSM_N, ElementA>;
  using S2RAtomB = Copy_Atom<SM75_U32x4_LDSM_N, ElementB>;

  static constexpr int kSmemBytes =
      static_cast<int>((cosize_v<SmemLayoutA> + cosize_v<SmemLayoutB>) * sizeof(ElementA));
};

}  // namespace gemm
}  // namespace spm
