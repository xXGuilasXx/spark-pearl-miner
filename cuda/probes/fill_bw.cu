// MB2 (partial) — operand-fill bandwidth probes for the GB10 (sm_121a):
//   1. L2/DRAM -> shared memory with cp.async.cg 16-byte copies (the gemm_v0 load path): every CTA
//      streams 24 KiB tiles (one A + B'ᵀ stage of the 128 x 256 x 64 kernel) through a 3-stage ring,
//      over windows of different sizes (small windows stay in the 24 MiB L2).
//   2. DSMEM: st.shared::cluster 16-byte stores into the peer CTA of a 2-CTA cluster.
//   3. DSMEM: cp.async.bulk shared::cta -> shared::cluster pushes (16 KiB) completing on the peer's
//      mbarrier.
// They bound what a CTA-tile design can get from L2 and whether sharing operands between the SMs
// of a cluster can cut that traffic (on GB10 it cannot: DSMEM moves ~1 B/clk/SM).
// Build: nvcc -O3 -gencode arch=compute_121a,code=sm_121a fill_bw.cu -o fill_bw   Run: ./fill_bw
// Short (well under a second of GPU time per line); fine with the vLLM resident.
#include <cstdint>
#include <cstdio>
#include <cstdlib>

#include <cuda_runtime.h>

#define CK(x)                                                                         \
  do {                                                                                \
    cudaError_t e = (x);                                                              \
    if (e != cudaSuccess) {                                                           \
      fprintf(stderr, "CUDA %s at %d\n", cudaGetErrorString(e), __LINE__);          \
      exit(1);                                                                        \
    }                                                                                 \
  } while (0)

constexpr int kStage = 24576;  // one A (8 KiB) + B'ᵀ (16 KiB) stage of gemm_v0

__global__ void __launch_bounds__(256, 1) cp_async_fill(const uint8_t* src, size_t window, int iters,
                                                        uint32_t* out) {
  extern __shared__ __align__(128) uint8_t smem[];
  const uint32_t base = static_cast<uint32_t>(__cvta_generic_to_shared(smem));
  size_t off = (static_cast<size_t>(blockIdx.x) * kStage * 7) % window;
  for (int it = 0; it < iters; ++it) {
    const uint32_t st = base + (it % 3) * kStage;
#pragma unroll
    for (int j = 0; j < 6; ++j) {
      const uint32_t idx = threadIdx.x + 256 * j;
      const uint8_t* p = src + (off + static_cast<size_t>(idx) * 16) % window;
      asm volatile("cp.async.cg.shared.global [%0], [%1], 16;\n" ::"r"(st + idx * 16), "l"(p));
    }
    asm volatile("cp.async.commit_group;\n" ::);
    asm volatile("cp.async.wait_group 2;\n" ::);
    off = (off + kStage) % window;
  }
  asm volatile("cp.async.wait_group 0;\n" ::);
  __syncthreads();
  if (threadIdx.x == 0 && smem[5] == 0x7e && smem[77] == 0x13) out[blockIdx.x] = 1;
}

__global__ void __cluster_dims__(2, 1, 1) __launch_bounds__(256, 1) dsmem_store(int iters, uint32_t* out) {
  extern __shared__ __align__(128) uint8_t smem[];
  uint32_t rank;
  asm("mov.u32 %0, %%cluster_ctarank;" : "=r"(rank));
  const uint32_t base = static_cast<uint32_t>(__cvta_generic_to_shared(smem));
  uint32_t peer;
  asm volatile("mapa.shared::cluster.u32 %0, %1, %2;" : "=r"(peer) : "r"(base), "r"(rank ^ 1u));
  asm volatile("barrier.cluster.arrive.release.aligned;\nbarrier.cluster.wait.acquire.aligned;\n" ::: "memory");
  uint32_t v = threadIdx.x;
  for (int it = 0; it < iters; ++it) {
    const uint32_t off = ((it * 256 + threadIdx.x) * 16) % 65536;
#pragma unroll
    for (int j = 0; j < 4; ++j) {
      asm volatile("st.shared::cluster.v4.u32 [%0], {%1,%1,%1,%1};" ::"r"(peer + ((off + j * 4096) % 65536)),
                   "r"(v)
                   : "memory");
    }
    ++v;
  }
  asm volatile("barrier.cluster.arrive.release.aligned;\nbarrier.cluster.wait.acquire.aligned;\n" ::: "memory");
  if (threadIdx.x == 0 && smem[3] == 77) out[blockIdx.x] = 1;
}

constexpr uint32_t kBulk = 16384, kBulkPerPhase = 32;

