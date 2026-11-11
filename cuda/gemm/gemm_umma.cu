// GEN5 UMMA kernel instantiation and cluster launcher (gemm_umma.cuh), selected by GEN5_MINE=1.
#include "gemm_umma.cuh"

namespace spm {
namespace umma {

namespace {
cudaError_t configure_once() {
  static cudaError_t status = [] {
    return cudaFuncSetAttribute(gemm_hash_umma_kernel,
                                cudaFuncAttributeMaxDynamicSharedMemorySize, (int)SMEM_BYTES_5);
  }();
  return status;
}
}  // namespace

cudaError_t configure_gemm_hash_s8_umma() { return configure_once(); }

cudaError_t gemm_hash_s8_umma_attributes(cudaFuncAttributes* out) {
  return cudaFuncGetAttributes(out, gemm_hash_umma_kernel);
}

cudaError_t launch_gemm_hash_s8_umma(Params5 p, uint32_t grid, cudaStream_t stream) {
  const cudaError_t cfg = configure_once();
  if (cfg != cudaSuccess) return cfg;
  if (grid == 0) return cudaSuccess;
  // The collective builds its own TMA descriptors from the raw operands; row strides are the real
  // k_total (the kernel itself only iterates k_slices = floor(k/128) slices).
  typename CollectiveMainloop::Arguments cargs{
      const_cast<int8_t*>(p.a_ptr),
      cute::make_stride(int64_t(p.k_total), Int<1>{}, int64_t(p.m) * p.k_total),
      const_cast<int8_t*>(p.b_ptr),
      cute::make_stride(int64_t(p.k_total), Int<1>{}, int64_t(p.n) * p.k_total)};
  p.cutlass_params = CollectiveMainloop::to_underlying_arguments(
      make_shape(p.m, p.n, p.k_total, 1), cargs, nullptr, cutlass::KernelHardwareInfo{});
  const cutlass::Status st = cutlass::launch_kernel_on_cluster(
      cutlass::ClusterLaunchParams{dim3(grid), dim3(THREADS), dim3(2, 1, 1), (int)SMEM_BYTES_5},
      (const void*)&gemm_hash_umma_kernel, p);
  return st == cutlass::Status::kSuccess ? cudaSuccess : cudaErrorUnknown;
}

}  // namespace umma
}  // namespace spm
