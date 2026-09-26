# Architecture

One Apache-2.0 binary, `spark-pearl-miner` (alias `spm`), in three roles. The design was chosen by a three-way review (MVP-first, kernel-first, product-first) scored by three independent judges; the merged result is summarized here. Consensus code is never re-implemented: the official `zk-pow` and `pearl-blake3` crates (ISC, pinned at `3fe2267`) are linked natively.

## Processes
- **Daemon** (`spm daemon`, a `systemd --user` service). It **never creates a CUDA context** (NVML only for telemetry), so it never appears as a compute process. Tasks: config service (TOML, atomic writes, hot reload), pool manager (pure-reducer failover over up to 3 user sessions plus 1 dev session), work arbiter (pause → dev slice → active pool → idle; binds every job to its session; credited-MAC counters; fee scheduler), worker supervisor (500 ms heartbeat, 5 s watchdog, backoff 5 s/30 s/2 min, hardware-fault alert after 3 failures in 10 min), power governor (NVML 10 Hz + `acpitz`, PI duty control, trips, fault signatures, unclean-shutdown step-down), coexistence (spark-modo / yield / yield-release / exclusive), API server (`127.0.0.1:4078`, REST + SSE, embedded web GUI), control socket (`$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock`, 0600, `SO_PEERCRED`).
- **GPU worker** (`spm gpu-worker`): the only process that holds a CUDA context. Fixed budget ≤ 2 GiB. Threads: control/IPC, CUDA driver (prep + gemm streams), proof pool (2–4 rayon threads on the Cortex-A725 cores). Versioned bincode frames over `worker.sock` (Hello/SetJob/Pause/Resume/SetDuty/Release/Shutdown ↔ Ready/Heartbeat/Stats/Proof/Fault). Stop/Release **exit the process**, freeing the context. Launch modes: `spawn` (generic) or `external` (DGX Spark with `spark-modo`: the system unit runs the worker under the exclusive GPU lease).
- **CLI**: `probe | capture | selftest | bench | fee-test | verify-proof | status | start | stop | gui | install-clock-cap`.

## Data flow (pool → job → GPU → share)
1. `mining.notify` on session S → `Job{session, job_id, header76, target, height, cert_version}`. `cert_version ≥ 4` or unknown ⇒ that pool is paused with "network upgrade – update required" (never an invalid share).
2. Work unit: m = n = 131072, k = 4096, r = 128, hash tile 8×16 aligned to `mma.sync` fragments (rows 0,8,…,56 × cols 0,1,8,9,…,56,57), `nbits_share = compact(target)`, bound = `penalized_target_bound(expand(nbits_share))` using the *smaller* of `target` and `expand(compact(target))`, exactly as pools verify with `nbits_override`.
3. Per job: A_base and Bᵀ are structured low-entropy fills from a seed (never stored in full); keyed-BLAKE3 Merkle layers with `job_key = blake3(header76 ‖ config52)`; `root_B → bind_root_b(n) → b_noise_seed`; the GPU builds E_B and B'ᵀ once per job.
4. Per attempt: a nonce patched into A chunk 0 with an incremental root update (1 chaining value + 19 parents) → `root_A → bind_root_a(m) → a_noise_seed`; GPU builds E_A and A' (~3–5 ms); one fused IMMA GEMM over all m·n/128 tiles (per-k-slice XOR fold, rotl-13 transcript, keyed BLAKE3, bound compare) ≈ 7.0e13 credited MACs in ~0.7–1.1 s; one canary tile per attempt recomputed on the CPU.
5. On a hit the proof pool rebuilds the 8 A rows and 16 Bᵀ rows, walks the Merkle siblings (same algorithm as `pearl_blake3::get_multileaf_proof`), builds `PlainProof{m,n,k,noise_rank,a,bt,moe:None}` and **verifies it locally** (`check_cert_version_eligible(3)` + `verify_plain_proof(…, Some(nbits_share), SeedDerivation::Salted)`). A verify failure is a compute fault: nothing is submitted, the known-answer test re-runs, mining stops if it repeats.
6. The daemon encodes the proof for the session's dialect (plain / zstd / gzip, base64) and submits **only on the originating session and only if its current `job_id` is the hit's**; otherwise the hit is discarded as stale.

