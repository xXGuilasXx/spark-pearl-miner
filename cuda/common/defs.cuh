// Shared macros of the CUDA sources.
#pragma once

// Functions usable from host and device code (constants, hashing, comparisons).
#if defined(__CUDACC__)
#define SPM_HD __host__ __device__ __forceinline__
#else
#define SPM_HD inline
#endif
