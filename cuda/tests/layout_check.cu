// Host-side proof of the fragment mapping of the fused kernel (cuda/gemm/gemm_tma.cuh), which the
// transcript fold relies on: every lane's 128 accumulators are exactly one 8 x 16 hash tile.
//
// It runs the kernel's own lane geometry (the __host__ __device__ helpers of gemm_tma.cuh:
// ldsm_offsets, lane_row / lane_col, dump_index, tile_coords, rotate_right_if, OPERAND_SWIZZLE)
// through models of the hardware taken from outside the kernel:
//   * the TMA SWIZZLE_64B shared-memory layout as CuTe's Swizzle<2,4,3>, tied to the descriptor
//     the job encodes (OPERAND_SWIZZLE) through CuTe's own swizzle -> CUtensorMapSwizzle mapping;
//   * ldmatrix.sync.m8n8.x4.b16 as the PTX ISA defines it (lanes 8j..8j+7 give the row addresses
//     of matrix j; lane l receives bytes 4(l%4)..4(l%4)+3 of row l/4 of every matrix);
//   * the A / B / C fragment layouts of mma.sync.m16n8k32 from CuTe's MMA_Traits:
//     SM80_16x8x32_S32S8S8S32_TN for MmaS8 and SM120_16x8x32_TN<e4m3, e4m3, f32> for MmaE4M3.
// For both policies it checks, per warp and lane:
//   * every ldmatrix row address is 16-byte aligned and each 8-lane phase hits 8 distinct 16-byte
//     bank groups (no shared-memory bank conflicts);
//   * every byte of a[i][r] / b[j][r] is exactly the (row, k) element of the stage the MMA atom
//     expects for fragment (i, ks) / (j, ks), so acc[i][j][v] accumulates C(16i + m, 8j + n) with
//     (m, n) = CLayout(lane, v), and each warp loads its 64-row A' and B'ᵀ slabs once per stage;
//   * the 128 accumulators acc[0..3][0..7][0..3] of a lane are rows lane_row + {0, 8, .., 56} x
//     cols lane_col + {0, 1, 8, 9, .., 56, 57}: one hash tile (base row mod 64 < 8, base col mod 64
//     even and < 8), and the 256 lanes of the 8 MMA warps cover the 128 x 256 CTA tile once.
// Then, independent of the policy: dump_index is the reference order (t_rows, then t_cols) over
// whole problems with partial CTA tiles, tile_coords visits every CTA tile exactly once for many
// shapes and band heights, and the kernel's transcript queue plus rotate_right_if put slot s at
// word s for every slice count.
//
// Build and run (no GPU needed; `cargo test -p spm-gpu --test layout` does the same), from the
// repository root:
//   nvcc -std=c++17 -I third_party/cutlass/include -I cuda/include
//        -gencode arch=compute_121a,code=sm_121a -o layout_check cuda/tests/layout_check.cu
//   ./layout_check
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <set>
#include <type_traits>
#include <utility>
#include <vector>

#include <cute/atom/copy_traits_sm90_tma_swizzle.hpp>
#include <cute/atom/mma_traits_sm120.hpp>
#include <cute/atom/mma_traits_sm80.hpp>
#include <cute/swizzle.hpp>

#include "../gemm/gemm_tma.cuh"

namespace g = spm::gemm;

