// TMA streaming bandwidth on the GB10 with the GEMM's access pattern: per CTA tile, k-tiles of an
// A box (128 rows x W bytes) and a B box (256 rows x W bytes) land in a ring of S shared-memory
// stages; 8 warps only wait for each stage and release it (no math), and thread 0 refills the stage
// released one k-tile earlier, like the first version of the strategy-C kernel. Tiles are taken
// M-inner, so with M = 2048 the 48 CTAs cover one 16-row raster band: A stays L2-resident and every
// B column block is fetched from DRAM once and shared by 16 CTAs.
// Build: nvcc -O3 -std=c++17 -gencode arch=compute_121a,code=sm_121a tma_stream.cu -o tma_stream
// Run:   ./tma_stream [M=2048] [N=16384]     (K = 4096)
// Result on the author's unit (2026-09-26, vLLM resident, clock not locked): W=64 x 4 stages
// 1.87-2.15 TB/s delivered to shared memory (the GEMM at 85 T-MAC/s needs ~1.0 TB/s).
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <cuda.h>
#include <cuda_runtime.h>

#define CK(x) do { cudaError_t e = (x); if (e != cudaSuccess) { printf("CUDA %s at %d\n", cudaGetErrorString(e), __LINE__); exit(1); } } while (0)

typedef CUresult (*EncodeFn)(CUtensorMap*, CUtensorMapDataType, cuuint32_t, void*, const cuuint64_t*, const cuuint64_t*,
                             const cuuint32_t*, const cuuint32_t*, CUtensorMapInterleave, CUtensorMapSwizzle,
                             CUtensorMapL2promotion, CUtensorMapFloatOOBfill);

__device__ __forceinline__ void mbar_init(uint32_t bar, uint32_t count) {
  asm volatile("mbarrier.init.shared::cta.b64 [%0], %1;" ::"r"(bar), "r"(count));
}
__device__ __forceinline__ void mbar_wait(uint32_t bar, uint32_t parity) {
  asm volatile("{\n.reg .pred p;\nW_%=:\nmbarrier.try_wait.parity.shared::cta.b64 p, [%0], %1;\n@!p bra W_%=;\n}" ::"r"(bar),
               "r"(parity) : "memory");
}
__device__ __forceinline__ void mbar_arrive(uint32_t bar) {
  asm volatile("mbarrier.arrive.shared::cta.b64 _, [%0];" ::"r"(bar) : "memory");
}
__device__ __forceinline__ void mbar_expect(uint32_t bar, uint32_t bytes) {
  asm volatile("mbarrier.arrive.expect_tx.shared::cta.b64 _, [%0], %1;" ::"r"(bar), "r"(bytes) : "memory");
}
__device__ __forceinline__ void tma2d(uint32_t dst, const CUtensorMap* m, int c0, int c1, uint32_t bar) {
  asm volatile("cp.async.bulk.tensor.2d.shared::cta.global.mbarrier::complete_tx::bytes [%0], [%1, {%2, %3}], [%4];"
               ::"r"(dst), "l"((uint64_t)m), "r"(c0), "r"(c1), "r"(bar) : "memory");
}

struct P { int tiles_m, tiles_n, ktiles, bm, bn, w, stages; };

__global__ void __launch_bounds__(256, 1) stream_kernel(const __grid_constant__ CUtensorMap ta,
                                                         const __grid_constant__ CUtensorMap tb, P p) {
  extern __shared__ __align__(1024) uint8_t smem_raw[];
  const uint32_t raw = (uint32_t)__cvta_generic_to_shared(smem_raw);
  const uint32_t base = (raw + 1023) & ~1023u;
  const uint32_t a_bytes = p.bm * p.w, b_bytes = p.bn * p.w, stage_bytes = a_bytes + b_bytes;
  const uint32_t full0 = base + p.stages * stage_bytes, empty0 = full0 + 8 * p.stages;
  const int lane = threadIdx.x & 31;
  if (threadIdx.x == 0) {
    for (int s = 0; s < p.stages; ++s) { mbar_init(full0 + 8 * s, 1); mbar_init(empty0 + 8 * s, 8); }
    asm volatile("fence.mbarrier_init.release.cluster;" ::: "memory");
  }
  __syncthreads();
  const int total = p.tiles_m * p.tiles_n;
  int pk = 0, ptile = blockIdx.x, pstage = 0, pphase = 0;
  auto produce = [&]() {
    if (ptile >= total) return;
    const int tm = ptile % p.tiles_m, tn = ptile / p.tiles_m;
    const uint32_t fb = full0 + 8 * pstage;
    mbar_wait(empty0 + 8 * pstage, pphase ^ 1);
    mbar_expect(fb, stage_bytes);
    const uint32_t dst = base + pstage * stage_bytes;
    tma2d(dst, &ta, pk * p.w, tm * p.bm, fb);
    tma2d(dst + a_bytes, &tb, pk * p.w, tn * p.bn, fb);
    if (++pk == p.ktiles) { pk = 0; ptile += gridDim.x; }
    if (++pstage == p.stages) { pstage = 0; pphase ^= 1; }
  };
  if (threadIdx.x == 0) for (int s = 0; s + 1 < p.stages; ++s) produce();
  __syncwarp();
  int stage = 0, phase = 0;
  for (int tile = blockIdx.x; tile < total; tile += gridDim.x) {
    for (int kt = 0; kt < p.ktiles; ++kt) {
      mbar_wait(full0 + 8 * stage, phase);
      __syncwarp();
      if (lane == 0) mbar_arrive(empty0 + 8 * stage);
      if (++stage == p.stages) { stage = 0; phase ^= 1; }
      if (threadIdx.x == 0) produce();
      __syncwarp();
    }
  }
}

