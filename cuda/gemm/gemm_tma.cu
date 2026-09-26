// Int8 instantiation and launcher of the persistent TMA GEMM + hash kernel (gemm_tma.cuh).
#include "gemm_tma.cuh"

namespace spm {
namespace gemm {

namespace {
cudaError_t configure_once() {
  static cudaError_t status = [] {
    return cudaFuncSetAttribute(gemm_hash_kernel<MmaS8>, cudaFuncAttributeMaxDynamicSharedMemorySize,
                                (int)SMEM_BYTES);
  }();
  return status;
}
}  // namespace

cudaError_t launch_gemm_hash_s8(const CUtensorMap& tmap_a, const CUtensorMap& tmap_b,
                                const Params& p, uint32_t grid, cudaStream_t stream) {
  const cudaError_t cfg = configure_once();
  if (cfg != cudaSuccess) return cfg;
  if (grid == 0) return cudaSuccess;
  gemm_hash_kernel<MmaS8><<<grid, THREADS, SMEM_BYTES, stream>>>(tmap_a, tmap_b, p);
  return cudaGetLastError();
}

cudaError_t gemm_hash_s8_attributes(cudaFuncAttributes* out) {
  return cudaFuncGetAttributes(out, gemm_hash_kernel<MmaS8>);
}

}  // namespace gemm
}  // namespace spm
