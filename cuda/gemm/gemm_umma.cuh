// GEN5 UMMA mining kernel (5th generation, pool semantics, sm_110a 2SM UMMA i8), selected by
// GEN5_MINE=1. Ported from the standalone pearl-umma-miner/gen5_miner.cu (101.05 T-MAC/s measured
// on the Thor GB10 at 16384^2 x 4096, bit-exact VERIFY + fold-oracle double PASS, 2026-11-12).
//
// Architecture (full story in pearl-umma-miner GEN5-PORT-20261111.md):
//   * CUTLASS SM100 2SM UMMA i8 mainloop (256x256x128 cluster tile, 2x1 cluster, 5 stages,
//     TMA pipeline) + TMEM double-buffered per-slice deltas + 8 epilogue warps accumulating into
//     registers (128 i32 per thread).
//   * Static coordinate model: every epilogue thread owns one fixed TMEM datapath
//     (row = 32*(warp%4) + lane) and reads 32 consecutive columns per atom tile (col = 32*tg + e),
//     derived from Copy_Traits<SM100_TMEM_LOAD_32dp32b32x>. A one-shot in-kernel self-check
//     compares all 128 coordinates against the model and raises STATUS_BADMODEL on any mismatch,
//     so the hot loop runs on static arithmetic only.
//   * fold: hash tile = 8 rows x 16 cols inside one 64-row/64-col block (pool rows_pattern/
//     cols_pattern); per-slice XOR fold via per-thread 4-slot part + warp shfl merge + one
//     representative atomicXor per (lane<8) group into the 256-word fold_part.
//   * transcript: per-thread 16-word circular slots in shared memory (stride 17, bank-conflict
//     free); slot s&15 holds V_s = rotl13(V_{s-16}) ^ fold_s, which IS the pool's
//     t[s%16] = rotl13(t[s%16]) ^ fold — no end-of-slice rotation needed, the readout is the
//     transcript in pool order.
//   * differences vs. the standalone bench: tile space = the job's CTA-tile raster (128x256) with
//     cluster tiles pairing CTA rows (2r, 2r+1); the host keeps chunk boundaries even so a cluster
//     tile never straddles chunks (odd tiles_total tail: the OOB half is zero-filled by TMA and its
//     hash tiles are skipped); per-hash-tile validity guard (t_rows < m && t_cols < n) for
//     64-multiple edge shapes; no verify output, no perf timing.
#pragma once
#include <cuda.h>
#include <cuda_runtime.h>
#include <stdint.h>

#include "cute/tensor.hpp"
#include "cutlass/cutlass.h"
#include "cutlass/arch/barrier.h"
#include "cutlass/cluster_launch.hpp"
#include "cutlass/kernel_hardware_info.hpp"
#include "cutlass/gemm/collective/collective_builder.hpp"
#include "cutlass/pipeline/sm100_pipeline.hpp"

#include "../common/blake3.cuh"
#include "../common/u256.cuh"

