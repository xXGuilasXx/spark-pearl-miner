// Host-side interfaces shared by the .cu files of libspm_cuda (not part of the C ABI).
#pragma once

#include <cstddef>
#include <cstdint>

#include <cuda_runtime.h>

#include "noise_hash.cuh"

namespace spm {

// ---- prep (cuda/prep/prep.cu) ----------------------------------------------------------------

/// Fills `rows * k` entries of the int7 stream seeded with `seed ^ domain`
/// (spm_cpuref::fill_int7). `rows * k` must be a multiple of 16.
cudaError_t launch_fill_int7(int8_t* out, uint64_t seed, uint64_t domain, uint64_t rows, uint32_t k,
                             cudaStream_t stream);

/// Uniform factor (A_L or B_Rᵀ): `rows` rows of 128 entries in [-32, 31], row i made of the
/// slot-0 noise hashes 4i .. 4i+3.
cudaError_t launch_uniform_factor(int8_t* out, uint64_t rows, const Words8& key, const Words8& label,
                                  cudaStream_t stream);

/// Permutation pairs (A_R or B_L): k pairs (p, q) stored as 2k bytes, pair l from word l % 8 of
/// the slot-1 noise hash l / 8.
cudaError_t launch_perm_pairs(uint8_t* pairs, uint32_t k, const Words8& key, const Words8& label,
                              cudaStream_t stream);

/// out[i][l] = in[i][l] + factor[i][p(l)] - factor[i][q(l)] for `rows` rows of `k` entries.
/// `in` and `out` may alias (in-place).
cudaError_t launch_apply_noise(const int8_t* in, int8_t* out, const int8_t* factor,
                               const uint8_t* pairs, uint64_t rows, uint32_t k, cudaStream_t stream);

// ---- fused GEMM + transcript + digest (cuda/gemm/gemm_v0_cpasync.cu) --------------------------

/// One hit of the ring: tile base and its digest (8 LE words).
struct DeviceHit {
  uint32_t t_rows;
  uint32_t t_cols;
  uint32_t digest[8];
};

struct GemmArgs {
  const int8_t* a;   // A' (m x k, row major)
  const int8_t* bt;  // B'ᵀ (n x k, row major)
  uint32_t m, n, k;
  uint32_t key[8];    // a_noise_seed as LE words (the jackpot hash key)
  uint32_t bound[8];  // LE U256 words
  uint32_t* hit_count;
  DeviceHit* hits;
  uint32_t hit_capacity;
  uint8_t* dump;  // m*n/128 records of 104 bytes, or nullptr
};

struct GemmGeometry {
  uint32_t block_m, block_n;  // CTA tile
  uint32_t tiles_m, tiles_n;  // CTA tiles over the output
  uint32_t ctas_per_sm;       // resident CTAs per SM (occupancy)
  uint32_t smem_bytes;        // dynamic shared memory per CTA
};

/// Geometry of the v0 kernel for an m x n output (validates nothing).
GemmGeometry gemm_v0_geometry(uint32_t m, uint32_t n);

/// Sets the kernel attributes (dynamic smem opt-in) once per process and reports the occupancy.
cudaError_t gemm_v0_prepare(uint32_t* ctas_per_sm);

/// Launches CTA tiles [tile_begin, tile_begin + tile_count) of the fused kernel.
cudaError_t launch_gemm_v0(const GemmArgs& args, uint32_t tile_begin, uint32_t tile_count,
                           bool dump, cudaStream_t stream);

}  // namespace spm