__global__ void __cluster_dims__(2, 1, 1) __launch_bounds__(128, 1) dsmem_bulk(int phases, uint32_t* out) {
  extern __shared__ __align__(128) uint8_t smem[];  // [0, 32K) source, [32K, 64K) destination
  __shared__ __align__(8) uint64_t mbar;
  uint32_t rank;
  asm("mov.u32 %0, %%cluster_ctarank;" : "=r"(rank));
  const uint32_t base = static_cast<uint32_t>(__cvta_generic_to_shared(smem));
  const uint32_t mb = static_cast<uint32_t>(__cvta_generic_to_shared(&mbar));
  uint32_t peer_dst, peer_mb;
  asm volatile("mapa.shared::cluster.u32 %0, %1, %2;" : "=r"(peer_dst) : "r"(base + 32768u), "r"(rank ^ 1u));
  asm volatile("mapa.shared::cluster.u32 %0, %1, %2;" : "=r"(peer_mb) : "r"(mb), "r"(rank ^ 1u));
  if (threadIdx.x == 0) {
    asm volatile("mbarrier.init.shared::cta.b64 [%0], 1;" ::"r"(mb));
    asm volatile("fence.mbarrier_init.release.cluster;" ::: "memory");
  }
  asm volatile("barrier.cluster.arrive.release.aligned;\nbarrier.cluster.wait.acquire.aligned;\n" ::: "memory");
  for (int ph = 0; ph < phases; ++ph) {
    if (threadIdx.x == 0) {
      asm volatile("mbarrier.arrive.expect_tx.shared::cta.b64 _, [%0], %1;" ::"r"(mb), "r"(kBulk * kBulkPerPhase)
                   : "memory");
    }
    asm volatile("barrier.cluster.arrive.release.aligned;\nbarrier.cluster.wait.acquire.aligned;\n" ::: "memory");
    if (threadIdx.x == 0) {
      for (uint32_t i = 0; i < kBulkPerPhase; ++i) {
        asm volatile(
            "cp.async.bulk.shared::cluster.shared::cta.mbarrier::complete_tx::bytes [%0], [%1], %2, [%3];" ::"r"(
                peer_dst + (i % 2) * kBulk),
            "r"(base + (i % 2) * kBulk), "r"(kBulk), "r"(peer_mb)
            : "memory");
      }
      uint32_t done = 0;
      while (!done) {
        asm volatile(
            "{ .reg .pred p; mbarrier.try_wait.parity.shared::cta.b64 p, [%1], %2; selp.u32 %0, 1, 0, p; }"
            : "=r"(done)
            : "r"(mb), "r"(static_cast<uint32_t>(ph & 1))
            : "memory");
      }
    }
    __syncthreads();
  }
  asm volatile("barrier.cluster.arrive.release.aligned;\nbarrier.cluster.wait.acquire.aligned;\n" ::: "memory");
  if (threadIdx.x == 0 && smem[40000] == 77) out[blockIdx.x] = 1;
}

template <typename F>
static double best_ms(F launch) {
  cudaEvent_t e0, e1;
  CK(cudaEventCreate(&e0));
  CK(cudaEventCreate(&e1));
  float best = 1e30f;
  for (int t = 0; t < 5; ++t) {
    CK(cudaEventRecord(e0));
    launch();
    CK(cudaEventRecord(e1));
    CK(cudaEventSynchronize(e1));
    float ms = 0.f;
    CK(cudaEventElapsedTime(&ms, e0, e1));
    if (ms < best) best = ms;
  }
  CK(cudaEventDestroy(e0));
  CK(cudaEventDestroy(e1));
  return best;
}

int main() {
  cudaDeviceProp p;
  CK(cudaGetDeviceProperties(&p, 0));
  int clk_khz = 0;
  CK(cudaDeviceGetAttribute(&clk_khz, cudaDevAttrClockRate, 0));
  const int sms = p.multiProcessorCount;
  const double hz = clk_khz * 1e3;
  printf("device: %s, %d SMs, L2 %d MiB (B/clk/SM at the %.0f MHz max clock)\n", p.name, sms, p.l2CacheSize >> 20,
         clk_khz / 1e3);
  uint8_t* src = nullptr;
  uint32_t* out = nullptr;
  const size_t buf = 256ull << 20;
  CK(cudaMalloc(&src, buf));
  CK(cudaMemset(src, 1, buf));
  CK(cudaMalloc(&out, 4096 * 4));
  CK(cudaFuncSetAttribute(cp_async_fill, cudaFuncAttributeMaxDynamicSharedMemorySize, 3 * kStage));
  CK(cudaFuncSetAttribute(dsmem_store, cudaFuncAttributeMaxDynamicSharedMemorySize, 65536));
  CK(cudaFuncSetAttribute(dsmem_bulk, cudaFuncAttributeMaxDynamicSharedMemorySize, 65536));

  for (size_t mib : {4, 8, 16, 64, 256}) {
    const int iters = 4000;
    const size_t window = mib << 20;
    const double ms = best_ms([&] { cp_async_fill<<<sms, 256, 3 * kStage>>>(src, window, iters, out); });
    const double bytes = static_cast<double>(sms) * iters * kStage;
    printf("cp.async.cg 16 B -> smem, window %3zu MiB: %7.1f GB/s  (%5.1f B/clk/SM)\n", mib,
           bytes / (ms / 1e3) / 1e9, bytes / (ms / 1e3) / sms / hz);
  }
  {
    const int iters = 20000;
    const int grid = sms - sms % 2;
    const double ms = best_ms([&] { dsmem_store<<<grid, 256, 65536>>>(iters, out); });
    const double bytes = static_cast<double>(grid) * 256 * iters * 4 * 16;
    printf("DSMEM st.shared::cluster.v4:          %7.1f GB/s  (%5.1f B/clk/SM)\n", bytes / (ms / 1e3) / 1e9,
           bytes / (ms / 1e3) / grid / hz);
  }
  {
    const int phases = 200;
    const int grid = sms - sms % 2;
    const double ms = best_ms([&] { dsmem_bulk<<<grid, 128, 65536>>>(phases, out); });
    const double bytes = static_cast<double>(grid) * phases * kBulkPerPhase * kBulk;
    printf("DSMEM cp.async.bulk 16 KiB pushes:    %7.1f GB/s  (%5.1f B/clk/SM)\n", bytes / (ms / 1e3) / 1e9,
           bytes / (ms / 1e3) / grid / hz);
  }
  CK(cudaFree(src));
  CK(cudaFree(out));
  return 0;
}
