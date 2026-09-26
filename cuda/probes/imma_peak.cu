// MB1 — register-only tensor-core peak on the GB10 (sm_121a).
// Measures sustained mma.sync throughput with all operands in registers (no memory traffic):
//   INT8:  mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32            (SASS IMMA.16832)
//   FP8:   mma.sync.aligned.m16n8k32.row.col.kind::f8f6f4.f32.e4m3.e4m3.f32 (SASS QMMA.16832)
// Each mma is 16*8*32 = 4096 MACs. Reports MAC/s, TOPS (2 ops per MAC) and MAC per clock per SM
// using the SM clock sampled by the host through NVML while the kernel runs.
// Build: nvcc -O3 -gencode arch=compute_121a,code=sm_121a -lnvidia-ml imma_peak.cu -o imma_peak
// Run:   ./imma_peak [seconds_per_test=3]
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <chrono>
#include <thread>
#include <atomic>
#include <cuda_runtime.h>
#include <nvml.h>

#define CK(x) do { cudaError_t e = (x); if (e != cudaSuccess) { fprintf(stderr, "CUDA %s at %d\n", cudaGetErrorString(e), __LINE__); exit(1); } } while (0)

constexpr int UNROLL = 8;

__global__ void imma_kernel(long iters, uint32_t seed, int32_t* out) {
  uint32_t a0 = seed ^ threadIdx.x, a1 = seed * 3u, a2 = seed * 5u, a3 = seed * 7u;
  uint32_t b0 = seed * 11u, b1 = seed * 13u;
  int32_t d[4][UNROLL];
  #pragma unroll
  for (int u = 0; u < UNROLL; ++u) { d[0][u] = 0; d[1][u] = 0; d[2][u] = 0; d[3][u] = 0; }
  for (long i = 0; i < iters; ++i) {
    #pragma unroll
    for (int u = 0; u < UNROLL; ++u) {
      asm volatile("mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
        : "+r"(d[0][u]), "+r"(d[1][u]), "+r"(d[2][u]), "+r"(d[3][u])
        : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(b0), "r"(b1));
    }
  }
  int32_t acc = 0;
  #pragma unroll
  for (int u = 0; u < UNROLL; ++u) acc += d[0][u] ^ d[1][u] ^ d[2][u] ^ d[3][u];
  if (acc == 0x7eadbeef) out[threadIdx.x] = acc;  // practically never true; keeps the work alive
}

__global__ void qmma_kernel(long iters, uint32_t seed, float* out) {
  uint32_t a0 = seed ^ threadIdx.x, a1 = seed * 3u, a2 = seed * 5u, a3 = seed * 7u;
  uint32_t b0 = seed * 11u, b1 = seed * 13u;
  float d[4][UNROLL];
  #pragma unroll
  for (int u = 0; u < UNROLL; ++u) { d[0][u] = 0.f; d[1][u] = 0.f; d[2][u] = 0.f; d[3][u] = 0.f; }
  for (long i = 0; i < iters; ++i) {
    #pragma unroll
    for (int u = 0; u < UNROLL; ++u) {
      asm volatile("mma.sync.aligned.m16n8k32.row.col.kind::f8f6f4.f32.e4m3.e4m3.f32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
        : "+f"(d[0][u]), "+f"(d[1][u]), "+f"(d[2][u]), "+f"(d[3][u])
        : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(b0), "r"(b1));
    }
  }
  float acc = 0.f;
  #pragma unroll
  for (int u = 0; u < UNROLL; ++u) acc += d[0][u] + d[1][u] + d[2][u] + d[3][u];
  if (acc == 12345.678f) out[threadIdx.x] = acc;
}

struct ClockSampler {
  std::atomic<bool> run{true}; std::atomic<long> sum{0}; std::atomic<long> n{0}; std::atomic<long> maxw{0};
  nvmlDevice_t dev; std::thread th;
  void start() { th = std::thread([this]{ while (run.load()) { unsigned c = 0, p = 0; if (nvmlDeviceGetClockInfo(dev, NVML_CLOCK_SM, &c) == NVML_SUCCESS) { sum += c; n += 1; } if (nvmlDeviceGetPowerUsage(dev, &p) == NVML_SUCCESS) { long w = p; long m = maxw.load(); while (w > m && !maxw.compare_exchange_weak(m, w)) {} } std::this_thread::sleep_for(std::chrono::milliseconds(50)); } }); }
  void stop() { run = false; th.join(); }
  double avg_mhz() const { return n.load() ? double(sum.load()) / double(n.load()) : 0.0; }
};

template <typename F>
static void run_test(const char* name, F launch, int sms, double seconds, nvmlDevice_t dev, double macs_per_mma) {
  // Calibrate iterations so that the kernel runs ~`seconds`.
  long iters = 1 << 14; float ms = 0.f;
  cudaEvent_t ev0, ev1; CK(cudaEventCreate(&ev0)); CK(cudaEventCreate(&ev1));
  for (;;) {
    CK(cudaEventRecord(ev0)); launch(iters); CK(cudaEventRecord(ev1)); CK(cudaEventSynchronize(ev1)); CK(cudaEventElapsedTime(&ms, ev0, ev1));
    if (ms > 300.f) break; iters *= 4;
  }
  iters = (long)(iters * (seconds * 1000.0 / ms));
  ClockSampler cs; cs.dev = dev; cs.start();
  CK(cudaEventRecord(ev0)); launch(iters); CK(cudaEventRecord(ev1)); CK(cudaEventSynchronize(ev1)); CK(cudaEventElapsedTime(&ms, ev0, ev1));
  cs.stop();
  const int warps = 8 * 4 * sms;  // 8 warps per block, 4 blocks per SM
  double mmas = double(iters) * UNROLL * warps;
  double macs = mmas * macs_per_mma; double sec = ms / 1000.0;
  double mac_s = macs / sec; double tops = 2.0 * mac_s / 1e12;
  double clk = cs.avg_mhz(); double mac_clk_sm = clk > 0 ? mac_s / (clk * 1e6) / sms : 0.0;
  printf("%-6s  %7.1f T-MAC/s  %7.1f TOPS  sm_clk %6.0f MHz  %6.0f MAC/clk/SM  maxW %.0f  (%.2f s)\n",
         name, mac_s / 1e12, tops, clk, mac_clk_sm, cs.maxw.load() / 1000.0, sec);
}

int main(int argc, char** argv) {
  double seconds = argc > 1 ? atof(argv[1]) : 3.0;
  int dev = 0; cudaDeviceProp p; CK(cudaGetDeviceProperties(&p, dev));
  if (nvmlInit() != NVML_SUCCESS) { fprintf(stderr, "nvml init failed\n"); return 1; }
  nvmlDevice_t nd; nvmlDeviceGetHandleByIndex(0, &nd);
  printf("device: %s  SMs %d  cc %d.%d\n", p.name, p.multiProcessorCount, p.major, p.minor);
  int32_t* oi; float* of; CK(cudaMalloc(&oi, 4096)); CK(cudaMalloc(&of, 4096));
  dim3 grid(4 * p.multiProcessorCount), block(256);
  run_test("IMMA8", [&](long it){ imma_kernel<<<grid, block>>>(it, 0x1234567u, oi); }, p.multiProcessorCount, seconds, nd, 16.0 * 8 * 32);
  run_test("QMMA8", [&](long it){ qmma_kernel<<<grid, block>>>(it, 0x1234567u, of); }, p.multiProcessorCount, seconds, nd, 16.0 * 8 * 32);
  nvmlShutdown(); return 0;
}
