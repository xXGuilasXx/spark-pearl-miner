// Host-side proof that the TiledMMA of cuda/gemm/hash_gemm_config.cuh gives every thread exactly
// one hash tile, before the kernel's transcript fold relies on it.
//
// For each of the 128 threads of the CTA it partitions an identity tensor over the 128 x 128 C
// tile with the same TiledMMA the kernel uses and checks that:
//   * the thread owns 128 accumulators;
//   * their coordinates are rows r0 + {0, 8, .., 56} x cols c0 + {0, 1, 8, 9, .., 56, 57} with
//     r0 = 64*(warp % 2) + lane/4 and c0 = 64*(warp / 2) + 2*(lane % 4) (a valid tile base:
//     r0 mod 64 < 8, c0 mod 64 even and < 8);
//   * accumulator 0 sits at (r0, c0), which the epilogue uses as the tile base;
//   * the 128 threads cover the C tile exactly once.
// It also checks that the A/B register fragments cover the whole 128 x 32 / 128 x 32 operand
// slabs exactly once per k-block.
//
// Build and run (no GPU needed):
//   nvcc -std=c++17 --expt-relaxed-constexpr -I third_party/cutlass/include -I cuda/include \
//        -I cuda/common -o layout_check cuda/tests/layout_check.cu && ./layout_check [--latex]
#include <cstdio>
#include <cstring>
#include <set>
#include <utility>
#include <vector>

#include "../gemm/hash_gemm_config.cuh"

using namespace cute;
using Cfg = spm::gemm::HashGemmConfig<spm::gemm::Int8Policy, 3>;

static int fail(const char* what, int tid, int i) {
  std::printf("FAIL: %s (thread %d, value %d)\n", what, tid, i);
  return 1;
}

int main(int argc, char** argv) {
  typename Cfg::TiledMma mma;
  const bool latex = argc > 1 && std::strcmp(argv[1], "--latex") == 0;
  if (latex) {
    print_latex(mma);
    return 0;
  }
  print(mma);
  std::printf("\n");

  std::vector<int> cover(Cfg::kBM * Cfg::kBN, 0);
  auto cC = make_identity_tensor(make_shape(typename Cfg::BM{}, typename Cfg::BN{}));
  for (int tid = 0; tid < Cfg::kThreads; ++tid) {
    auto thr = mma.get_slice(tid);
    auto tCcC = thr.partition_C(cC);
    if (size(tCcC) != 128) return fail("accumulator count != 128", tid, -1);
    const int warp = tid / 32, lane = tid % 32;
    const int r0 = 64 * (warp % 2) + lane / 4;
    const int c0 = 64 * (warp / 2) + 2 * (lane % 4);
    if (get<0>(tCcC(0)) != r0 || get<1>(tCcC(0)) != c0)
      return fail("accumulator 0 is not the tile base", tid, 0);
    std::set<std::pair<int, int>> expected, got;
    for (int u = 0; u < 8; ++u)
      for (int v = 0; v < 16; ++v) expected.insert({r0 + 8 * u, c0 + (v / 2) * 8 + (v % 2)});
    for (int i = 0; i < size(tCcC); ++i) {
      const int r = get<0>(tCcC(i)), c = get<1>(tCcC(i));
      got.insert({r, c});
      cover[r * Cfg::kBN + c] += 1;
    }
    if (got != expected) return fail("coordinates are not one hash tile", tid, -1);
  }
  for (int x : cover)
    if (x != 1) return fail("C tile not covered exactly once", -1, -1);

  // Operand fragments: per k-block (32 wide) each thread's A fragment spans the warp's 64 rows,
  // each B fragment the warp's 64 columns, and the threads of a warp together cover them once.
  auto cA = make_identity_tensor(make_shape(typename Cfg::BM{}, Int<32>{}));
  auto cB = make_identity_tensor(make_shape(typename Cfg::BN{}, Int<32>{}));
  for (int warp = 0; warp < 4; ++warp) {
    std::vector<int> ca(Cfg::kBM * 32, 0), cb(Cfg::kBN * 32, 0);
    for (int lane = 0; lane < 32; ++lane) {
      auto thr = mma.get_slice(warp * 32 + lane);
      auto tA = thr.partition_A(cA);
      auto tB = thr.partition_B(cB);
      for (int i = 0; i < size(tA); ++i) {
        const int r = get<0>(tA(i));
        if (r / 64 != warp % 2) return fail("A fragment row outside the warp's rows", warp * 32 + lane, i);
        ca[r * 32 + get<1>(tA(i))] += 1;
      }
      for (int i = 0; i < size(tB); ++i) {
        const int c = get<0>(tB(i));
        if (c / 64 != warp / 2) return fail("B fragment col outside the warp's cols", warp * 32 + lane, i);
        cb[c * 32 + get<1>(tB(i))] += 1;
      }
    }
    for (int r = 0; r < Cfg::kBM; ++r)
      for (int k = 0; k < 32; ++k)
        if (ca[r * 32 + k] != (r / 64 == warp % 2 ? 1 : 0)) return fail("A slab coverage", warp, r);
    for (int c = 0; c < Cfg::kBN; ++c)
      for (int k = 0; k < 32; ++k)
        if (cb[c * 32 + k] != (c / 64 == warp / 2 ? 1 : 0)) return fail("B slab coverage", warp, c);
  }

  std::printf("OK: 128 threads x 128 accumulators, one 8x16 hash tile per thread, C covered once;\n"
              "    A/B fragments cover each warp's 64-row / 64-col slabs once per k-block.\n"
              "    smem per CTA: %d bytes (%d stages)\n",
              Cfg::kSmemBytes, Cfg::kStages);
  return 0;
}
