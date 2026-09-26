// Fused noised int8 GEMM + per-slice transcript + keyed BLAKE3 + bound compare (strategy B:
// CuTe/CUTLASS atoms).
//
// One CTA computes a 128 x 128 block of C' = A' * B'ᵀ with 4 warps of 64 x 64. Operand k-tiles of
// 64 bytes stream through a cp.async multistage pipeline in swizzled shared memory, LDSM moves
// them into registers and mma.sync.m16n8k32 (IMMA.16832.S8.S8) accumulates in i32. Every thread's
// 128 accumulators are exactly one 8 x 16 hash tile (hash_gemm_config.cuh), so after every
// r = 128 slice (two k-tiles) each thread XOR-folds its cumulative accumulators into one word and
// updates its 16-word transcript; after the last slice it hashes the transcript
// with BLAKE3 keyed by a_noise_seed and either records the tile (dump mode, 104 bytes in the
// reference order) or compares the digest with the bound and pushes hits into the ring.
#include "hash_gemm.cuh"

#include "hash_gemm_config.cuh"

#include "../common/blake3.cuh"
#include "../common/u256.cuh"

#ifndef SPM_GEMM_STAGES
#define SPM_GEMM_STAGES 3
#endif

// Register budget per thread (KERNEL.md: <= 232, zero spills). 2 CTAs x 128 threads x 232 regs
// fit the 64K-register file, so two CTAs stay resident per SM.
#ifndef SPM_GEMM_MAXNREG
#define SPM_GEMM_MAXNREG 232
#endif
#if SPM_GEMM_MAXNREG > 0
#define SPM_GEMM_BOUNDS __maxnreg__(SPM_GEMM_MAXNREG)
#else
#define SPM_GEMM_BOUNDS __launch_bounds__(128, 2)
#endif

