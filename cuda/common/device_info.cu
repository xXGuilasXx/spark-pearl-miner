#include "spm_cuda.h"
#include <cuda_runtime.h>
#include <cstring>
#include <cstdio>

extern "C" const char* spm_cuda_version(void) { return "libspm_cuda 0.0.1 (sm_121a)"; }

extern "C" int32_t spm_cuda_device_info(spm_device_info_t* out) {
  if (!out) return -1;
  cudaDeviceProp p;
  cudaError_t e = cudaGetDeviceProperties(&p, 0);
  if (e != cudaSuccess) return (int32_t)e;
  memset(out, 0, sizeof(*out));
  strncpy(out->name, p.name, sizeof(out->name) - 1);
  out->sm_count = p.multiProcessorCount;
  out->cc_major = p.major; out->cc_minor = p.minor;
  int smem = 0; cudaDeviceGetAttribute(&smem, cudaDevAttrMaxSharedMemoryPerBlockOptin, 0); out->max_smem_optin_bytes = smem;
  out->regs_per_sm = p.regsPerMultiprocessor;
  int clk = 0; cudaDeviceGetAttribute(&clk, cudaDevAttrClockRate, 0); out->sm_clock_khz = clk;
  out->total_mem_bytes = (int64_t)p.totalGlobalMem;
  return 0;
}

// Register-only INT8 mma.sync loop (same as cuda/probes/imma_peak.cu, without NVML).
__global__ static void imma_loop(long iters, uint32_t seed, int32_t* out) {
  uint32_t a0 = seed ^ threadIdx.x, a1 = seed * 3u, a2 = seed * 5u, a3 = seed * 7u, b0 = seed * 11u, b1 = seed * 13u;
  int32_t d0 = 0, d1 = 0, d2 = 0, d3 = 0, e0 = 0, e1 = 0, e2 = 0, e3 = 0;
  for (long i = 0; i < iters; ++i) {
    asm volatile("mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
      : "+r"(d0), "+r"(d1), "+r"(d2), "+r"(d3) : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(b0), "r"(b1));
    asm volatile("mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
      : "+r"(e0), "+r"(e1), "+r"(e2), "+r"(e3) : "r"(a1), "r"(a2), "r"(a3), "r"(a0), "r"(b1), "r"(b0));
  }
  if ((d0 ^ d1 ^ d2 ^ d3 ^ e0 ^ e1 ^ e2 ^ e3) == 0x7eadbeef) out[threadIdx.x] = d0;
}

extern "C" double spm_cuda_imma_peak(double seconds) {
  cudaDeviceProp p; if (cudaGetDeviceProperties(&p, 0) != cudaSuccess) return 0.0;
  int32_t* o = nullptr; if (cudaMalloc(&o, 4096) != cudaSuccess) return 0.0;
  dim3 grid(4 * p.multiProcessorCount), block(256);
  cudaEvent_t ev0, ev1; cudaEventCreate(&ev0); cudaEventCreate(&ev1);
  long iters = 1 << 12; float ms = 0.f;
  for (;;) { cudaEventRecord(ev0); imma_loop<<<grid, block>>>(iters, 0x1234567u, o); cudaEventRecord(ev1); cudaEventSynchronize(ev1); cudaEventElapsedTime(&ms, ev0, ev1); if (ms > 200.f) break; iters *= 4; }
  iters = (long)(iters * (seconds * 1000.0 / ms));
  cudaEventRecord(ev0); imma_loop<<<grid, block>>>(iters, 0x1234567u, o); cudaEventRecord(ev1); cudaEventSynchronize(ev1); cudaEventElapsedTime(&ms, ev0, ev1);
  double warps = 8.0 * grid.x; double macs = (double)iters * 2.0 * warps * (16.0 * 8 * 32);
  cudaFree(o); cudaEventDestroy(ev0); cudaEventDestroy(ev1);
  return macs / (ms / 1000.0) / 1e12;
}
