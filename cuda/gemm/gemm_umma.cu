// GEN5 UMMA kernel instantiation and cluster launcher (gemm_umma.cuh), selected by GEN5_MINE=1.
#include "gemm_umma.cuh"

namespace spm {
namespace umma {

__global__ void __launch_bounds__(THREADS, 1)
gemm_hash_umma_kernel(const __grid_constant__ Params5 p) {
  extern __shared__ __align__(1024) uint8_t smem_raw[];
  SharedStorage5& shared = *reinterpret_cast<SharedStorage5*>(smem_raw);

  const int warp_idx = threadIdx.x >> 5;
  const uint32_t cluster_rank = block_rank_in_cluster();
  const bool is_leader = (cluster_rank == 0);

  typename MainloopPipeline::Params mainloop_params;
  mainloop_params.role = (warp_idx == 8) ? MainloopPipeline::ThreadCategory::Producer
                                         : MainloopPipeline::ThreadCategory::Consumer;
  mainloop_params.is_leader = (warp_idx == 8) && ((threadIdx.x & 31) == 0) && is_leader;
  mainloop_params.transaction_bytes = CollectiveMainloop::TmaTransactionBytes;
  mainloop_params.initializing_warp = 8;
  MainloopPipeline mainloop_pipeline(shared.pipelines.mainloop, mainloop_params, ClusterShape{},
                                     cute::true_type{}, cute::false_type{});

  typename AccumulatorPipeline::Params acc_params;
  acc_params.producer_arv_count = 1;
  acc_params.consumer_arv_count = size(AtomThrShapeMNK{}) * EPI_THREADS;
  acc_params.initializing_warp = 9;
  acc_params.role = AccumulatorPipeline::ThreadCategory::Producer;
  if (warp_idx < EPI_WARPS) acc_params.role = AccumulatorPipeline::ThreadCategory::Consumer;
  AccumulatorPipeline acc_pipeline(shared.pipelines.accumulator, acc_params, ClusterShape{});

  // The pipeline ctor delays the mask init (stock-kernel convention): explicit init_masks, or the
  // consumer_release multicast arrives with a zero mask and producer_tail hangs (wedge #8).
  mainloop_pipeline.init_masks(ClusterShape{}, cute::block_id_in_cluster());

  cutlass::arch::fence_barrier_init();
  __syncthreads();

  TileSeq5 seq{p.ctiles_m, p.ctiles_n, p.swizzle};
  const uint32_t nclusters = gridDim.x >> 1;
  const uint32_t my_cluster = blockIdx.x >> 1;

  if (warp_idx == 8) {
    // ---------------- TMA producer warp ----------------
    const int lane = threadIdx.x & 31;
    CollectiveMainloop collective(p.cutlass_params, ClusterShape{}, cluster_rank);
    auto load_inputs = collective.load_init(
        make_shape(p.m, p.n, p.k_total, Int<1>{}), shared.tensors);
    auto prod_state = cutlass::make_producer_start_state<MainloopPipeline>();
    // Abort contract: the flag is POLLED per tile and only recorded in the status word; the chunk
    // always runs to completion and exits through the exact same choreography as a normal chunk.
    // (An in-kernel early-exit drain was tried and silently corrupted the CUDA context on this
    // board: the kernel returned cudaSuccess yet every later call in the context hung. Chunk-
    // bounded abort latency (<= ~8 ms) is well inside the daemon's cancellation budget.)
    for (uint32_t ct = p.tile_begin + my_cluster; ct < p.tile_end; ct += nclusters) {
      if (*p.abort_flag != 0u) {
        if (lane == 0) atomicOr(p.status, STATUS_ABORTED);
      }
      if (p.progress != nullptr && lane == 0) *p.progress = (ct - p.tile_begin) + 1u;  // 1-based
      uint32_t tm, tn;
      seq.coords(ct, tm, tn);
      auto cta_coord = make_coord(int(tm * 2), int(tn), 0, 0);  // m in 128-row units (per CTA)
      struct KTileIter {  // int* would only advance the pointer: all slices would read k=0
        int v;
        __device__ __forceinline__ int operator*() const { return v; }
        __device__ __forceinline__ KTileIter& operator++() { ++v; return *this; }
      };
      KTileIter k_iter{0};
      prod_state = cute::get<0>(collective.load(
          mainloop_pipeline, prod_state, load_inputs, cta_coord, k_iter, int(p.k_slices)));
    }
    mainloop_pipeline.producer_tail(prod_state);
  } else if (warp_idx == 9) {
    // ---------------- MMA warp (leader CTA issues) ----------------
    cute::TMEM::Allocator2Sm tmem_allocator{};
    // tcgen05.alloc requires a single fully-active warp as the issuer.
    tmem_allocator.allocate(cute::TMEM::Sm100TmemCapacityColumns, &shared.tmem_base_ptr);
    cutlass::arch::NamedBarrier tmem_bar(EPI_THREADS + 32, cutlass::arch::ReservedNamedBarriers::TmemAllocBarrier);
    tmem_bar.arrive_and_wait();
    const uint32_t tmem_base = shared.tmem_base_ptr;

    Tensor sA = make_tensor(make_smem_ptr(shared.tensors.smem_A.begin()), SmemLayoutA{});
    Tensor sB = make_tensor(make_smem_ptr(shared.tensors.smem_B.begin()), SmemLayoutB{});
    Tensor tCrA = TiledMma::make_fragment_A(sA);
    Tensor tCrB = TiledMma::make_fragment_B(sB);

    auto acc_shape = CollectiveMainloop::partition_accumulator_shape();
    auto tAcc = cutlass::detail::make_sm100_accumulator<2, false>(
        TiledMma{}, acc_shape, make_shape(Int<128>{}, Int<256>{}));
    tAcc.data() = tmem_base;

    auto acc_state = cutlass::make_producer_start_state<AccumulatorPipeline>();
    typename MainloopPipeline::PipelineState cons_state;
    TiledMma tiled_mma;

    for (uint32_t ct = p.tile_begin + my_cluster; ct < p.tile_end; ct += nclusters) {
      if (is_leader) {
        for (uint32_t s = 0; s < p.k_slices; ++s) {
          acc_pipeline.producer_acquire(acc_state);
          Tensor acc_stage = tAcc(_, _, _, acc_state.index());
          tiled_mma.accumulate_ = UMMA::ScaleOut::Zero;
          mainloop_pipeline.consumer_wait(cons_state);
          const int read_stage = cons_state.index();
          CUTLASS_PRAGMA_UNROLL
          for (int kb = 0; kb < size<2>(tCrA); ++kb) {
            cute::gemm(tiled_mma, tCrA(_, _, kb, read_stage), tCrB(_, _, kb, read_stage), acc_stage);
            tiled_mma.accumulate_ = UMMA::ScaleOut::One;
          }
          mainloop_pipeline.consumer_release(cons_state);
          ++cons_state;
          acc_pipeline.producer_commit(acc_state);
          ++acc_state;
        }
      } else {
        for (uint32_t s = 0; s < p.k_slices; ++s) { ++acc_state; }
      }
    }
    if (is_leader) acc_pipeline.producer_tail(acc_state);
    // Let the epilogue warps finish reading before releasing TMEM.
    cutlass::arch::NamedBarrier epi_done_bar(EPI_THREADS + 32, (cutlass::arch::ReservedNamedBarriers)(int(cutlass::arch::ReservedNamedBarriers::FirstUserBarrier) + 1));
    epi_done_bar.arrive_and_wait();
    tmem_allocator.release_allocation_lock();
    tmem_allocator.free(tmem_base, cute::TMEM::Sm100TmemCapacityColumns);
  } else {
    // ---------------- Epilogue warps ----------------
    const int epi_tid = threadIdx.x;  // 0..255 (epilogue = warps 0-7: TMEM dp-window alignment)
    cutlass::arch::NamedBarrier tmem_bar(EPI_THREADS + 32, cutlass::arch::ReservedNamedBarriers::TmemAllocBarrier);
    tmem_bar.arrive_and_wait();
    const uint32_t tmem_base = shared.tmem_base_ptr;

    auto acc_shape = CollectiveMainloop::partition_accumulator_shape();
    auto tAcc = cutlass::detail::make_sm100_accumulator<2, false>(
        TiledMma{}, acc_shape, make_shape(Int<128>{}, Int<256>{}));
    tAcc.data() = tmem_base;

    Tensor cD = make_identity_tensor(make_shape(Int<GEN5_TILE_M>{}, Int<GEN5_TILE_N>{}));
    auto tiled_t2r = make_tmem_copy(SM100_TMEM_LOAD_32dp32b32x{}, tAcc(_, _, _, 0));
    auto thr_t2r = tiled_t2r.get_slice(epi_tid & 127);  // TV has 128 threads; warps 4-7 reuse 0-3
    auto cta_mma_epi = TiledMma{}.get_slice(cluster_rank);
    Tensor tCcD = cta_mma_epi.partition_C(cD);
    Tensor tDcD = thr_t2r.partition_D(tCcD);
    Tensor frag_tile = make_tensor<ElementAcc>(shape(tDcD(_, 0, _, _)));

    // ---- static coordinate model + one-shot self-check ----
    const int tv = epi_tid & 127;
    const uint32_t r_static = 32u * (tv >> 5) + (epi_tid & 31);
    const uint32_t HR_static = 8u * ((r_static >> 6) & 1u) + (r_static & 7u);
    const int t0 = (epi_tid >= 128) ? 4 : 0;   // column half: warps 0-3 -> tg 0-3, warps 4-7 -> 4-7
    uint32_t slot_base[4];
#pragma unroll
    for (int tt = 0; tt < 4; ++tt)
      slot_base[tt] = HR_static * 16u + 4u * ((t0 + tt) >> 1);
    {
      bool ok = true;
#pragma unroll
      for (int tt = 0; tt < 4; ++tt) {
        auto ct_chk = tDcD(_, t0 + tt, _, _);
#pragma unroll
        for (int e = 0; e < 32; ++e) {
          auto rc = ct_chk(e);
          if ((get<0>(rc) & 127u) != r_static || get<1>(rc) != (uint32_t)(32 * (t0 + tt) + e)) ok = false;
        }
      }
      if (!ok) atomicOr(p.status, STATUS_BADMODEL);  // results would be untrustworthy
    }

    cutlass::arch::NamedBarrier fold_bar(EPI_THREADS, cutlass::arch::ReservedNamedBarriers::EpilogueBarrier);
    typename AccumulatorPipeline::PipelineState acc_cons;
    const int laneid = threadIdx.x & 31;

    for (uint32_t ct = p.tile_begin + my_cluster; ct < p.tile_end; ct += nclusters) {
      uint32_t tm, tn;
      seq.coords(ct, tm, tn);

      ElementAcc cum[128];
#pragma unroll
      for (int i = 0; i < 128; ++i) cum[i] = 0;

      for (uint32_t s = 0; s < p.k_slices; ++s) {
        acc_pipeline.consumer_wait(acc_cons);
        Tensor tDtS = thr_t2r.partition_S(tAcc(_, _, _, acc_cons.index()));
        if (s != 0) fold_bar.arrive_and_wait();  // last slice's owner reads are done
        shared.fold_part[epi_tid] = 0u;
        fold_bar.arrive_and_wait();
        uint32_t part[4];
#pragma unroll
        for (int q = 0; q < 4; ++q) part[q] = 0u;
#pragma unroll
        for (int tt = 0; tt < 4; ++tt) {
          copy(tiled_t2r, tDtS(_, t0 + tt, _, _), frag_tile);
#pragma unroll
          for (int e = 0; e < 32; ++e) {
            const ElementAcc v = cum[tt * 32 + e] + frag_tile(e);
            cum[tt * 32 + e] = v;
            part[(e & 7u) >> 1] ^= static_cast<uint32_t>(v);   // q = (col & 7) >> 1, col = 32tg+e
          }
          // lanes l, l^8, l^16, l^24 share HR (rows 8 apart, same window): merge 4 rows.
#pragma unroll
          for (int q = 0; q < 4; ++q) {
            part[q] ^= __shfl_xor_sync(0xffffffffu, part[q], 8);
            part[q] ^= __shfl_xor_sync(0xffffffffu, part[q], 16);
          }
          // one representative atomic per group (laneid < 8); slot base = HR*16 + 4*blk (static).
          if (laneid < 8) {
#pragma unroll
            for (int q = 0; q < 4; ++q)
              atomicXor(&shared.fold_part[slot_base[tt] + q], part[q]);
          }
        }
        fold_bar.arrive_and_wait();
        // transcript owner: thread epi_tid owns hash tile (HR = epi_tid>>4, HC = epi_tid&15).
        {
          const uint32_t HR = epi_tid >> 4, HC = epi_tid & 15u;
          const uint32_t t_rows = tm * 256 + cluster_rank * 128 + 64u * (HR >> 3) + (HR & 7u);
          const uint32_t t_cols = tn * 256 + 64u * ((HC >> 2) & 3u) + 2u * (HC & 3u);
          if (t_rows < p.m && t_cols < p.n) {  // 64-multiple shapes: t_rows < m <=> tile fully in
            const uint32_t fold = shared.fold_part[HR * 16u + HC];
            uint32_t* my_t = &shared.t_circ[epi_tid * 17];
            const uint32_t head = s & 15u;
            const uint32_t old = (s < 16u) ? 0u : my_t[head];  // first pass has no predecessor
            my_t[head] = (old << TRANSCRIPT_ROTL | old >> (32 - TRANSCRIPT_ROTL)) ^ fold;
          }
        }
        acc_pipeline.consumer_release(acc_cons);
        ++acc_cons;
      }

      // The circular slots already hold the transcript in pool order (slot j = word j).
      uint32_t t[16];
#pragma unroll
      for (int j = 0; j < 16; ++j)
        t[j] = (j < (int)p.k_slices) ? shared.t_circ[epi_tid * 17 + j] : 0u;

      // BLAKE3 + bound + hit/dump
      {
        uint32_t digest[8];
        uint32_t key[8], bound[8];
#pragma unroll
        for (int i = 0; i < 8; ++i) { key[i] = p.key[i]; bound[i] = p.bound[i]; }
        spm::b3::keyed_hash_one_block(key, t, digest);
        const uint32_t HR = epi_tid >> 4, HC = epi_tid & 15u;
        const uint32_t t_rows = tm * 256 + cluster_rank * 128 + 64u * (HR >> 3) + (HR & 7u);
        const uint32_t t_cols = tn * 256 + 64u * ((HC >> 2) & 3u) + 2u * (HC & 3u);
        if (t_rows < p.m && t_cols < p.n) {
          if (p.dump != nullptr) {
            const uint32_t T_r = (tm * 256 + cluster_rank * 128) / 8 + (HR >> 3) * 8 + (HR & 7u);
            const uint32_t T_c = (tn * 256) / 16 + (HC >> 2) * 4 + (HC & 3u);
            const uint64_t idx = (uint64_t)T_r * p.dump_row_stride + T_c;
            uint32_t* rec = p.dump + idx * DUMP_WORDS;
            rec[0] = t_rows; rec[1] = t_cols;
#pragma unroll
            for (int i = 0; i < 16; ++i) rec[2 + i] = t[i];
#pragma unroll
            for (int i = 0; i < 8; ++i) rec[18 + i] = digest[i];
          }
          if (spm::u256_le_leq(digest, bound)) {
            const uint32_t slot = atomicAdd(p.hit_count, 1u);
            if (slot < p.hit_capacity) {
              HitRecord5& hit = p.hits[slot];
              hit.t_rows = t_rows;
              hit.t_cols = t_cols;
#pragma unroll
              for (int i = 0; i < 8; ++i) hit.digest[i] = digest[i];
            }
          }
        }
      }
    }
    cutlass::arch::NamedBarrier epi_done_bar(EPI_THREADS + 32, (cutlass::arch::ReservedNamedBarriers)(int(cutlass::arch::ReservedNamedBarriers::FirstUserBarrier) + 1));
    epi_done_bar.arrive_and_wait();
    cute::cluster_arrive_relaxed();
    cute::cluster_wait();
  }
  if (warp_idx >= EPI_WARPS) {  // producer + MMA: the epilogue synced inside its branch
    cute::cluster_arrive_relaxed();
    cute::cluster_wait();
  }
}


namespace {
cudaError_t configure_once() {
  static cudaError_t status = [] {
    return cudaFuncSetAttribute(gemm_hash_umma_kernel,
                                cudaFuncAttributeMaxDynamicSharedMemorySize, (int)SMEM_BYTES_5);
  }();
  return status;
}
}  // namespace

cudaError_t configure_gemm_hash_s8_umma() { return configure_once(); }

cudaError_t gemm_hash_s8_umma_attributes(cudaFuncAttributes* out) {
  return cudaFuncGetAttributes(out, gemm_hash_umma_kernel);
}

cudaError_t launch_gemm_hash_s8_umma(Params5 p, uint32_t grid, cudaStream_t stream) {
  const cudaError_t cfg = configure_once();
  if (cfg != cudaSuccess) return cfg;
  if (grid == 0) return cudaSuccess;
  // The collective builds its own TMA descriptors from the raw operands; row strides are the real
  // k_total (the kernel itself only iterates k_slices = floor(k/128) slices).
  typename CollectiveMainloop::Arguments cargs{
      const_cast<int8_t*>(p.a_ptr),
      cute::make_stride(int64_t(p.k_total), Int<1>{}, int64_t(p.m) * p.k_total),
      const_cast<int8_t*>(p.b_ptr),
      cute::make_stride(int64_t(p.k_total), Int<1>{}, int64_t(p.n) * p.k_total)};
  p.cutlass_params = CollectiveMainloop::to_underlying_arguments(
      make_shape(p.m, p.n, p.k_total, 1), cargs, nullptr, cutlass::KernelHardwareInfo{});
  // cuda_stream MUST carry the job stream: without it the kernel lands on the default stream,
  // so every event/status/memcpy on the job stream decouples from the running kernel (chunk
  // timings collapse to launch latency, prep kernels starve against the mining kernel, the
  // 10 ms cancellation contract breaks). Found via the worker's attempt decomposition
  // (kernel_ms 0.87 vs wall_ms 1403 per attempt, 2026-11-13).
  const cutlass::Status st = cutlass::launch_kernel_on_cluster(
      cutlass::ClusterLaunchParams{dim3(grid), dim3(THREADS), dim3(2, 1, 1), (int)SMEM_BYTES_5,
                                   stream},
      (const void*)&gemm_hash_umma_kernel, p);
  return st == cutlass::Status::kSuccess ? cudaSuccess : cudaErrorUnknown;
}

}  // namespace umma
}  // namespace spm
