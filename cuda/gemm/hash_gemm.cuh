// Launch interface of the fused int8 GEMM + transcript + BLAKE3 + bound kernel (no CuTe here, so
// the job code that includes it compiles quickly).
#pragma once

#include <cuda_runtime.h>
#include <stdint.h>

#include "spm_cuda.h"

namespace spm {
namespace gemm {

// Everything one launch chunk needs. Passed by value as a __grid_constant__ kernel parameter.
struct HashGemmParams {
  const int8_t* a;         // A'  (m x k, row major, s8)
  const int8_t* bt;        // B'ᵀ (n x k, row major, s8)
  int32_t m, n, k;         // k is also the row stride of both operands
  int32_t slices;          // floor(k / 128): r-wide slices that enter the transcript
  int32_t tiles_m;         // m / 128
  int32_t tiles_n;         // n / 128
  int32_t total_ctas;      // tiles_m * tiles_n
  int32_t cta_base;        // first CTA tile of this chunk (the grid covers [cta_base, +grid))
  int32_t group_m;         // raster: CTA rows that advance together along n (L2 reuse)
  uint32_t key[8];         // a_noise_seed (jackpot hash key), LE words
  uint32_t bound[8];       // difficulty bound, LE U256 words
  uint8_t* dump;           // dump mode: (m*n/128) records of 104 bytes, reference tile order
  uint32_t* hit_count;     // mining mode: atomic hit counter of the attempt
  spm_hit_t* hits;         // mining mode: hit ring (slot = count % hit_capacity)
  uint32_t hit_capacity;
  // Optional chunk gate (pipelined runs): the chunk's first CTA sets *gate to 1 (run) or, when
  // *abort is already set, to 2 (skip); every CTA of the chunk follows that single decision, so a
  // chunk is either computed completely or not at all. nullptr = always run.
  uint32_t* gate;
  const volatile uint32_t* abort;  // device-visible abort flag (mapped host memory)
};

// CTA tile edge (both M and N) and smem bytes of the int8 instantiation.
constexpr int kCtaTile = 128;
int hash_gemm_smem_bytes();
int hash_gemm_stages();

// Resident CTAs per SM of the kernel (0 on error).
int hash_gemm_ctas_per_sm(bool dump);

// Launches `ctas` CTAs starting at p.cta_base.
cudaError_t launch_hash_gemm(const HashGemmParams& p, int ctas, bool dump, cudaStream_t stream);

}  // namespace gemm
}  // namespace spm
