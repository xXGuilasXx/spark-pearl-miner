# spm-worker — the GPU worker process (M5)

`spm-worker` is the process that holds the CUDA context: `spark-pearl-miner gpu-worker [--attach <sock>]`
(what the daemon spawns, or the spark-modo `miner` runtime runs) and the standalone
`spark-pearl-gpu-worker [--attach <sock>]` run the same code. It attaches to the daemon's
`$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock`, proves the GPU with a known-answer test, and mines the
work units it is given, sending only proofs that the official verifier accepted.

```
daemon                                   worker
  accept ── Hello{version} ────────────▶  memory guard → CUDA context → known-answer test
         ◀──────────── Ready{kat_ok, device}
         ◀──────────── Heartbeat (500 ms), Stats (1 s: credited MACs, tiles, attempts, SM clock, W)
  SetJob{WorkUnit} / Pause / Resume / SetDuty / Release / Shutdown ──▶
         ◀──────────── Proof{wu_id, session_id, job_id, is_block, digest, t_rows, t_cols, bincode}
         ◀──────────── Fault{kind, msg}        (then exit)
  worker.ack (next to the socket) ◀──── "paused N" / "running N"   (spm_coexist::handshake)
```

## Per job (host + device)

* **Fills.** `gen_seed` = the first 8 bytes (LE) of the work unit's `fill_seed`; A = `fill_int7(gen_seed,
  DOMAIN_A)`, Bᵀ = `fill_int7(gen_seed, DOMAIN_BT)` — the SplitMix64 generator of `spm-cpuref`, which the GPU
  runs too (`Operands::Generated`). Neither matrix is ever stored: SplitMix64 is counter based, so any row or
  1024-byte chunk is regenerated on demand (`spm_cpuref::fill_int7_at`).
* **Merkle layer caches** (`spm_cpuref::MatrixTree`), keyed by `job_key = blake3(header76 ‖ config52)`. The
  tree is exactly `pearl_blake3::MerkleTree`, but only nodes at and above 64-chunk segments are stored, plus
  all of segment 0: about 0.5 MiB of chaining values for a 512 MiB operand. Both trees are built in parallel
  from regenerated segments (`hazmat` subtree hashing): **67–76 ms** for the two 512 MiB operands of the
  default shape on the GB10's 20 cores.
* `root_b → bind_root_b(n) → bound_b → b_noise_seed = blake3(job_key ‖ bound_b)`, the B permutation pairs
  (for the canary), then `spm_gpu::Job::new` builds B'ᵀ on the device (**41–56 ms**, 1.06 GiB for the job).
* A work unit with the same `job_key` and shape (for example only a new share target) reuses both.

## Per attempt

1. **Nonce.** A random 64-bit start per job (so a restart never repeats work), +1 per attempt, written as 16
   nibbles (`nibble − 8` ∈ [-8, 7]) into A[0..16], i.e. chunk 0 = the first 1024 bytes of row 0.
2. **Incremental root.** `MatrixTree::patch_chunk0`: one leaf hash plus one merge per level (18 parents and
   the root at 2^19 leaves) → `root_a → bind_root_a(m) → bound_a → a_noise_seed`. Microseconds.
3. **Device.** `set_attempt` with the 1024-byte patched chunk as the A prefix (the GPU copy of chunk 0) builds
   A' (**3.2 ms** at the default shape); then chunks with the library's adaptive target (4.5 ms, capped at
   ~8 ms of work at 1800 MHz) until done.
4. **Hits** go to the verifier thread; the next attempt starts at once.

The device bound is `max(share_bound, canary_bound)`, where `canary_bound = ⌊2^256 / tiles⌋ · 8` gives about
eight hits per attempt at any difficulty.

### Canary tile

The job ABI has no single-tile dump, so the canary uses the hit path with a loosened bound (above): the
canary tile is one of the attempt's hits, chosen by rotating over them with the attempt number. The CPU
recomputes it from scratch — its 8 A rows (chunk 0 patched) and 16 Bᵀ rows are regenerated, noised with the
official `generate_uniform_random_matrix` / `generate_permutation_matrix` for exactly those indices
(`spm_cpuref::NoiseSide`), and the per-slice transcript and keyed digest are replayed
(`spm_cpuref::tile_from_rows`, proven equal to `Oracle::tile`). A different digest is a compute fault. Because
the hits are wherever the hash puts them, the canary samples the whole attempt uniformly. Also a fault: a hit
above the device bound, one tile reported with two digests, and four attempts in a row without any hit at the
canary bound (probability ~10⁻¹⁴ with eight expected), which catches a kernel that silently skips work.

### Shares

Every hit with `digest ≤ share_bound` becomes a `PlainProof` on the CPU: the tile's rows of A (chunk 0
patched) and Bᵀ with multileaf proofs from the layer caches — byte-identical to
`spm_cpuref::build_plain_proof` (tested) — and must pass `verify_v3(header, proof, Some(nbits_share))`
(`check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`) before `Proof` is sent, with
`is_block = digest ≤ block_bound`. A verify failure is a compute fault: nothing is sent.

### Faults

`Fault{kind}` is sent and the process exits; the supervisor restarts it (so the known-answer test runs again)
and stops after three failures in ten minutes.

| kind | when |
|---|---|
| `KatFailed` | the known-answer test failed (`Ready{kat_ok: false}` first); nothing is ever mined |
| `CanaryMismatch` | canary digest differs, a hit above the bound, duplicate tiles disagree, no hits for 4 attempts |
| `VerifyFailed` | a share's PlainProof failed local verification |
| `Cuda`, `OutOfMemory` | device errors; memory guard refusal or exit |
| `Other` | a work unit that cannot be mined (reported, dropped; the worker keeps running) |
| `Protocol` | no `Hello` of this IPC version |

## Known-answer test

Before `Ready`: one fixed 256 × 256 × 2048 job through the production path (host job, nonce patch, device job
in dump mode). The 512 tile records must equal `spm_cpuref::transcripts` of the same problem built from whole
matrices (`first_mismatch == None`), the host's roots and seeds the oracle's commitment, the hit ring the
oracle's hits, and the host proof of a hit must be byte-identical to `build_plain_proof` and pass the
verifier. 13–32 ms on the GB10 once the context is up.

## Control

* **Pause / Resume** (IPC) and **SIGUSR1 / SIGUSR2** feed one `WorkerHandshake` (the last command wins). A
  pause sets the abort word at once, so the chunk in flight stops within a tile; the ACK `paused N` is
  written to `worker.ack` as soon as nothing is queued on the device (**0.6–1.2 ms** measured, mid-attempt at
  the default shape). The context and the job stay; `Resume` re-runs the interrupted chunk (hits it
  reports twice are de-duplicated and must agree). Nothing is mined before the first `Resume`.
* **SetJob** with a different work unit bumps the epoch and sets the abort word: the attempt is cancelled
  within one chunk (**61–290 µs** from `SetJob` to nothing queued at the default shape).
* **SetDuty** p %: after each chunk the worker sleeps `kernel · 100 / p` (the queued chunk runs meanwhile).
* **Release / Shutdown / SIGTERM / SIGINT**: stop at the next quiescent point, free the job, exit with status
  0 (queued verifications are dropped so the context goes quickly; `nvidia-smi` no longer lists the process).
* **Memory**: the device job is checked against 2 GiB by the ABI; start refused unless
  `MemAvailable − 2 GiB ≥ 20 GiB`; exit below 16 GiB available or above 10 % memory pressure
  (`spm_coexist::memguard`).

## Measurements (2026-09-26, vLLM resident and idle, SM clock not locked)

| | |
|---|---|
| default shape 131072² × 4096, attempt | **815–826 ms** wall (777–787 ms kernel, 3.2–3.5 ms `set_attempt`) at 2424 MHz = **85–86 T-MAC/s credited end to end** (before the 4.5 ms chunk graft: 819 ms / 803 ms) |
| longest chunk | 6.1 ms (8.0–9.4 ms with the former 6 ms target) |
| host job / device job | 67–76 ms / 41–56 ms |
| pause ACK / job-switch cancel | 0.6–1.2 ms / 61–290 µs |
| same, while another agent's CUDA bench time-sliced the GPU | 1.85 s per attempt at 2171 MHz; pause ACK 3.4 ms, cancel 3.7 ms |
| 4096² × 2048 (tests) | 0.55 ms per attempt; ~4 verified shares per attempt at share bound 2^241 |
| Release → process exit | 120–170 ms (2048² job) |

Not wired yet: the opt-in double-buffered A side of `spm-gpu` (`prepare_attempt`) would hide the 3.2 ms
`set_attempt` (~0.4 % of an attempt) for 528 MiB more device memory; its build runs on a second stream
that a pause does not wait for, so it needs an ABI call to fence it before the ACK.

## Code

| file | |
|---|---|
| `src/host.rs` | `JobHost` (fills, layer caches, seeds), `AttemptHost` (nonce patch), canary tile, PlainProof |
| `src/engine.rs` | the `Engine` trait (create / set attempt / run chunk / hits / dump / destroy, abort word) |
| `src/gpu.rs` | `GpuEngine` over `spm_gpu::Job` |
| `src/cpu.rs` | `CpuEngine`: the same ABI from `spm-cpuref` rows (small shapes; tests, fault injection) |
| `src/kat.rs` | the known-answer test |
| `src/worker.rs` | IPC, handshake, heartbeat/stats, verifier thread, the attempt loop |
| `src/telemetry.rs` | NVML SM clock and power |

`#![forbid(unsafe_code)]` everywhere; the FFI lives in `spm-gpu`.

