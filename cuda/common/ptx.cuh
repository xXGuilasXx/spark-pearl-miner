// Thin inline-PTX wrappers used by the kernels: shared-memory addresses, mbarrier, TMA
// (cp.async.bulk.tensor), setmaxnreg, ldmatrix. Checked on the GB10 (sm_121a, CUDA 13.0); the TMA
// swizzle layout the ldmatrix addressing relies on is verified by cuda/probes/tma_swizzle.cu.
#pragma once
#include <cuda.h>
#include <stdint.h>

namespace spm {
namespace ptx {

__device__ __forceinline__ uint32_t smem_addr(const void* p) {
  return static_cast<uint32_t>(__cvta_generic_to_shared(p));
}

// ---- mbarrier ------------------------------------------------------------------------------

__device__ __forceinline__ void mbar_init(uint32_t bar, uint32_t count) {
  asm volatile("mbarrier.init.shared::cta.b64 [%0], %1;" ::"r"(bar), "r"(count) : "memory");
}

/// Makes initialized mbarriers visible to the async proxy (TMA) and the other threads.
__device__ __forceinline__ void mbar_fence_init() {
  asm volatile("fence.mbarrier_init.release.cluster;" ::: "memory");
}

/// Plain arrival (release, CTA scope).
__device__ __forceinline__ void mbar_arrive(uint32_t bar) {
  asm volatile("mbarrier.arrive.shared::cta.b64 _, [%0];" ::"r"(bar) : "memory");
}

/// Arrival that also announces `bytes` of pending async-proxy transactions (TMA) on this phase.
__device__ __forceinline__ void mbar_arrive_expect_tx(uint32_t bar, uint32_t bytes) {
  asm volatile("mbarrier.arrive.expect_tx.shared::cta.b64 _, [%0], %1;" ::"r"(bar), "r"(bytes)
               : "memory");
}

/// Blocks until the phase with parity `parity` has completed (acquire, CTA scope).
__device__ __forceinline__ void mbar_wait(uint32_t bar, uint32_t parity) {
  asm volatile(
      "{\n\t"
      ".reg .pred done;\n\t"
      "SPM_WAIT_%=:\n\t"
      "mbarrier.try_wait.parity.shared::cta.b64 done, [%0], %1;\n\t"
      "@!done bra SPM_WAIT_%=;\n\t"
      "}" ::"r"(bar),
      "r"(parity)
      : "memory");
}

// ---- TMA ----------------------------------------------------------------------------------

/// 2-D tiled bulk tensor load global -> shared, completing `bar`'s transaction count.
/// `c0` is the innermost (contiguous) coordinate, `c1` the row.
///
/// The destination is spelled `.shared::cta` (the sm_120-family form). With `.shared::cluster`,
/// ptxas for sm_121a guards the instruction with a runtime check and a call to
/// `__cuda_syscall_cp_async_bulk_tensor_2d_tile_unicast`, and that extern call makes it ignore
/// setmaxnreg for the whole kernel (warning C7506).
__device__ __forceinline__ void tma_load_2d(uint32_t dst, const CUtensorMap* map, int32_t c0,
                                            int32_t c1, uint32_t bar) {
  asm volatile(
      "cp.async.bulk.tensor.2d.shared::cta.global.mbarrier::complete_tx::bytes"
      " [%0], [%1, {%2, %3}], [%4];" ::"r"(dst),
      "l"(reinterpret_cast<uint64_t>(map)), "r"(c0), "r"(c1), "r"(bar)
      : "memory");
}

/// Prefetches a 2-D box into L2 only (no shared-memory destination, no completion tracking).
__device__ __forceinline__ void tma_prefetch_l2_2d(const CUtensorMap* map, int32_t c0, int32_t c1) {
  asm volatile("cp.async.bulk.prefetch.tensor.2d.L2.global.tile [%0, {%1, %2}];" ::"l"(
                   reinterpret_cast<uint64_t>(map)),
               "r"(c0), "r"(c1)
               : "memory");
}

__device__ __forceinline__ void tma_prefetch_descriptor(const CUtensorMap* map) {
  asm volatile("prefetch.tensormap [%0];" ::"l"(reinterpret_cast<uint64_t>(map)) : "memory");
}

// ---- register reallocation between warpgroups ----------------------------------------------

/// Lowers this warpgroup's register budget to kRegs per thread (all 4 warps must execute it).
template <uint32_t kRegs>
__device__ __forceinline__ void setmaxnreg_dec() {
  asm volatile("setmaxnreg.dec.sync.aligned.u32 %0;" ::"n"(kRegs) : "memory");
}

/// Raises this warpgroup's register budget to kRegs per thread (waits for released registers).
template <uint32_t kRegs>
__device__ __forceinline__ void setmaxnreg_inc() {
  asm volatile("setmaxnreg.inc.sync.aligned.u32 %0;" ::"n"(kRegs) : "memory");
}

// ---- ldmatrix -----------------------------------------------------------------------------

/// Four 8x8 b16 matrices (each 8 rows x 16 bytes): lanes 8j..8j+7 give the row addresses of
/// matrix j; lane l receives row l/4, bytes 4(l%4)..4(l%4)+3 of every matrix.
__device__ __forceinline__ void ldmatrix_x4(uint32_t& r0, uint32_t& r1, uint32_t& r2, uint32_t& r3,
                                            uint32_t addr) {
  asm volatile("ldmatrix.sync.aligned.m8n8.x4.shared.b16 {%0, %1, %2, %3}, [%4];"
               : "=r"(r0), "=r"(r1), "=r"(r2), "=r"(r3)
               : "r"(addr));
}

// ---- misc ---------------------------------------------------------------------------------

__device__ __forceinline__ uint32_t rotl32(uint32_t x, uint32_t n) {
  return __funnelshift_l(x, x, n);
}

/// a ^ b ^ c in one LOP3.
__device__ __forceinline__ uint32_t xor3(uint32_t a, uint32_t b, uint32_t c) {
  uint32_t d;
  asm("lop3.b32 %0, %1, %2, %3, 0x96;" : "=r"(d) : "r"(a), "r"(b), "r"(c));
  return d;
}

}  // namespace ptx
}  // namespace spm