namespace spm {
namespace umma {

using namespace cute;

#ifndef GEN5_TILE_M
#define GEN5_TILE_M 256
#endif
#ifndef GEN5_TILE_N
#define GEN5_TILE_N 256
#endif
#ifndef GEN5_TILE_K
#define GEN5_TILE_K 128
#endif
#ifndef GEN5_STAGES
#define GEN5_STAGES 5
#endif

using ElementA = int8_t;
using ElementB = int8_t;
using ElementAcc = int32_t;

using TileShape = Shape<Int<GEN5_TILE_M>, Int<GEN5_TILE_N>, Int<GEN5_TILE_K>>;
using ClusterShape = Shape<_2, _1, _1>;

using CollectiveMainloop = typename cutlass::gemm::collective::CollectiveBuilder<
    cutlass::arch::Sm100, cutlass::arch::OpClassTensorOp,
    ElementA, cutlass::layout::RowMajor, 16,
    ElementB, cutlass::layout::ColumnMajor, 16,
    ElementAcc, TileShape, ClusterShape,
    cutlass::gemm::collective::StageCount<GEN5_STAGES>,
    cutlass::gemm::KernelTmaWarpSpecialized2SmSm100>::CollectiveOp;

using TiledMma = typename CollectiveMainloop::TiledMma;
using SmemLayoutA = typename CollectiveMainloop::SmemLayoutA;
using SmemLayoutB = typename CollectiveMainloop::SmemLayoutB;
using CollectiveParams = typename CollectiveMainloop::Params;

using AtomThrShapeMNK = typename CollectiveMainloop::AtomThrShapeMNK;
using MainloopPipeline = cutlass::PipelineTmaUmmaAsync<GEN5_STAGES, ClusterShape, AtomThrShapeMNK>;
using AccumulatorPipeline = cutlass::PipelineUmmaAsync<2, AtomThrShapeMNK>;

constexpr int EPI_WARPS = 8;
constexpr int EPI_THREADS = EPI_WARPS * 32;
constexpr int TOTAL_WARPS = EPI_WARPS + 2;  // TMA + MMA + 8 epilogue
constexpr int THREADS = TOTAL_WARPS * 32;
constexpr uint32_t TRANSCRIPT_ROTL = 13;
constexpr uint32_t DUMP_WORDS = 26;  // t_rows, t_cols, transcript[16], digest[8] (104 bytes)

constexpr uint32_t STATUS_ABORTED = 1u;    // same bit as gemm::STATUS_ABORTED
constexpr uint32_t STATUS_BADMODEL = 2u;   // static coordinate self-check failed: do not trust

struct HitRecord5 {  // must stay layout-identical to spm_hit_t / gemm::HitRecord (40 bytes)
  uint32_t t_rows;
  uint32_t t_cols;
  uint32_t digest[8];
};

struct Params5 {
  uint32_t m, n;
  uint32_t k_slices;            // floor(k / 128): the kernel reads k columns [0, 128*k_slices)
  uint32_t k_total;             // the real operand extent (row stride of A'/B'ᵀ)
  uint32_t ctiles_m, ctiles_n;  // cluster tiles: ceil(m/256) x ceil(n/256)
  uint32_t swizzle;             // raster band height in CLUSTER rows (>= 1)
  uint32_t tile_begin, tile_end;  // this launch's cluster-tile range of the raster order
  uint32_t* status;             // STATUS_* bits, zeroed before every launch
  const volatile uint32_t* abort_flag;  // host-mapped; nonzero stops at the next tile
  uint32_t key[8];              // a_noise_seed (jackpot hash key)
  uint32_t bound[8];            // difficulty bound, LE words
  uint32_t* dump;               // DUMP_WORDS per hash tile in reference order, or nullptr
  uint32_t dump_row_stride;     // hash tiles per tile row = n / 16
  HitRecord5* hits;
  uint32_t* hit_count;
  uint32_t hit_capacity;
  const int8_t* a_ptr;          // A'  m x k_total row-major
  const int8_t* b_ptr;          // B'ᵀ n x k_total row-major
  uint32_t* progress;           // optional debug: producer writes 1-based tile index per tile
  CollectiveParams cutlass_params;  // built by the launcher from a_ptr/b_ptr (see .cu)
};

struct SharedStorage5 {
  struct {
    typename MainloopPipeline::SharedStorage mainloop;
    typename AccumulatorPipeline::SharedStorage accumulator;
  } pipelines;
  typename CollectiveMainloop::TensorStorage tensors;
  uint32_t tmem_base_ptr;
  uint32_t fold_part[256];       // per slice: fold of hash tile (HR,HC) (atomic XOR)
  uint32_t t_circ[256 * 17];     // per-thread transcript circular slots (stride 17: banks)
};

// Cluster-tile raster: bands of `swizzle` cluster rows, column-major inside a band (the L2 sweet
// spot measured at 253.9 T for the pure GEMM engine and kept for the mining kernel).
struct TileSeq5 {
  uint32_t ctiles_m, ctiles_n, swizzle;
  __device__ __forceinline__ uint32_t total() const { return ctiles_m * ctiles_n; }
  __device__ __forceinline__ void coords(uint32_t t, uint32_t& tm, uint32_t& tn) const {
    const uint32_t per_group = swizzle * ctiles_n;
    const uint32_t g = t / per_group;
    const uint32_t first = g * swizzle;
    const uint32_t rows = min(swizzle, ctiles_m - first);
    const uint32_t r = t - g * per_group;
    tn = r / rows;
    tm = first + (r - tn * rows);
  }
};
constexpr uint32_t SMEM_BYTES_5 = (uint32_t)sizeof(SharedStorage5);

/// Sets the dynamic shared-memory limit of the UMMA kernel (idempotent; also loads the module).
cudaError_t configure_gemm_hash_s8_umma();

/// Launches the UMMA kernel: `grid` persistent CTAs (grid = 2 * cluster count) over the cluster
/// tiles [tile_begin, tile_end). Builds the collective's TMA descriptors from p.a_ptr/p.b_ptr.
cudaError_t launch_gemm_hash_s8_umma(Params5 p, uint32_t grid, cudaStream_t stream);

/// Register / shared-memory / spill figures of the UMMA kernel (cudaFuncGetAttributes).
cudaError_t gemm_hash_s8_umma_attributes(cudaFuncAttributes* out);

}  // namespace umma
}  // namespace spm