## Where things live
- Fee switch: only in the work arbiter (`spm-fee` debt scheduler). The worker is fee-agnostic.
- Failover: only in the `spm-pool` reducer. The worker never sees pool identities.
- Consensus: `spm-pow` over the official crates, dispatched by `cert_version` (V3 now; V4/FP8 behind the same trait).
- GUI: embedded static files speaking REST + SSE. It never touches files or spawns processes.

## GPU kernel (sm_121a)
- v0 `gemm_v0_cpasync`: CTA tile 128×256×64, 3-stage `cp.async`, 2×4 warps of 64×64, hash tile resident in registers, transcript in L1, BLAKE3 epilogue, mapped hit ring. CI gates: `ptxas -v` with 0 spills and ≤ 232 registers; SASS contains `IMMA.16832.S8.S8` and `LDSM`, no `HMMA`. Abort flag polled per chunk (≤ 10 ms).
- v1 `gemm_v1_tma`: persistent (48 CTAs), TMA for B with an mbarrier ring, L2-band raster, double-buffered A', sweep of BK=128×2 / 64×4 / 128×128 @ 2 CTAs/SM; target ≥ 85 % of the measured IMMA peak at 2200 MHz; abort per tile (< 0.2 ms).
- The mainloop is templated on `MmaPolicy { Int8V3 | Fp8V4 }`: the same `ldmatrix`/fragment layout drives `mma.sync.m16n8k32.kind::f8f6f4.e4m3` (`QMMA.16832` on sm_121a) for the certificate-v4 fork, with one FP32 accumulation chain per output in ascending K and no split-K.
- Contingency: the official 2×64 pattern on the same mainloop through a cross-warp XOR combine, selectable per pool (`auto|official`).

## Pools and protocol
`spm-proto`: NDJSON codec (4 MiB read cap, 2 MiB write guard), `rustls` TLS on/off/auto (TLS first, plain only on a protocol error, cached per host), dialects `object` (HeroMiners — captured live 2026-09-26 — and LuckyPool), `kryptex` (v1 + v2-gzip), stubs `cryptonote` and `positional`; proof encoders plain/zstd/gzip learned per pool. Defaults: HeroMiners BR → LuckyPool BR → Kryptex.

## Failover (P0)
Pure reducer `step(state, event, now)` with an injected clock. Failure triggers on the active slot: DNS/connect/TLS failure, auth error (→ ConfigError, retried every 10 min), no job within 30 s, EOF/reset, 900 s stall (soft reconnect first), ≥ 5 consecutive invalid rejects or > 50 % of the last 20, 3 submit-ack timeouts, stale > 2 % over 100 shares, ban text (→ 10 min quarantine). Policy: start on the highest-priority slot; on failure move to the next usable slot with wrap-around; if the next also streak-rejects → `Paused{RejectEverywhere}`; all down → round-robin retries honouring backoff; fail back to a higher-priority slot after a 300 s probe plus 60 s of stable health; manual pin overrides. Invariants (proptest): at most one active user session, hits only on the originating session, no lost or duplicated share, the dev session never receives user hits.

## Developer fee
2.00 % as a time slice on a separate, pre-connected session (HeroMiners; LuckyPool/Kryptex fallbacks), worker `devfee`, 120 s slices, debt 200/9800 accrued only while hashing, persisted, capped; first slice at a random point; auto-off when the user's wallet is the fee wallet; every constant in `crates/spm-fee/src/lib.rs`; banner, logs, `/api/v1/fee`, `spm fee-test`, constants hash in `--version`; CI fails if the READMEs disagree with the constants.

## Power and coexistence
Profiles Eco / **Balanced (75 W target, 85 W stop, default)** / Max; optional boot-time clock cap unit (`nvidia-smi -lgc 300,2200`, root once); non-root NVML governor; fault signatures; `running.marker`. Memory: worker ≤ 2 GiB; refuse to start unless `MemAvailable − budget ≥ 20 GiB`; exit below 16 GiB or PSI memory `some avg10 > 10 %`. On a `spark-modo` box the worker runs only as the `miner` runtime; loading a model stops it (≤ 10 ms in v0).

## Security of the local API
Token file (0600) exchanged for an HttpOnly SameSite=Strict cookie plus a CSRF header; Host/Origin allowlist; strict CSP; pool strings rendered as text only; config changes audited; LAN binding opt-in and TLS-only.
