// C ABI between the Rust gpu-worker and libspm_cuda (CUDA C++). Keep this header dependency-free.
#pragma once
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  char name[64];
  int32_t sm_count;
  int32_t cc_major;
  int32_t cc_minor;
  int32_t max_smem_optin_bytes;   // per block, opt-in
  int32_t regs_per_sm;
  int32_t sm_clock_khz;           // current
  int64_t total_mem_bytes;
} spm_device_info_t;

// Fills `out` for device 0. Returns 0 on success, a CUDA error code otherwise.
int32_t spm_cuda_device_info(spm_device_info_t* out);

// Runs the register-only IMMA peak probe for ~`seconds` and returns T-MAC/s (0 on failure).
double spm_cuda_imma_peak(double seconds);

// Library version string (static).
const char* spm_cuda_version(void);

#ifdef __cplusplus
}
#endif
