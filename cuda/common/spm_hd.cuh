// Host/device function qualifiers shared by the header-only helpers in cuda/common.
#pragma once

#if defined(__CUDACC__)
#define SPM_HD __host__ __device__ __forceinline__
#define SPM_DEVICE __device__ __forceinline__
#else
#define SPM_HD inline
#define SPM_DEVICE inline
#endif