namespace {

int g_failures = 0;

#define CHECK(cond, ...)                         \
  do {                                           \
    if (!(cond)) {                               \
      if (g_failures < 20) {                     \
        std::printf("FAIL %s:%d: ", __FILE__, __LINE__); \
        std::printf(__VA_ARGS__);                \
        std::printf("\n");                       \
      }                                          \
      ++g_failures;                              \
    }                                            \
  } while (0)

// ---- hardware models ---------------------------------------------------------------------

/// TMA SWIZZLE_64B as a CuTe swizzle: 16-byte chunk bits [4, 6) XOR address bits [7, 9).
using Sw64 = cute::Swizzle<2, 4, 3>;

/// Logical (operand, row, k byte) of a physical stage offset. The swizzle is an involution and
/// keeps bits >= 6 (the row), so the operand and the row read the same before and after it.
struct Elem {
  int operand;  // 0 = A' (rows 0..BM-1), 1 = B'ᵀ (rows 0..BN-1)
  uint32_t row;
  uint32_t k;
};

Elem logical(uint32_t phys) {
  Elem e;
  e.operand = phys < g::A_STAGE_BYTES ? 0 : 1;
  const uint32_t rel = e.operand == 0 ? phys : phys - g::A_STAGE_BYTES;
  const uint32_t l = static_cast<uint32_t>(Sw64{}(static_cast<int>(rel)));
  e.row = l / g::BK;
  e.k = l % g::BK;
  return e;
}

/// ldmatrix.x4: `addr[t]` is the row address lane t provides. Byte b of register r of lane l is
/// byte 4 (l % 4) + b of row l / 4 of matrix r, whose row address comes from lane 8r + l / 4.
uint32_t ldsm_byte_addr(const uint32_t (&addr)[32], uint32_t lane, uint32_t reg, uint32_t byte) {
  return addr[8 * reg + lane / 4] + 4 * (lane % 4) + byte;
}

void check_ldsm_phase(const uint32_t (&addr)[32], const char* what) {
  for (uint32_t r = 0; r < 4; ++r) {
    std::set<uint32_t> banks;
    for (uint32_t t = 8 * r; t < 8 * r + 8; ++t) {
      CHECK(addr[t] % 16 == 0, "%s: ldmatrix row address %u not 16-byte aligned", what, addr[t]);
      banks.insert((addr[t] / 16) % 8);
    }
    CHECK(banks.size() == 8, "%s: matrix %u rows share a 16-byte bank group", what, r);
  }
}

// ---- MMA atoms of the policies ------------------------------------------------------------

template <class Mma>
struct Atom;
template <>
struct Atom<g::MmaS8> {
  using Op = cute::SM80_16x8x32_S32S8S8S32_TN;
  static constexpr const char* name = "MmaS8 (mma.sync m16n8k32 s8.s8.s32)";
};
template <>
struct Atom<g::MmaE4M3> {
  using Op = cute::SM120_16x8x32_TN<cutlass::float_e4m3_t, cutlass::float_e4m3_t, float>;
  static constexpr const char* name = "MmaE4M3 (mma.sync m16n8k32 kind::f8f6f4 e4m3.e4m3.f32)";
};

template <class Mma>
void check_policy() {
  using Op = typename Atom<Mma>::Op;
  using Traits = cute::MMA_Traits<Op>;
  using Acc = typename Mma::Acc;
  // The policy's mma() takes the operation's register arrays: 4 A words, 2 B words, 4 accumulators.
  static_assert(std::extent<typename Op::ARegisters>::value == 4, "A registers");
  static_assert(std::extent<typename Op::BRegisters>::value == 2, "B registers");
  static_assert(std::extent<typename Op::DRegisters>::value == 4, "D registers");
  static_assert(sizeof(typename Op::DRegisters) == 4 * sizeof(Acc), "accumulator width");
  static_assert(std::is_same<typename Traits::ValTypeD, Acc>::value, "accumulator type");
  static_assert(std::is_same<decltype(&Mma::mma),
                             void (*)(Acc (&)[4], const uint32_t (&)[4], uint32_t, uint32_t)>::value,
                "policy mma() signature");
  using namespace cute;
  static_assert(size(typename Traits::ThrID{}) == 32, "one warp");
  static_assert(size<0>(typename Traits::Shape_MNK{}) == 16 && size<1>(typename Traits::Shape_MNK{}) == 8 &&
                    size<2>(typename Traits::Shape_MNK{}) == 32,
                "m16n8k32");
  const typename Traits::ALayout al{};
  const typename Traits::BLayout bl{};
  const typename Traits::CLayout cl{};
  static_assert(size<1>(typename Traits::ALayout{}) == 16, "16 A bytes per lane");
  static_assert(size<1>(typename Traits::BLayout{}) == 8, "8 B bytes per lane");
  static_assert(size<1>(typename Traits::CLayout{}) == 4, "4 accumulators per lane");

  const uint32_t before = g_failures;
  std::vector<int> c_cover(g::BM * g::BN, 0);
  for (uint32_t warp = 0; warp < static_cast<uint32_t>(g::MMA_WARPS); ++warp) {
    const uint32_t wm = g::warp_m(warp), wn = g::warp_n(warp);
    CHECK(wm < g::BM / g::WARP_TILE && wn < g::BN / g::WARP_TILE, "warp %u outside the CTA tile", warp);
    g::LdsmOffsets off[32];
    for (uint32_t l = 0; l < 32; ++l) off[l] = g::ldsm_offsets(wm, wn, l);
    std::vector<int> a_cover(g::BM * g::BK, 0), b_cover(g::BN * g::BK, 0);

    for (uint32_t ks = 0; ks < 2; ++ks) {
      // A' fragments: ldmatrix.x4 at off.a[ks] + i * 16 * BK (the kernel's mainloop).
      for (uint32_t i = 0; i < g::FRAGS_M; ++i) {
        uint32_t addr[32];
        for (uint32_t t = 0; t < 32; ++t) addr[t] = off[t].a[ks] + i * 16 * g::BK;
        check_ldsm_phase(addr, "A");
        for (uint32_t l = 0; l < 32; ++l)
          for (uint32_t r = 0; r < 4; ++r)
            for (uint32_t b = 0; b < 4; ++b) {
              const Elem e = logical(ldsm_byte_addr(addr, l, r, b));
              const int idx = al(static_cast<int>(l), static_cast<int>(4 * r + b));  // (m, k) col-major 16 x 32
              const uint32_t m = idx % 16, k = idx / 16;
              CHECK(e.operand == 0 && e.row == wm * g::WARP_TILE + 16 * i + m && e.k == 32 * ks + k,
                    "A warp %u lane %u frag %u ks %u reg %u byte %u: got (%d, %u, %u), atom wants row %u k %u",
                    warp, l, i, ks, r, b, e.operand, e.row, e.k, wm * g::WARP_TILE + 16 * i + m, 32 * ks + k);
              if (e.operand == 0 && e.row < g::BM) a_cover[e.row * g::BK + e.k] += 1;
            }
      }
      // B'ᵀ fragments: pair jp at off.b[ks] + jp * 16 * BK gives b[2jp][0], b[2jp][1] (matrices 0, 1)
      // and b[2jp + 1][0], b[2jp + 1][1] (matrices 2, 3).
      for (uint32_t jp = 0; jp < g::FRAGS_N / 2; ++jp) {
        uint32_t addr[32];
        for (uint32_t t = 0; t < 32; ++t) addr[t] = off[t].b[ks] + jp * 16 * g::BK;
        check_ldsm_phase(addr, "B");
        for (uint32_t l = 0; l < 32; ++l)
          for (uint32_t mat = 0; mat < 4; ++mat) {
            const uint32_t j = 2 * jp + mat / 2, r = mat % 2;
            for (uint32_t b = 0; b < 4; ++b) {
              const Elem e = logical(ldsm_byte_addr(addr, l, mat, b));
              const int idx = bl(static_cast<int>(l), static_cast<int>(4 * r + b));  // (n, k) col-major 8 x 32
              const uint32_t n = idx % 8, k = idx / 8;
              CHECK(e.operand == 1 && e.row == wn * g::WARP_TILE + 8 * j + n && e.k == 32 * ks + k,
                    "B warp %u lane %u frag %u ks %u reg %u byte %u: got (%d, %u, %u), atom wants row %u k %u",
                    warp, l, j, ks, r, b, e.operand, e.row, e.k, wn * g::WARP_TILE + 8 * j + n, 32 * ks + k);
              if (e.operand == 1 && e.row < g::BN) b_cover[e.row * g::BK + e.k] += 1;
            }
          }
      }
    }
    // Each warp reads its own 64-row slabs of the stage exactly once, and nothing else.
    for (uint32_t r = 0; r < g::BM; ++r)
      for (uint32_t k = 0; k < g::BK; ++k)
        CHECK(a_cover[r * g::BK + k] == (r / g::WARP_TILE == wm ? 1 : 0), "A slab coverage warp %u row %u k %u", warp, r, k);
    for (uint32_t r = 0; r < g::BN; ++r)
      for (uint32_t k = 0; k < g::BK; ++k)
        CHECK(b_cover[r * g::BK + k] == (r / g::WARP_TILE == wn ? 1 : 0), "B slab coverage warp %u row %u k %u", warp, r, k);

    // Accumulators: acc[i][j][v] holds C(16i + m, 8j + n) of the warp tile, (m, n) = CLayout(l, v).
    for (uint32_t l = 0; l < 32; ++l) {
      std::set<std::pair<uint32_t, uint32_t>> got, want;
      for (uint32_t i = 0; i < g::FRAGS_M; ++i)
        for (uint32_t j = 0; j < g::FRAGS_N; ++j)
          for (uint32_t v = 0; v < 4; ++v) {
            const int idx = cl(static_cast<int>(l), static_cast<int>(v));  // (m, n) col-major 16 x 8
            got.insert({16 * i + idx % 16, 8 * j + idx / 16});
          }
      const uint32_t r0 = g::lane_row(l), c0 = g::lane_col(l);
      for (uint32_t u = 0; u < 8; ++u)
        for (uint32_t w = 0; w < 16; ++w) want.insert({r0 + 8 * u, c0 + 8 * (w / 2) + w % 2});
      CHECK(got.size() == 128, "warp %u lane %u: %zu distinct accumulator positions", warp, l, got.size());
      CHECK(got == want, "warp %u lane %u: accumulators are not the hash tile (%u, %u)", warp, l, r0, c0);
      CHECK(r0 % 64 < 8 && c0 % 64 < 8 && c0 % 2 == 0, "lane %u: (%u, %u) is not a hash-tile base", l, r0, c0);
      for (const auto& rc : got) {
        const uint32_t row = wm * g::WARP_TILE + rc.first, col = wn * g::WARP_TILE + rc.second;
        if (row < g::BM && col < g::BN) c_cover[row * g::BN + col] += 1;
      }
    }
  }
  for (uint32_t x = 0; x < g::BM * g::BN; ++x)
    CHECK(c_cover[x] == 1, "CTA tile element (%u, %u) covered %d times", x / g::BN, x % g::BN, c_cover[x]);
  if (g_failures == before)
    std::printf("OK %s: 8 warps x 32 lanes, each lane's 4 x 8 x 4 = 128 accumulators are one 8 x 16 hash\n"
                "   tile, CTA tile covered once; ldmatrix through SWIZZLE_64B feeds every A/B register the\n"
                "   (row, k) the atom expects, each warp reads its 64-row slabs once, no bank conflicts\n",
                Atom<Mma>::name);
}

// ---- policy-independent checks -----------------------------------------------------------

/// dump_index is the rank of (t_rows, t_cols) in the reference order, over every hash tile of an
/// m x n problem (active warp tiles of every CTA tile, partial CTA tiles included).
void check_dump_order(uint32_t m, uint32_t n) {
  const uint32_t before = g_failures;
  const uint32_t tiles_m = (m + g::BM - 1) / g::BM, tiles_n = (n + g::BN - 1) / g::BN;
  std::vector<std::pair<uint32_t, uint32_t>> at(static_cast<size_t>(m) * n / 128, {~0u, ~0u});
  uint64_t written = 0;
  for (uint32_t tm = 0; tm < tiles_m; ++tm)
    for (uint32_t tn = 0; tn < tiles_n; ++tn)
      for (uint32_t warp = 0; warp < static_cast<uint32_t>(g::MMA_WARPS); ++warp) {
        const uint32_t row0 = tm * g::BM + g::warp_m(warp) * g::WARP_TILE;
        const uint32_t col0 = tn * g::BN + g::warp_n(warp) * g::WARP_TILE;
        if (!(row0 < m && col0 < n)) continue;  // the kernel's `active`
        for (uint32_t l = 0; l < 32; ++l) {
          const uint64_t idx = g::dump_index(row0, col0, l, n / 16);
          CHECK(idx < at.size(), "m=%u n=%u: dump index %llu out of range", m, n, (unsigned long long)idx);
          if (idx >= at.size()) continue;
          CHECK(at[idx].first == ~0u, "m=%u n=%u: dump index %llu written twice", m, n, (unsigned long long)idx);
          at[idx] = {row0 + g::lane_row(l), col0 + g::lane_col(l)};
          ++written;
        }
      }
  CHECK(written == at.size(), "m=%u n=%u: %llu of %zu records written", m, n, (unsigned long long)written, at.size());
  // Reference order: t_rows = 64a + b (b < 8) ascending, then t_cols = 64c + 2d (d < 4) ascending.
  size_t i = 0;
  for (uint32_t r = 0; r < m; ++r) {
    if (r % 64 >= 8) continue;
    for (uint32_t c = 0; c < n; c += 2) {
      if (c % 64 >= 8) continue;
      CHECK(i < at.size() && at[i] == std::make_pair(r, c), "m=%u n=%u: record %zu is not tile (%u, %u)", m, n, i, r, c);
      ++i;
    }
  }
  CHECK(i == at.size(), "m=%u n=%u: %zu reference tiles, %zu records", m, n, i, at.size());
  if (g_failures == before)
    std::printf("OK dump order m=%u n=%u: %zu records, each hash tile once, in reference order\n", m, n, at.size());
}

/// tile_coords is a bijection from [0, tiles_m * tiles_n) onto the CTA-tile grid.
bool raster_is_bijective(uint32_t tiles_m, uint32_t tiles_n, uint32_t band) {
  g::Params p;
  std::memset(&p, 0, sizeof(p));
  p.tiles_m = tiles_m;
  p.tiles_n = tiles_n;
  p.band = band < tiles_m ? band : tiles_m;  // the job clamps the band to tiles_m
  std::vector<uint8_t> seen(static_cast<size_t>(tiles_m) * tiles_n, 0);
  for (uint32_t t = 0; t < tiles_m * tiles_n; ++t) {
    uint32_t tm = ~0u, tn = ~0u;
    g::tile_coords(p, t, tm, tn);
    if (tm >= tiles_m || tn >= tiles_n || seen[static_cast<size_t>(tm) * tiles_n + tn]++) return false;
  }
  return true;
}

void check_raster() {
  const uint32_t before = g_failures;
  int shapes = 0;
  for (uint32_t tm : {1u, 2u, 5u, 16u, 17u, 33u, 128u, 1024u})
    for (uint32_t tn : {1u, 2u, 3u, 7u, 64u, 512u})
      for (uint32_t band : {1u, 3u, 16u, 24u, 1024u}) {
        CHECK(raster_is_bijective(tm, tn, band), "raster tiles_m=%u tiles_n=%u band=%u", tm, tn, band);
        ++shapes;
      }
  if (g_failures == before)
    std::printf("OK raster: tile_coords visits every CTA tile once (%d grid x band combinations)\n", shapes);
}

/// The transcript as the kernel keeps it: slot s mod 16 lives at t[0] during slice s (the array is
/// rotated as a queue after each slice), and rotate_right_if by S mod 16 restores slot order.
void check_transcript_slots() {
  const uint32_t before = g_failures;
  for (uint32_t slices = 1; slices <= 64; ++slices) {
    uint32_t t[16], ref[16];
    for (uint32_t i = 0; i < 16; ++i) t[i] = ref[i] = 0;
    for (uint32_t s = 0; s < slices; ++s) {
      const uint32_t fold = 0x9e3779b9u * (s + 1) ^ (s << 7);
      const auto rotl = [](uint32_t x) { return (x << g::TRANSCRIPT_ROTL) | (x >> (32 - g::TRANSCRIPT_ROTL)); };
      ref[s % 16] = rotl(ref[s % 16]) ^ fold;  // spm-cpuref: t[s mod 16] = rotl13(t[s mod 16]) ^ fold
      const uint32_t head = rotl(t[0]) ^ fold;   // consume(): update t[0], then shift the queue
      for (int i = 0; i < 15; ++i) t[i] = t[i + 1];
      t[15] = head;
    }
    const uint32_t rot = slices & 15u;
    g::rotate_right_if<1>(t, rot & 1u);
    g::rotate_right_if<2>(t, rot & 2u);
    g::rotate_right_if<4>(t, rot & 4u);
    g::rotate_right_if<8>(t, rot & 8u);
    CHECK(std::memcmp(t, ref, sizeof(t)) == 0, "transcript slot order wrong after %u slices", slices);
  }
  if (g_failures == before) std::printf("OK transcript: slot order restored for 1..64 slices\n");
}

}  // namespace

