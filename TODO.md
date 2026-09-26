# TODO — pilot checklist (English; Portuguese summary in [TODO.pt-BR.md](TODO.pt-BR.md))

> **P0 (user request, verbatim, 2026-09-26):** _"GUI do minerador para configurar o endereço da carteira e até 3 endereços de pool e portas; para caso o usuário tenha colocado mais de uma e a primeira falhar ele vai automaticamente para a segunda."_ → wallet + up to 3 pools (host, port, TLS) editable in the GUI; automatic failover 1 → 2 → 3 with automatic return to pool 1.

Status legend: `[ ]` open · `[x]` done · `[~]` in progress. Milestone ids (M0…M15) match `docs/en/ARCHITECTURE.md` and the plan.

## Milestones

| Id | Title | Est. days | Depends on |
|---|---|---|---|
| BT | Bluetooth/keyboard fix (the user's stated priority), already applied | 0.1 | — |
| M0 | Approvals, toolchain, repo skeleton | 1 | — |
| M1 | Oracle core: spm-pow + spm-cpuref | 1.5 | M0 |
| M2 | Protocol capture tooling + authorize-only probes | 1 | M0 |
| M3 | Optional Day-1/2 bring-up share with CPPminer (CPU only) | 1 | M0, M2 |
| M4 | Host core: proto, work, ipc, mockpool | 3 | M1, M2 |
| M5a | GB10 microbenchmark window | 1 | M0 |
| M5 | libspm_cuda v0 + gpu-worker, bit-exact (gate G0) | 8 | M1, M5a |
| M6 | First accepted shares with our own miner (gate G2) | 1 | M4, M5 |
| M7 | 3-pool failover state machine (backend of the user's P0 item) | 3 | M4 |
| M8 | Transparent 2% dev fee | 1.5 | M6, M7 |
| M9 | Daemon API + web GUI EN/PT-BR + systemd/desktop + spark-modo contrib (frontend of the user's P0 item) | 4 | M7 |
| M10 | Kernel v1 performance | 5 | M5, M6 |
| M11 | Power governor, clock cap, coexistence, soaks (gate G1) | 2 | M6 |
| M12 | FP8/V4 readiness spike (gate G3) | 1.5 | M1, M5a |
| M13 | Docs EN+PT-BR, CI, public release v0.1.0 (gate G4) | 3 | M8, M9, M11 |
| M14 | 72 h pilot + report + v0.1.1 | 3 | M13 |
| M15 | Contingency: official-pattern fallback (only if a pool rejects 8x16) | 2 | M6 |

## Checklist


### BT

- [x] DONE 2026-09-26 16:31-16:36 UTC. Changes: audio stack limited to the desktop user (ConditionUser=xxguilasxx drop-ins in /etc/systemd/user/*.d/50-only-desktop-user.conf); user@990.service (spark-rtx-feed) masked; BT USB autosuspend off (udev + modprobe); us+intl layout (GNOME + localectl). Verified 16:38 UTC: 0 Endpoint (un)registered and 0 NotPermitted events since 16:33; bluetoothd at 0.0% CPU; MX MCHNCL and MX Master 3S connected.
- [ ] Re-verify after the next reboot and again after 24 h: journalctl -u bluetooth --since -1h | grep -c Endpoint returns 0; ps -C bluetoothd -o pcpu is about 0; no repeated keys; gsettings shows us+intl.
- [ ] Optional: suggest to the owner of the 192.168.50.122 poller one persistent SSH connection (ControlMaster/ControlPersist) instead of a new login every ~5 s. Keep the rollback documented: remove the drop-ins and run systemctl unmask user@990.service.

### M0
- [ ] [M0] **Fee wallet is an exchange deposit address (SafeTrade) — user confirmed 2026-09-26.** Before v0.1.0: (1) create a self-custody Pearl wallet (official desktop wallet or oystercli) for `DEV_WALLET`; (2) until then keep the exchange address but document that exchanges may have a minimum deposit and can change/block addresses; (3) set the HeroMiners payout threshold for the user's own mining ≥ SafeTrade's PRL minimum deposit so 1 PRL payouts are not lost; (4) G4 (signed ownership challenge) is only possible with a self-custody wallet.

- [ ] docs/decisions.md records the user's answers: rustup install; optional CPPminer bring-up; LuckyPool/Kryptex authorize-only probes; GPU windows (warning before each vLLM stop plus the 2200 MHz lock); gh done by the user; repo public from day 1 or at v0.1.0; commit identity.
- [x] rustup installed at user level with the rustup-init sha256 verified; cargo --version reports 1.88 or newer; clippy and rustfmt present.
- [x] User installed gh and ran gh auth login; gh auth status shows xXGuilasXx
- [x] Repo created with git init -b main at /home/xxguilasxx/Desktop/Miner PRL/spark-pearl-miner. It contains LICENSE (Apache-2.0), NOTICE (ISC Pearl Research Labs + Decred, BSD-3 CUTLASS), THIRD_PARTY_NOTICES.md, CLAUDE.md (Opus 5.5 subagents, EN+PT-BR docs, fee only in spm-fee/src/lib.rs, never mine in CI), TODO.md and TODO.pt-BR.md.
- [x] TODO.md carries the P0 request verbatim: GUI do minerador para configurar o endereço da carteira e até 3 endereços de pool e portas; se o usuário colocou mais de uma e a primeira falhar, ele vai automaticamente para a segunda.
- [x] zk-pow and pearl-blake3 pinned as git deps at rev 3fe226761a139a9652b8f28a6464a4bbc25986c8; cargo build --release -p spm-pow succeeds on aarch64; Cargo.lock committed locally.
- [x] third_party/cutlass submodule pinned to a v4.x tag; the probe built with nvcc -gencode arch=compute_121,code=sm_121 shows IMMA.16832.S8.S8 in cuobjdump -sass.

### M1

- [x] Pattern bytes asserted: rows [0,8,...,56] give 07 07 00 00 00 00; cols [0,1,8,9,...,56,57] give 00 01 03 07 00 00. The 52-byte MiningConfiguration is asserted too.
- [x] Partition test passes: tiles at t_rows = 64a+g (g<8) and t_cols = 64b+2t (t<4) cover a 512x512 output exactly once, matching offset_is_valid.
- [x] Bound math passes: diff 2,097,152 gives target 0x7fff8<<184, nbits 0x1a07fff8 and bound = target*2^19. expand(compact(t)) <= t holds under proptest, and overflow returns None.
- [ ] bind_root_a/b and the commitment chain reproduce the zk-pow seed.rs pinned vectors.
- [ ] Layer builder, multileaf sibling walker and attempt-path overlay match pearl_blake3 MerkleTree and get_multileaf_proof on 1000 random cases, and the root equals blake3::keyed_hash(job_key, data).
- [x] try_mine_one with our config (m=n=256, k=2048, r=128, easy nbits) produces proofs that pass check_cert_version_eligible(3), verify_plain_proof(Salted) and check_rank_penalty; all 5 mutation classes fail.
- [x] spm-cpuref all-tile transcripts and digests equal the zk-pow reference on 3 random problems; golden fixtures and SHA256SUMS committed. (Five problems incl. k = 4096 and a ragged k; first hit and PlainProof byte-identical to try_mine_one on replayed RNG; golden-cpuref-{1,2,3}.json.)

### M2

- [x] tools/spm-probe.py: sends one authorize per connection, allows at most 5 connections per pool per hour, listens 10 min, never submits, writes redacted JSONL.
- [x] tools/spm-proxy.py: logging TCP/TLS forwarder recording both directions with timestamps; the wallet is replaced with a placeholder and proofs are logged as sha256 plus length.
- [x] HeroMiners BR certificate chain, issuer and SAN recorded with openssl s_client, using no credentials.
- [x] HeroMiners BR authorize-only capture runs H1 object, then H1b wallet.worker, then H2 array, then H3 CryptoNote login, stopping at the first result:true. It records the notify fields, target endianness, cert_version presence, diff, job cadence and idle behaviour.
- [x] LuckyPool BR (TLS, self-signed pinned) and Kryptex 8048/TLS authorize-only captures done (7048 plain does not answer)
- [x] Redacted fixtures, SHA256SUMS and docs/protocol/{herominers,luckypool,kryptex}.md committed; notify.target == floor(0xFFFF*2^208/diff) on HeroMiners/LuckyPool, 2^224/diff-1 on Kryptex (always use notify.target)

### M3

- [ ] (optional) CPPminer @6785ad3 built CPU-only in the scratchpad with CMake -DCP_ENABLE_CPU=ON plus cargo for cp-proof-ffi; --mock passes its zk-pow verify.
- [ ] (optional) Smoke share on the LuckyPool CPU port (pearl-cpu-eu1.luckypool.io:3370) accepted through spm-proxy.
- [ ] (optional) HeroMiners BR share accepted through spm-proxy (at nice 19, pinned to X925 cores 5-9 and 15-19, 3 h cap), with the plain_proof_zst patch only if the capture requires it. The worker is visible in HeroMiners stats and the exchange is saved as a golden fixture.

### M4

- [x] spm-proto: NDJSON codec (4 MiB read cap, 2 MiB write guard, CRLF tolerant); rustls TLS off/on/auto/pinned with a per-endpoint cache and SNI. Roots come from webpki-roots (Mozilla bundle), not the OS store. Auto falls back to plain only on a TLS protocol error (garbage record, EOF or silence during the handshake), never on a certificate error. Pinned checks SHA-256(SPKI) of the end-entity key (LuckyPool `d0ehDQxa…/mk=`) and still verifies handshake signatures. Tested against local TLS/plain servers (`crates/spm-proto/tests/tls.rs`).
- [~] object, kryptex (v1) and kryptex-v2 (gzip) dialects implemented, with an optional `"jsonrpc":"2.0"` per pool (LuckyPool needs it); replaying the fixtures reproduces the outgoing handshake frames byte for byte (modulo the probe's `json.dumps` separators). Open: cryptonote and positional stubs (no pool needs them yet); kryptex-v2 is from open-source clients, not confirmed live.
- [x] plain, zstd (level 3) and gzip encoders pass round-trip tests (bounded decompression); proof and wire sizes are logged on every submit; the proof field (`plain_proof` / `plain_proof_zst`) is learned per pool (switch after 3 consecutive format rejects).
- [x] spm-work: turns a notify into a WorkUnit with target/diff/nbits_share/share bound, block bound and hash classification (miss/share/block), job_key = blake3(header76 ‖ config52), fill_seed = blake3(job_key ‖ "spm/fill/v1"); cert_version 4 or above, any other value, or a missing field gives 'update required'.
- [x] spm-ipc frames (versioned, length-prefixed bincode, 4 MiB cap) and spm-mockpool fault injection (refuse, blackhole, auth reject, no job, reject storm, mute submits, stall, EOF mid-submit, oversized line) work; the CI end-to-end test is green in under 1 s (release): the official reference miner (`try_mine_one`, m = n = 256, k = 2048, our 8x16 pattern) produces the share, the daemon-side path verifies it locally, submits it through `PoolSession`, and the mock verifies it with zk-pow and accepts it (object/plain, kryptex/zstd, kryptex-v2/gzip); a stale job's proof is refused before it reaches the wire. The share comes from the reference miner because spm-cpuref (M1) is not written yet; TLS failures are covered in spm-proto instead of the (plain-TCP) mock.

### M5a

- [x] User stopped spark-vllm.service and ran bench/mb1.sh with sudo clock locks; vLLM restored afterwards
- [x] IMMA register-only peak measured at stock/2200/2000/1800 MHz: 108.6/96.0/85.0/75.7 T-MAC/s (919 MAC/clk/SM); QMMA FP8 rate equal to INT8 (107.0 T-MAC/s stock)
- [~] ldmatrix, cp.async vs TMA fill, L2 (12 MiB band) and DRAM bandwidth measured; vLLM restored; docs/BENCHMARKS.md updated. (Partial, vLLM resident: `cuda/probes/fill_bw.cu` gives cp.async.cg 16 B → smem ~2.1 TB/s (18 B/clk/SM) and DSMEM only ~118 GB/s (1 B/clk/SM) for both remote stores and bulk pushes; ldmatrix, TMA and DRAM still open.)

### M5

- [x] Noise kernels are bit-exact against zk-pow generate_uniform_random_matrix and generate_permutation_matrix for 1000 seeds; A' and B' rows equal compute_noise_for_indices. (Strategy A branch: `tests/g0.rs` checks A_L and the A pairs for 1000 seeds and B_Rᵀ and the B pairs for 24; A' and B'ᵀ equal spm-cpuref's noised operands, which its reference tests prove equal to `compute_noise_for_indices`.)
- [x] gemm_v0 (128x256x64, 3-stage cp.async, 2x4 warps of 64x64, 8x16 register-local hash tile, L1 transcript, BLAKE3 epilogue, mapped hit ring) builds with 0 spills and at most 232 registers; SASS contains IMMA.16832.S8.S8 and LDSM. (208 registers, 0 spills, 0 stack, 73,728 B smem; `tools/check-sass.py` passes: IMMA.16832.S8.S8 x128, LDSM x32, no HMMA. Deviations: the transcript is a rotating register queue instead of L1, and the hit ring lives in device memory and is read after the attempt.)
- [x] G0: 100% of debug-dump transcripts and digests equal spm-cpuref for m,n in {256,512,1024}, k in {2048,4096}, 3 seeds each. (54 problems, 150,528 tiles, 0 mismatches; see crates/spm-gpu/README.md.)
- [x] Forced-hit test: at least 100 GPU PlainProofs pass verify_plain_proof(Salted) and check_rank_penalty; mutated proofs fail. (166 GPU hits through `verify_v3` + `check_rank_penalty`; flipped bytes, shifted rows, wrong rank and another header are rejected.)
- [x] compute-sanitizer memcheck, racecheck and synccheck are clean. (On the small-shape tests: edges/chunks/abort/patch, forced hits, operands.)
- [ ] gpu-worker runs the KAT at start, heartbeats, cancels on epoch change in 10 ms or less, and recomputes one canary tile per attempt; spark-pearl-miner selftest and bench --minutes 10 (JSON: credited MAC/s, clocks, W, temperatures) recorded at 2200 MHz. (libspm_cuda side ready: `spm_job_run_attempt` reads an abort flag between adaptive ~4 ms chunks with two in flight; the worker process, KAT and canary are still to do.)

### M6

- [ ] LuckyPool BR 3360: at least 5 accepted and 0 rejected with our miner.
- [ ] HeroMiners BR for 1 h: at least 20 accepted, 0 invalid, stale under 1%; the working proof field (plain_proof_zst or plain_proof) stored in state.json; the worker visible in HeroMiners stats.
- [ ] Pool-side hashrate within Poisson bounds of the local credited MAC/s over at least 6 h; our 8x16 pattern accepted on both pools, otherwise start M15.

### M7

- [x] spm-pool reducer with an injected clock passes at least 25 deterministic time-warped scenarios. (42 scenarios in `crates/spm-pool/tests/scenarios.rs`, plus 5 unit tests.)
- [x] proptest invariants hold: one active user session; hits only on the originating session; no lost or duplicated shares; the dev session stays isolated. (`crates/spm-pool/tests/invariants.rs`, 1024 random sequences per run plus a state-coverage guard; the dev session is outside the reducer, which is asserted never to address a non-user slot.)
- [ ] Mockpool faults (refuse, blackhole, TLS fail, auth reject, no job, reject storm, stall, EOF mid-submit, 4 MiB line) each lead to failover within 15 s.
- [ ] Live check: with pool 1 pointed at a closed port, pool 2 becomes active within 15 s; after pool 1 is restored, mining returns to it after the 300 s probe plus 60 s stable; the timeline is logged.

### M8

- [x] crates/spm-fee/src/lib.rs holds every fee constant (200 bps, DEV_WALLET, worker devfee, HeroMiners regions with LuckyPool and Kryptex fallbacks, 120 s slices); a test checks the exact address and its bech32m validity. The schedule constants live there too. There is no override path (tests/policy.rs), and the constants hash is pinned by a test.
- [x] The 30-day simulation gives 2.00% ± 0.02% (measured 1.998%); every 24 h window stays at or below 2.0% under failure injection (pauses, refused or silent dev logins, dropped dev sessions); nothing accrues while paused or yielding. Windows are measured over active mining time. The only documented exception is repaying a backlog above 240 s after a dev-pool outage: it is capped at 3600 s and paid in 120 s slices at least 30 min apart (see crates/spm-fee/README.md).
- [~] The fee turns off when wallet == DEV_WALLET; the banner shows the fee line; --version prints the constants hash; the CI guard confirms READMEs and FEE.md match lib.rs. Done: auto-off (scheduler + tests), banner(), constants_hash(), the guard on both READMEs. Open: wiring --version once the CLI crate exists, and docs/en/FEE.md plus docs/pt-BR/TAXA.md.
- [ ] spark-pearl-miner fee-test on HeroMiners: worker devfee visible under the dev address, zero stale at switches, the user session never dropped.

### M9
- [ ] Fee wallet and fee constants are read-only everywhere in the GUI and API (no edit control, PUT /api/v1/config rejects any fee field); a test asserts the config schema has no fee keys.

- [ ] API on 127.0.0.1:4078 uses token-file to cookie, CSRF header, Host/Origin allowlist and strict CSP; tests show no cookie gives 401, a foreign Host 403 and a missing CSRF header 403.
- [ ] GUI EN/PT-BR: wallet bech32m validation rejects an invalid address; up to 3 pools (host, port, TLS auto/on/off) and a 4th is rejected; reorder, presets, Test connection, and Save & Apply with hot reload.
- [ ] GUI failover demo: with pool 1 on a closed port, Pool 2 ACTIVE shows within 15 s and the later return to Pool 1 is visible; works locally and over ssh -L 4078:127.0.0.1:4078.
- [ ] Dashboard, Pools, Fee, Power, Logs (SSE plus redacted diagnostics export) and About screens done; screenshots saved for the docs.
- [ ] systemd --user unit (Restart=on-failure), .desktop launcher and spark-pearl-miner gui work; Stop frees the CUDA context (nvidia-smi lists no spark-pearl-miner process).
- [ ] contrib/spark-modo (spark-miner.service, spark-recurso and spark-modo patches, rollback.sh) prepared, reviewed and applied by the user with sudo. spark-modo runtime miner starts the worker; a router request swaps the miner for a model; treino mode never starts the miner.

### M10

- [ ] gemm_v1 (persistent 48 CTAs, TMA for B with an mbarrier ring, L2 band raster, per-tile epoch check, double-buffered A') passes the full M5 bit-exact suite.
- [ ] Sweep of BK=128x2, BK=64x4 and 128x128 at 2 CTAs/SM recorded; winner chosen; cancel latency under 1 ms.
- [ ] Kernel-only throughput reaches at least 85% of the measured IMMA peak at 2200 MHz, or the reason is documented; credited TH/s and GPU W published (goal: at least 76 TH/s at 85 W or less).

### M11

- [ ] The governor holds within ±3 W of target for 30 min in each profile, and trips fire on synthetic thresholds.
- [ ] The fault-signature detector and the running.marker step-down are demonstrated with SIGKILL plus restart.
- [ ] packaging spark-pearl-clockcap.service and install-clockcap.sh reviewed and installed by the user with sudo (optional); the miner detects the cap.
- [ ] Ladder 1800-2200 MHz at 10 min per step, a 60 min soak and a 24 h soak at the default profile: no power-off and zero compute mismatches; default profile chosen; results in docs POWER-THERMAL.
- [ ] Generic yield and yield-release tested under a vLLM load: pause takes 10 ms or less in v0; after release spark-recurso can start vLLM; the memory guard refuses to start below 20 GiB of headroom.

### M12

- [ ] G3: zk-pow built from fp8-scheme (d6755d9 or newer) in the scratchpad; the QMMA kind::f8f6f4 e4m3 microkernel compared with B200::matmul_fp8 on at least 1e6 random and adversarial atoms; result recorded.
- [ ] QMMA f32 throughput relative to IMMA measured; go/no-go memo in docs/KERNEL.md; MmaPolicy template merged.

### M13
- [ ] Release page explains how to verify the binary (SHA256SUMS + `gh attestation verify`) so users can confirm the fee wallet compiled in is the official one.

- [ ] README, README.pt-BR and docs/{en,pt-BR} complete, with facts.toml; parity, link and fee-constant checks green.
- [ ] ci.yml, release.yml and upstream-watch.yml green on ubuntu-24.04-arm; no job mines or contacts a pool.
- [ ] G4: the user signed the ownership challenge for prl1pxtue…eydh in oystercli; signature and verification steps published in FEE.md.
- [ ] tools/gpu-validate.sh report (selftest, 10 min bench, 1 h HeroMiners soak with 0 invalid, fee-test) attached to the draft release; reproducibility diff clean; SHA256SUMS and attestations present; gh attestation verify passes.
- [ ] After the user's explicit OK: public repo xXGuilasXx/spark-pearl-miner created and v0.1.0 tagged; a fresh install from the tarball reaches Mining through the GUI alone.

### M14

- [ ] 72 h pilot at Balanced: stale under 1%, rejects about 0, every failover handled, no power-off, uptime at least 99%, pool-side hashrate within ±5% of local over 12 h windows.
- [ ] docs PILOT-REPORT (EN and PT-BR) written, TODO.md updated, v0.1.1 released with the fixes.

### M15

- [ ] (only if a pool rejects 8x16) The official-pattern path via the cross-warp XOR combine passes the bit-exact suite; the per-pool pattern setting auto|official switches after a burst of invalid-proof rejects.

### ONGOING

- [ ] Every week: upstream watch (Fp8ForkHeight or new fork fields in params.go, PR #311, tags). Every month: refresh the VIABILITY snapshot (difficulty, reward, price). Review the Brazil DeCripto note with an accountant.

## Gates

- **G0** GPU int8 kernel bit-identical to the official `zk-pow` reference; PlainProofs pass `check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`.
- **G1** 60 min and 24 h soaks at the default power profile with the 2200 MHz cap: no power-off, zero compute mismatches, ≥ ~70 TH/s credited.
- **G2** Accepted shares on LuckyPool BR and HeroMiners BR with our own miner (0 invalid, stale < 1 %).
- **G3** GB10 `QMMA` FP8 bit-exact against the `fp8-scheme` `zk-pow` reference (≥ 1e6 atoms) — decides post-fork viability.
- **G4** Owner signs the fee address (BIP-322 simple, oystercli) before the public release.


## Weekly / monthly

- Weekly: upstream watch (`Fp8ForkHeight` or new fork fields in `node/chaincfg/params.go`, PR #311, tags). Monthly: refresh `docs/en/VIABILITY.md` numbers (difficulty, reward, price). Review Brazil DeCripto (IN RFB 2291/2025) with an accountant.