int main(int argc, char** argv) {
  const int M = argc > 1 ? atoi(argv[1]) : 2048, N = argc > 2 ? atoi(argv[2]) : 16384, K = 4096;
  void* fn = nullptr; cudaDriverEntryPointQueryResult q;
  CK(cudaGetDriverEntryPointByVersion("cuTensorMapEncodeTiled", &fn, 12000, cudaEnableDefault, &q));
  EncodeFn enc = (EncodeFn)fn;
  int8_t *a, *b; CK(cudaMalloc(&a, (size_t)M * K)); CK(cudaMalloc(&b, (size_t)N * K));
  CK(cudaMemset(a, 1, (size_t)M * K)); CK(cudaMemset(b, 2, (size_t)N * K));
  CK(cudaFuncSetAttribute(stream_kernel, cudaFuncAttributeMaxDynamicSharedMemorySize, 101376));
  struct Cfg { int w, stages; } cfgs[] = {{64, 4}, {64, 3}, {128, 2}};
  for (auto c : cfgs) {
    const int bm = 128, bn = 256;
    auto mk = [&](CUtensorMap* m, int8_t* ptr, int rows, int boxrows) {
      cuuint64_t dims[2] = {(cuuint64_t)K, (cuuint64_t)rows}; cuuint64_t str[1] = {(cuuint64_t)K};
      cuuint32_t box[2] = {(cuuint32_t)c.w, (cuuint32_t)boxrows}; cuuint32_t es[2] = {1, 1};
      CUresult r = enc(m, CU_TENSOR_MAP_DATA_TYPE_UINT8, 2, ptr, dims, str, box, es, CU_TENSOR_MAP_INTERLEAVE_NONE,
                       c.w == 64 ? CU_TENSOR_MAP_SWIZZLE_64B : CU_TENSOR_MAP_SWIZZLE_128B,
                       CU_TENSOR_MAP_L2_PROMOTION_L2_256B, CU_TENSOR_MAP_FLOAT_OOB_FILL_NONE);
      if (r != CUDA_SUCCESS) { printf("encode failed %d\n", (int)r); exit(1); }
    };
    CUtensorMap ta, tb; mk(&ta, a, M, bm); mk(&tb, b, N, bn);
    P p{M / bm, N / bn, K / c.w, bm, bn, c.w, c.stages};
    stream_kernel<<<48, 256, 101376>>>(ta, tb, p);
    CK(cudaDeviceSynchronize());
    cudaEvent_t e0, e1; cudaEventCreate(&e0); cudaEventCreate(&e1);
    const int reps = 5;
    cudaEventRecord(e0);
    for (int r = 0; r < reps; ++r) stream_kernel<<<48, 256, 101376>>>(ta, tb, p);
    cudaEventRecord(e1); CK(cudaEventSynchronize(e1));
    float ms; cudaEventElapsedTime(&ms, e0, e1);
    const double bytes = (double)(M / bm) * (N / bn) * K * (bm + bn);
    const double s = ms / 1e3 / reps;
    printf("W=%3d stages=%d: %.0f GB/s delivered to shared memory (equivalent to %.1f T-MAC/s of 128x256 tiles)\n",
           c.w, c.stages, bytes / s / 1e9, (double)M * N * K / s / 1e12);
  }
  return 0;
}