namespace spm {
namespace gemm {
namespace {

using Cfg = HashGemmConfig<Int8Policy, SPM_GEMM_STAGES>;
static_assert(Cfg::kBM == kCtaTile && Cfg::kBN == kCtaTile, "launch code assumes square CTA tiles");

// Grouped raster: `group_m` CTA rows advance together along n, so the A' strips of the group
// stay in L2 while the B'ᵀ strips stream past.
__device__ __forceinline__ void raster(int cta, int tiles_m, int tiles_n, int group_m, int& tm,
                                       int& tn) {
  const int per_group = group_m * tiles_n;
  const int g = cta / per_group;
  const int first_m = g * group_m;
  const int rows = min(tiles_m - first_m, group_m);
  const int local = cta - g * per_group;
  tm = first_m + local % rows;
  tn = local / rows;
}

// The 16-word transcript of the thread's tile: t[s mod 16] = rotl(t[s mod 16], 13) ^ fold after
// slice s. Two interchangeable layouts, selected at build time:
//   * default: the words live in local memory (L1) indexed by the running slot, one load and one
//     store per 128-wide slice, which leaves 16 registers to the mainloop;
//   * SPM_TRANSCRIPT_IN_REGS=1: a register shift register (the slot to update is always w[0],
//     then the words rotate left by one, so every index is static).
#if defined(SPM_TRANSCRIPT_IN_REGS) && SPM_TRANSCRIPT_IN_REGS
struct Transcript {
  uint32_t w[16];
  __device__ __forceinline__ void init() {
#pragma unroll
    for (int j = 0; j < 16; ++j) w[j] = 0;
  }
  __device__ __forceinline__ void push(uint32_t fold) {
    const uint32_t x = __funnelshift_l(w[0], w[0], 13) ^ fold;
#pragma unroll
    for (int j = 0; j < 15; ++j) w[j] = w[j + 1];
    w[15] = x;
  }
  // After S slices the register holding logical word j is (j - S) mod 16; rotating left by
  // (16 - S mod 16) mod 16 restores the order (a no-op for k = 2048 and k = 4096).
  __device__ __forceinline__ void finish(uint32_t (&out)[16], int slices) {
    const int extra = (16 - (slices & 15)) & 15;
#pragma unroll
    for (int it = 0; it < 15; ++it) {
      if (it < extra) {
        const uint32_t first = w[0];
#pragma unroll
        for (int j = 0; j < 15; ++j) w[j] = w[j + 1];
        w[15] = first;
      }
    }
#pragma unroll
    for (int j = 0; j < 16; ++j) out[j] = w[j];
  }
};
#else
struct Transcript {
  uint32_t w[16];
  int slot;
  __device__ __forceinline__ void init() {
#pragma unroll
    for (int j = 0; j < 16; ++j) w[j] = 0;
    slot = 0;
  }
  __device__ __forceinline__ void push(uint32_t fold) {
    const uint32_t x = w[slot];
    w[slot] = __funnelshift_l(x, x, 13) ^ fold;
    slot = (slot + 1) & 15;
  }
  __device__ __forceinline__ void finish(uint32_t (&out)[16], int /*slices*/) {
#pragma unroll
    for (int j = 0; j < 16; ++j) out[j] = w[j];
  }
};
#endif

template <class Config, bool kDump>
__global__ void SPM_GEMM_BOUNDS
    hash_gemm_kernel(const __grid_constant__ HashGemmParams p) {
  using namespace cute;
  using TA = typename Config::ElementA;
  using TB = typename Config::ElementB;
  using Policy = typename Config::Policy;
  constexpr int kStages = Config::kStages;

  const int cta = p.cta_base + static_cast<int>(blockIdx.x);
  if (cta >= p.total_ctas) return;
  int tile_m, tile_n;
  raster(cta, p.tiles_m, p.tiles_n, p.group_m, tile_m, tile_n);

  // The CTA's operand strips: 128 rows of A' and of B'ᵀ, only the k-tiles the transcript uses.
  const int k_tiles = p.slices * Config::kTilesPerSlice;
  const TA* a_blk = reinterpret_cast<const TA*>(p.a) + static_cast<size_t>(tile_m) * Config::kBM * p.k;
  const TB* b_blk = reinterpret_cast<const TB*>(p.bt) + static_cast<size_t>(tile_n) * Config::kBN * p.k;
  Tensor mA = make_tensor(make_gmem_ptr(a_blk),
                          make_layout(make_shape(typename Config::BM{}, k_tiles * Config::kBK),
                                      make_stride(p.k, _1{})));
  Tensor mB = make_tensor(make_gmem_ptr(b_blk),
                          make_layout(make_shape(typename Config::BN{}, k_tiles * Config::kBK),
                                      make_stride(p.k, _1{})));
  Tensor gA = local_tile(mA, make_tile(typename Config::BM{}, typename Config::BK{}),
                         make_coord(0, _));  // (BM, BK, KT)
  Tensor gB = local_tile(mB, make_tile(typename Config::BN{}, typename Config::BK{}),
                         make_coord(0, _));  // (BN, BK, KT)

  extern __shared__ __align__(128) uint8_t smem_raw[];
  TA* smem_a = reinterpret_cast<TA*>(smem_raw);
  TB* smem_b = reinterpret_cast<TB*>(smem_raw + cosize_v<typename Config::SmemLayoutA> * sizeof(TA));
  Tensor sA = make_tensor(make_smem_ptr(smem_a), typename Config::SmemLayoutA{});  // (BM, BK, PIPE)
  Tensor sB = make_tensor(make_smem_ptr(smem_b), typename Config::SmemLayoutB{});  // (BN, BK, PIPE)

  // gmem -> smem partitions.
  typename Config::GmemTiledCopy g2s;
  auto g2s_thr = g2s.get_slice(threadIdx.x);
  Tensor tAgA = g2s_thr.partition_S(gA);  // (CPY, CPY_M, CPY_K, KT)
  Tensor tAsA = g2s_thr.partition_D(sA);  // (CPY, CPY_M, CPY_K, PIPE)
  Tensor tBgB = g2s_thr.partition_S(gB);
  Tensor tBsB = g2s_thr.partition_D(sB);

  // MMA partitions and accumulators.
  typename Config::TiledMma mma;
  auto thr_mma = mma.get_slice(threadIdx.x);
  Tensor tCrA = thr_mma.partition_fragment_A(sA(_, _, 0));  // (MMA, MMA_M, MMA_K)
  Tensor tCrB = thr_mma.partition_fragment_B(sB(_, _, 0));  // (MMA, MMA_N, MMA_K)
  Tensor tCrC = partition_fragment_C(mma, make_shape(typename Config::BM{}, typename Config::BN{}));
  static_assert(decltype(size(tCrC))::value == 128, "one hash tile per thread");
  clear(tCrC);

  // smem -> register copies (LDSM), retiled onto the MMA fragments.
  auto s2r_a = make_tiled_copy_A(typename Config::S2RAtomA{}, mma);
  auto s2r_thr_a = s2r_a.get_slice(threadIdx.x);
  Tensor tXsA = s2r_thr_a.partition_S(sA);  // (CPY, MMA_M, MMA_K, PIPE)
  Tensor tXrA = s2r_thr_a.retile_D(tCrA);   // (CPY, MMA_M, MMA_K)
  auto s2r_b = make_tiled_copy_B(typename Config::S2RAtomB{}, mma);
  auto s2r_thr_b = s2r_b.get_slice(threadIdx.x);
  Tensor tXsB = s2r_thr_b.partition_S(sB);  // (CPY, MMA_N, MMA_K, PIPE)
  Tensor tXrB = s2r_thr_b.retile_D(tCrB);   // (CPY, MMA_N, MMA_K)

  constexpr int kKBlocks = decltype(size<2>(tCrA))::value;  // k-blocks of 32 per k-tile
  static_assert(kKBlocks == Config::kBK / 32, "k-blocks per k-tile");

  // Prologue: k-tiles 0 .. kStages-2 in flight. Groups are committed even when empty so the
  // cp.async.wait_group counts stay uniform at the tail.
  int kt_load = 0;
#pragma unroll
  for (int s = 0; s < kStages - 1; ++s) {
    if (kt_load < k_tiles) {
      copy(g2s, tAgA(_, _, _, kt_load), tAsA(_, _, _, s));
      copy(g2s, tBgB(_, _, _, kt_load), tBsB(_, _, _, s));
    }
    cp_async_fence();
    ++kt_load;
  }

  int read_stage = 0;
  int write_stage = kStages - 1;
  Tensor tXsA_p = tXsA(_, _, _, read_stage);
  Tensor tXsB_p = tXsB(_, _, _, read_stage);

  cp_async_wait<kStages - 2>();
  __syncthreads();
  copy(s2r_a, tXsA_p(_, _, Int<0>{}), tXrA(_, _, Int<0>{}));
  copy(s2r_b, tXsB_p(_, _, Int<0>{}), tXrB(_, _, Int<0>{}));

  Transcript transcript;
  transcript.init();

  // One k-tile: LDSM of the next k-block overlaps the MMAs of the current one; the gmem -> smem
  // copy of the k-tile kStages-1 ahead is issued at the first k-block.
  auto k_tile = [&]() {
#pragma unroll
    for (int kb = 0; kb < kKBlocks; ++kb) {
      if (kb == kKBlocks - 1) {
        // The next stage was issued kStages-1 k-tiles ago; wait for it and make it visible.
        tXsA_p = tXsA(_, _, _, read_stage);
        tXsB_p = tXsB(_, _, _, read_stage);
        cp_async_wait<kStages - 2>();
        __syncthreads();
      }
      const int kb_next = (kb + 1) % kKBlocks;
      copy(s2r_a, tXsA_p(_, _, kb_next), tXrA(_, _, kb_next));
      copy(s2r_b, tXsB_p(_, _, kb_next), tXrB(_, _, kb_next));
      if (kb == 0) {
        if (kt_load < k_tiles) {
          copy(g2s, tAgA(_, _, _, kt_load), tAsA(_, _, _, write_stage));
          copy(g2s, tBgB(_, _, _, kt_load), tBsB(_, _, _, write_stage));
        }
        cp_async_fence();
        ++kt_load;
        write_stage = read_stage;
        read_stage = (read_stage + 1 == kStages) ? 0 : read_stage + 1;
      }
      cute::gemm(mma, tCrA(_, _, kb), tCrB(_, _, kb), tCrC);
    }
  };

  // End of an r-wide slice: XOR of the 128 cumulative accumulators (order independent).
  auto fold = [&]() {
    uint32_t x0 = 0, x1 = 0, x2 = 0, x3 = 0;
#pragma unroll
    for (int i = 0; i < 128; i += 4) {
      x0 ^= Policy::acc_bits(tCrC(i));
      x1 ^= Policy::acc_bits(tCrC(i + 1));
      x2 ^= Policy::acc_bits(tCrC(i + 2));
      x3 ^= Policy::acc_bits(tCrC(i + 3));
    }
    transcript.push((x0 ^ x1) ^ (x2 ^ x3));
  };

#pragma unroll 1
  for (int kt = 0; kt < k_tiles; ++kt) {
    k_tile();
    if (kt % Config::kTilesPerSlice == Config::kTilesPerSlice - 1) fold();
  }
  cp_async_wait<0>();
  uint32_t t[16];
  transcript.finish(t, p.slices);

  // Tile base = coordinate of accumulator 0 (layout_check.cu proves it is the minimum).
  Tensor cC = make_identity_tensor(make_shape(typename Config::BM{}, typename Config::BN{}));
  Tensor tCcC = thr_mma.partition_C(cC);
  const uint32_t t_rows = static_cast<uint32_t>(tile_m * Config::kBM + get<0>(tCcC(0)));
  const uint32_t t_cols = static_cast<uint32_t>(tile_n * Config::kBN + get<1>(tCcC(0)));

  uint32_t digest[8];
  blake3::keyed_hash_64(p.key, t, digest);

  if constexpr (kDump) {
    // Reference order: tile rows t_rows = 64a + g (g < 8) are row index 8a + g; tile columns
    // t_cols = 64b + 2c (c < 4) are column index 4b + c; rows outer, columns inner.
    const uint64_t row_idx = (t_rows >> 6) * 8u + (t_rows & 63u);
    const uint64_t col_idx = (t_cols >> 6) * 4u + ((t_cols & 63u) >> 1);
    const uint64_t idx = row_idx * static_cast<uint64_t>(p.n / 16) + col_idx;
    uint2* rec = reinterpret_cast<uint2*>(p.dump + idx * 104u);
    rec[0] = make_uint2(t_rows, t_cols);
#pragma unroll
    for (int j = 0; j < 8; ++j) rec[1 + j] = make_uint2(t[2 * j], t[2 * j + 1]);
#pragma unroll
    for (int j = 0; j < 4; ++j) rec[9 + j] = make_uint2(digest[2 * j], digest[2 * j + 1]);
  } else {
    if (u256_le_leq(digest, p.bound)) {
      const uint32_t slot = atomicAdd(p.hit_count, 1u) % p.hit_capacity;
      spm_hit_t* h = p.hits + slot;
      h->t_rows = t_rows;
      h->t_cols = t_cols;
      uint32_t* d = reinterpret_cast<uint32_t*>(h->digest);
#pragma unroll
      for (int j = 0; j < 8; ++j) d[j] = digest[j];
    }
  }
}

template <bool kDump>
cudaError_t configure_kernel() {
  auto kernel = hash_gemm_kernel<Cfg, kDump>;
  cudaError_t e = cudaFuncSetAttribute(kernel, cudaFuncAttributeMaxDynamicSharedMemorySize,
                                       Cfg::kSmemBytes);
  if (e != cudaSuccess) return e;
  // Ask for the largest shared-memory carveout so two CTAs fit on one SM.
  return cudaFuncSetAttribute(kernel, cudaFuncAttributePreferredSharedMemoryCarveout,
                              cudaSharedmemCarveoutMaxShared);
}

template <bool kDump>
cudaError_t launch(const HashGemmParams& p, int ctas, cudaStream_t stream) {
  static const cudaError_t configured = configure_kernel<kDump>();
  if (configured != cudaSuccess) return configured;
  hash_gemm_kernel<Cfg, kDump><<<ctas, Cfg::kThreads, Cfg::kSmemBytes, stream>>>(p);
  return cudaGetLastError();
}

template <bool kDump>
int ctas_per_sm() {
  if (configure_kernel<kDump>() != cudaSuccess) return 0;
  int n = 0;
  if (cudaOccupancyMaxActiveBlocksPerMultiprocessor(&n, hash_gemm_kernel<Cfg, kDump>, Cfg::kThreads,
                                                    Cfg::kSmemBytes) != cudaSuccess)
    return 0;
  return n;
}

}  // namespace

int hash_gemm_smem_bytes() { return Cfg::kSmemBytes; }
int hash_gemm_stages() { return Cfg::kStages; }

int hash_gemm_ctas_per_sm(bool dump) { return dump ? ctas_per_sm<true>() : ctas_per_sm<false>(); }

cudaError_t launch_hash_gemm(const HashGemmParams& p, int ctas, bool dump, cudaStream_t stream) {
  if (ctas <= 0) return cudaSuccess;
  return dump ? launch<true>(p, ctas, stream) : launch<false>(p, ctas, stream);
}

}  // namespace gemm
}  // namespace spm