## Tests

```sh
export CARGO_TARGET_DIR=$HOME/.cache/spark-pearl-miner/target-worker   # the checkout path has spaces
cargo test --release -p spm-worker                    # no GPU: host pipeline, IPC state machine, signals
SPM_GPU_TESTS=1 cargo test --release -p spm-worker --test gpu_e2e -- --test-threads=1 --nocapture
SPM_GPU_TESTS=1 cargo test --release -p spm-worker --test gpu_e2e -- --ignored --nocapture   # default shape, ~6 s, 1.06 GiB
```

* `host.rs` unit tests: roots, seeds, tiles and byte-identical proofs against the oracle for three nonces;
  every oracle hit verifies. `spm-cpuref` carries the layer cache tests (1000 random trees against
  `pearl_blake3`, with and without the chunk-0 patch).
* `tests/ipc.rs` (CPU engine, fake daemon): Ready → proofs that the official verifier accepts, stats and
  heartbeats; pause ACK < 100 ms and no chunk while paused, repeated pause re-ACKed; a new work unit cancels
  within one chunk and switches shape; duty cycle; Shutdown / Release / daemon gone; a failed KAT never mines;
  corrupted hits fail the KAT; a fault after `Ready` is a `CanaryMismatch` and nothing is submitted.
* `tests/signals.rs`: SIGUSR1 / SIGUSR2 with ACKs, mixed with IPC commands; SIGTERM exits.
* `tests/gpu_e2e.rs` (`SPM_GPU_TESTS=1`): KAT on the GPU, 4096² × 2048 with verified proofs, canaries, NVML
  stats and the pause ACK; the `spark-pearl-gpu-worker` binary mines a share and on `Release` exits 0 and
  disappears from `nvidia-smi`; `--ignored`: the default shape with full-size proofs verified.
