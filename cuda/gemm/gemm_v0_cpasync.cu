// Instantiations and launcher of the v0 fused kernel (see gemm_v0_cpasync.cuh).
// Only the int8 (certificate V3) policy is instantiated; the FP8 policy in mma_ops.cuh shares
// the mainloop and is instantiated once the V4 transcript is specified (M12).
#include <cuda_runtime.h>

#include "gemm_v0_cpasync.cuh"
#include "spm_internal.h"

namespace spm {

namespace {
using gemm_v0::fused_kernel;
using Int8 = mma::S8S8S32;
}  // namespace

GemmGeometry gemm_v0_geometry(uint32_t m, uint32_t n) {
  GemmGeometry g{};
  g.block_m = gemm_v0::kBM;
  g.block_n = gemm_v0::kBN;
  g.tiles_m = (m + gemm_v0::kBM - 1) / gemm_v0::kBM;
  g.tiles_n = (n + gemm_v0::kBN - 1) / gemm_v0::kBN;
  g.ctas_per_sm = 1;
  g.smem_bytes = gemm_v0::kSmemBytes;
  return g;
}

cudaError_t gemm_v0_prepare(uint32_t* ctas_per_sm) {
  cudaError_t e = cudaFuncSetAttribute(fused_kernel<Int8, false>,
                                       cudaFuncAttributeMaxDynamicSharedMemorySize,
                                       gemm_v0::kSmemBytes);
  if (e != cudaSuccess) return e;
  e = cudaFuncSetAttribute(fused_kernel<Int8, true>, cudaFuncAttributeMaxDynamicSharedMemorySize,
                           gemm_v0::kSmemBytes);
  if (e != cudaSuccess) return e;
  int blocks = 0;
  e = cudaOccupancyMaxActiveBlocksPerMultiprocessor(&blocks, fused_kernel<Int8, false>,
                                                    gemm_v0::kThreads, gemm_v0::kSmemBytes);
  if (e != cudaSuccess) return e;
  if (blocks < 1) return cudaErrorInvalidConfiguration;
  if (ctas_per_sm) *ctas_per_sm = static_cast<uint32_t>(blocks);
  return cudaSuccess;
}

cudaError_t launch_gemm_v0(const GemmArgs& args, uint32_t tile_begin, uint32_t tile_count,
                           bool dump, cudaStream_t stream) {
  if (tile_count == 0) return cudaSuccess;
  const GemmGeometry g = gemm_v0_geometry(args.m, args.n);
  const dim3 grid(tile_count), block(gemm_v0::kThreads);
  if (dump) {
    fused_kernel<Int8, true><<<grid, block, gemm_v0::kSmemBytes, stream>>>(args, tile_begin, g.tiles_m,
                                                                           g.tiles_n);
  } else {
    fused_kernel<Int8, false><<<grid, block, gemm_v0::kSmemBytes, stream>>>(args, tile_begin,
                                                                            g.tiles_m, g.tiles_n);
  }
  return cudaGetLastError();
}

}  // namespace spm