int main() {
  // The swizzle modelled here is the one the job's TMA descriptors use.
  static_assert(cute::detail::get_tma_swizzle_bits(Sw64{}) == cute::TMA::SmemSwizzleBits::B64, "Sw64 is B64");
  CHECK(cute::TMA::to_CUtensorMapSwizzle(cute::detail::get_tma_swizzle_bits(Sw64{}),
                                         cute::detail::get_tma_swizzle_base(Sw64{})) == g::OPERAND_SWIZZLE,
        "the modelled swizzle is not the descriptor's OPERAND_SWIZZLE");
  static_assert(g::STAGE_BYTES % 1024 == 0 && g::A_STAGE_BYTES % 512 == 0,
                "operand bases keep the 512-byte swizzle period");
  static_assert(g::BK == 64 && g::WARP_TILE * 2 == g::BM && g::WARP_TILE * 4 == g::BN, "geometry");
  static_assert(g::FRAGS_M * 16 == g::WARP_TILE && g::FRAGS_N * 8 == g::WARP_TILE, "fragments");

  check_policy<g::MmaS8>();
  check_policy<g::MmaE4M3>();
  for (auto mn : {std::make_pair(64u, 64u), std::make_pair(192u, 320u), std::make_pair(320u, 192u),
                  std::make_pair(256u, 512u), std::make_pair(1024u, 1024u), std::make_pair(128u, 2112u)})
    check_dump_order(mn.first, mn.second);
  check_raster();
  check_transcript_slots();
  if (g_failures != 0) {
    std::printf("layout check: %d failure(s)\n", g_failures);
    return 1;
  }
  std::printf("layout check: all passed\n");
  return 0;
}
