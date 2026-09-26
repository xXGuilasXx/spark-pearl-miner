// Tensor-core operations of the fused kernel, one policy per MMA kind.
//
// The mainloop (gemm_v0_cpasync.cuh) is templated on the policy: the shared-memory layout, the
// ldmatrix fragment loads and the accumulator layout of mma.sync m16n8k32 are identical for
// s8 x s8 -> s32 (IMMA.16832, certificate V3) and e4m3 x e4m3 -> f32 (QMMA.16832, the
// certificate-V4 fork), so only the instruction and the accumulator type change.
#pragma once

#include <cstdint>

namespace spm {
namespace mma {

/// s8 x s8 -> s32, `mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32` (SASS IMMA.16832.S8.S8).
/// Exact integer accumulation: |C'| <= 127^2 * k < 2^31 for every consensus-valid k.
struct S8S8S32 {
  using Acc = int32_t;
  static constexpr bool kImplemented = true;

  static __device__ __forceinline__ void mma(Acc (&d)[4], const uint32_t (&a)[4],
                                             const uint32_t (&b)[2]) {
    asm volatile(
        "mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, "
        "{%0,%1,%2,%3};\n"
        : "+r"(d[0]), "+r"(d[1]), "+r"(d[2]), "+r"(d[3])
        : "r"(a[0]), "r"(a[1]), "r"(a[2]), "r"(a[3]), "r"(b[0]), "r"(b[1]));
  }

  /// The accumulator as the u32 the transcript folds (two's-complement bits).
  static __device__ __forceinline__ uint32_t bits(Acc x) { return static_cast<uint32_t>(x); }
};

/// e4m3 x e4m3 -> f32, `mma.sync.aligned.m16n8k32.row.col.kind::f8f6f4.f32.e4m3.e4m3.f32`
/// (SASS QMMA.16832). Placeholder for the certificate-V4 fork (M12): the fold of f32
/// accumulators is not specified yet, so no kernel is instantiated with it.
struct E4M3E4M3F32 {
  using Acc = float;
  static constexpr bool kImplemented = false;

  static __device__ __forceinline__ void mma(Acc (&d)[4], const uint32_t (&a)[4],
                                             const uint32_t (&b)[2]) {
    asm volatile(
        "mma.sync.aligned.m16n8k32.row.col.kind::f8f6f4.f32.e4m3.e4m3.f32 {%0,%1,%2,%3}, "
        "{%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
        : "+f"(d[0]), "+f"(d[1]), "+f"(d[2]), "+f"(d[3])
        : "r"(a[0]), "r"(a[1]), "r"(a[2]), "r"(a[3]), "r"(b[0]), "r"(b[1]));
  }

  static __device__ __forceinline__ uint32_t bits(Acc x) { return __float_as_uint(x); }
};

}  // namespace mma
}  // namespace spm
