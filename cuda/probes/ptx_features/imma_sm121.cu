// Compile-only probe: does the local nvcc emit native INT8 tensor-core SASS (IMMA.16832.S8.S8)
// for the GB10 (compute capability 12.1)? Build:
//   nvcc -gencode arch=compute_121,code=sm_121 -c imma_sm121.cu -o imma_sm121.o && cuobjdump -sass imma_sm121.o | grep -c IMMA
// Nothing here is meant to be launched; it only exercises mma.sync.m16n8k32 s8.s8.s32.
#include <cstdint>
__global__ void imma_probe(const int32_t* __restrict__ a, const int32_t* __restrict__ b, int32_t* __restrict__ c, int iters) {
  // Fragment registers for one m16n8k32 int8 MMA per thread (A: 4 regs, B: 2 regs, C/D: 4 regs).
  uint32_t a0 = a[threadIdx.x * 4 + 0], a1 = a[threadIdx.x * 4 + 1], a2 = a[threadIdx.x * 4 + 2], a3 = a[threadIdx.x * 4 + 3];
  uint32_t b0 = b[threadIdx.x * 2 + 0], b1 = b[threadIdx.x * 2 + 1];
  int32_t d0 = 0, d1 = 0, d2 = 0, d3 = 0;
  #pragma unroll 4
  for (int i = 0; i < iters; ++i) {
    asm volatile(
      "mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
      : "+r"(d0), "+r"(d1), "+r"(d2), "+r"(d3)
      : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(b0), "r"(b1));
  }
  c[threadIdx.x * 4 + 0] = d0; c[threadIdx.x * 4 + 1] = d1; c[threadIdx.x * 4 + 2] = d2; c[threadIdx.x * 4 + 3] = d3;
}
