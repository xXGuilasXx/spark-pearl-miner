// TMA probe for the GB10 (sm_121a, CUDA 13.0): cuTensorMapEncodeTiled through the runtime's driver
// entry point, one cp.async.bulk.tensor.2d load completing an mbarrier, and a check of the exact
// shared-memory layout of SWIZZLE_64B / SWIZZLE_128B against the model the GEMM's ldmatrix
// addressing uses:
//   byte offset o = row * box_width + col  ->  o ^ (((o >> 7) & mask) << 4),  mask = 3 (64B) or 7 (128B)
// i.e. 16-byte chunk c of row r lands at chunk c ^ ((r >> 1) & 3) for 64-byte rows.
// Build: nvcc -O2 -std=c++17 -gencode arch=compute_121a,code=sm_121a tma_swizzle.cu -o tma_swizzle
// Result on the author's unit (2026-09-26): 0 mismatches for SW64 128x64, SW128 128x128, SW64 256x64.
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <vector>
#include <cuda.h>
#include <cuda_runtime.h>

#define CK(x) do { cudaError_t e = (x); if (e != cudaSuccess) { printf("CUDA %s at %d\n", cudaGetErrorString(e), __LINE__); exit(1); } } while (0)

typedef CUresult (*EncodeFn)(CUtensorMap*, CUtensorMapDataType, cuuint32_t, void*, const cuuint64_t*, const cuuint64_t*,
                             const cuuint32_t*, const cuuint32_t*, CUtensorMapInterleave, CUtensorMapSwizzle,
                             CUtensorMapL2promotion, CUtensorMapFloatOOBfill);

__global__ void tma_kernel(const __grid_constant__ CUtensorMap map, int k0, int row0, int box_bytes, uint8_t* out) {
  extern __shared__ __align__(1024) uint8_t smem[];
  __shared__ __align__(8) uint64_t bar;
  uint32_t sbar = (uint32_t)__cvta_generic_to_shared(&bar);
  uint32_t sdst = (uint32_t)__cvta_generic_to_shared(smem);
  if (threadIdx.x == 0) {
    asm volatile("mbarrier.init.shared::cta.b64 [%0], %1;" :: "r"(sbar), "r"(1));
    asm volatile("fence.mbarrier_init.release.cluster;" ::: "memory");
  }
  __syncthreads();
  if (threadIdx.x == 0) {
    asm volatile("mbarrier.arrive.expect_tx.shared::cta.b64 _, [%0], %1;" :: "r"(sbar), "r"(box_bytes) : "memory");
    asm volatile("cp.async.bulk.tensor.2d.shared::cta.global.mbarrier::complete_tx::bytes [%0], [%1, {%2, %3}], [%4];"
                 :: "r"(sdst), "l"(&map), "r"(k0), "r"(row0), "r"(sbar) : "memory");
  }
  asm volatile(
      "{\n"
      ".reg .pred p;\n"
      "WAIT_%=:\n"
      "mbarrier.try_wait.parity.shared::cta.b64 p, [%0], %1;\n"
      "@!p bra WAIT_%=;\n"
      "}\n" :: "r"(sbar), "r"(0) : "memory");
  for (int i = threadIdx.x; i < box_bytes; i += blockDim.x) out[i] = smem[i];
}

int main() {
  const int K = 256, ROWS = 512;
  std::vector<uint8_t> h(K * ROWS);
  for (int r = 0; r < ROWS; ++r) for (int c = 0; c < K; ++c) h[r * K + c] = (uint8_t)((r * 131 + c * 7 + (r >> 3)) & 0xff);
  uint8_t *d, *o; CK(cudaMalloc(&d, h.size())); CK(cudaMalloc(&o, 1 << 16));
  CK(cudaMemcpy(d, h.data(), h.size(), cudaMemcpyHostToDevice));
  void* fn = nullptr; cudaDriverEntryPointQueryResult q;
  CK(cudaGetDriverEntryPointByVersion("cuTensorMapEncodeTiled", &fn, 12000, cudaEnableDefault, &q));
  if (!fn || q != cudaDriverEntryPointSuccess) { printf("no entry point\n"); return 1; }
  EncodeFn enc = (EncodeFn)fn;
  struct Case { int inner; CUtensorMapSwizzle sw; const char* name; int boxrows; };
  Case cases[] = {{64, CU_TENSOR_MAP_SWIZZLE_64B, "SW64 128x64", 128}, {128, CU_TENSOR_MAP_SWIZZLE_128B, "SW128 128x128", 128},
                  {64, CU_TENSOR_MAP_SWIZZLE_64B, "SW64 256x64", 256}};
  CK(cudaFuncSetAttribute(tma_kernel, cudaFuncAttributeMaxDynamicSharedMemorySize, 65536));
  int failures = 0;
  for (auto& cs : cases) {
    CUtensorMap map;
    cuuint64_t dims[2] = {(cuuint64_t)K, (cuuint64_t)ROWS};
    cuuint64_t strides[1] = {(cuuint64_t)K};
    cuuint32_t box[2] = {(cuuint32_t)cs.inner, (cuuint32_t)cs.boxrows};
    cuuint32_t estr[2] = {1, 1};
    CUresult r = enc(&map, CU_TENSOR_MAP_DATA_TYPE_UINT8, 2, d, dims, strides, box, estr, CU_TENSOR_MAP_INTERLEAVE_NONE,
                     cs.sw, CU_TENSOR_MAP_L2_PROMOTION_L2_256B, CU_TENSOR_MAP_FLOAT_OOB_FILL_NONE);
    if (r != CUDA_SUCCESS) { printf("%s: encode failed %d\n", cs.name, (int)r); ++failures; continue; }
    const int k0 = 64, row0 = 72;
    const int bytes = cs.inner * cs.boxrows;
    tma_kernel<<<1, 128, 65536>>>(map, k0, row0, bytes, o);
    CK(cudaDeviceSynchronize());
    std::vector<uint8_t> got(bytes); CK(cudaMemcpy(got.data(), o, bytes, cudaMemcpyDeviceToHost));
    const int mask = cs.inner == 64 ? 3 : 7;
    int bad = 0;
    for (int row = 0; row < cs.boxrows; ++row) for (int col = 0; col < cs.inner; ++col) {
      const int off = row * cs.inner + col;
      const int sw = off ^ (((off >> 7) & mask) << 4);
      if (got[sw] != h[(row0 + row) * K + k0 + col]) ++bad;
    }
    printf("%s: %d mismatches against the swizzle model (%d bytes)\n", cs.name, bad, bytes);
    failures += bad != 0;
  }
  return failures ? 1 : 0;
}
