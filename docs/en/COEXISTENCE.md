# Coexistence with an LLM server

A DGX Spark usually has a job already: serving a model. spark-pearl-miner is built to mine only while that job is idle and to get out of its way fast. Code: `crates/spm-coexist`. Numbers: `docs/_data/facts.toml` (`[coexist]`). Portuguese: [`docs/pt-BR/COEXISTENCIA.md`](../pt-BR/COEXISTENCIA.md).

## 1. Why the miner has to yield

- **Time-slicing.** Two CUDA contexts on one GPU are time-sliced. When both are busy each gets roughly half the GPU, so mining next to a busy vLLM costs it about **50 %** of its throughput and adds latency to every request, and the miner loses the same.
- **Unified memory.** GB10 shares one LPDDR5X pool between CPU and GPU. Under memory pressure the box does not OOM cleanly, it wedges. On the author's box the orchestration also kills vLLM when `MemAvailable` drops below 12 GiB.
- **Power.** Adding our load to vLLM's can push the GPU into the power-off band ([POWER-THERMAL](POWER-THERMAL.md)). The governor's trips count other processes' power too.

## 2. Modes

| Mode | Who starts and stops the worker | While the LLM server is busy | Use it for |
|---|---|---|---|
| `spark-modo` | the `spark-modo` `miner` runtime | the worker is not running (the runtime was swapped out) | boxes managed by `spark-modo` (the author's) |
| `yield` (default) | the daemon | worker **paused**, CUDA context and memory kept | quick resume, memory to spare |
| `yield-release` | the daemon | worker **released**: the process exits, context and memory freed | memory is tight, or the LLM server needs the whole GPU |
| `exclusive` | the daemon | ignored: the miner keeps mining | opt-in, dedicated boxes only |

The memory guard (section 5) and the power governor apply in every mode.

## 3. spark-modo runtime model

On the author's Spark, `spark-modo` hands out an exclusive GPU lease to one *runtime* at a time (the vLLM server, a training mode, and so on). The miner is one more runtime, `miner`:

- The worker runs only as that runtime, started by the system unit that `contrib/spark-modo/` installs. No CUDA process is left resident outside it.
- When a request needs a model, `spark-modo` swaps the miner out: the worker is stopped, its context is freed and the model loads. The v0 worker stops within ~10 ms.
- A training mode never starts the miner.
- In this mode the daemon does not gate anything. It keeps pools, fee and statistics, and reports whether the `miner` runtime is active.

The integration files live in `contrib/spark-modo/` (M9); the owner reviews and installs them with sudo.

## 4. The vLLM signal (`yield`, `yield-release`)

vLLM serves Prometheus metrics without authentication at `GET http://127.0.0.1:8001/metrics` (about 58 KB; a fetch takes ~6 ms on this box). The daemon reads two gauges and sums them over every series (engine, model):

```text
vllm:num_requests_running{engine="0",model_name="..."} 0.0
vllm:num_requests_waiting{engine="0",model_name="..."} 0.0
```

Names must match exactly (`vllm:num_requests_waiting_by_reason` is a different metric). The client is a small HTTP/1.1 `GET` over tokio: plain `http://`, `Connection: close`, `Content-Length`, chunked or read-to-close bodies, a 4 MiB cap and a 250 ms timeout.

Rules:

- Poll every **200 ms** (configurable 100–250 ms).
- Any request running or waiting → pause (or release) **at once**.
- Mine again only after **5 s** of continuous `running == 0 && waiting == 0`. The 5 s restart from the last busy poll.
- The daemon starts out not mining and needs the same 5 s of quiet first.

**Fallback.** When the metrics are unavailable (connection refused, timeout, not a vLLM), the daemon uses NVML per-process SM utilization: the sum over the *other* compute processes (our worker excluded, graphics-only processes such as the compositor ignored). At or above 10 % counts as busy. If neither source answers, the GPU counts as busy: the miner never mines blind next to an LLM server.

**Latency cost.** A request that arrives while we mine waits at most one poll (200 ms) + the fetch (~6 ms) + the pause (≤ 10 ms in v0) before vLLM has the GPU to itself, and meanwhile runs time-sliced with us, not blocked.

## 5. Memory guard

Read from `/proc/meminfo` (`MemAvailable`) and `/proc/pressure/memory` (`some avg10`):

- **Start** only if `MemAvailable − worker budget ≥ 20 GiB`. The worker's budget is fixed at ≤ 2 GiB.
- **Exit** (release the worker) if `MemAvailable < 16 GiB` or `some avg10 > 10 %`.
- The gap between 20 and 16 GiB is the hysteresis: after a memory exit the worker comes back only when the start condition holds again.
- A kernel without PSI only loses the pressure rule; an unreadable `MemAvailable` refuses the start.

These thresholds sit well above the 12 GiB where the owner's orchestration kills vLLM.

## 6. Pause/resume handshake

For controllers outside the daemon (spark-modo scripts, a shell) the worker speaks POSIX signals:

- **SIGUSR1 = pause.** The worker lets the GEMM chunk in flight finish (a quiescent point, ≤ 10 ms in v0), stops issuing GPU work, keeps its context and memory, then acknowledges. With nothing in flight it is quiescent at once.
- **SIGUSR2 = resume.** Immediate, acknowledged.
- **ACK:** one line `paused <seq>` or `running <seq>`, `seq` growing on every ACK, written atomically to `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack` and sent to the daemon over the worker IPC. Repeating a command re-sends the ACK, so a controller that missed one simply asks again.
- Signals coalesce: the worker keeps only the last one received between two polls of its loop, so the last command wins.
- **Escalation:** no pause ACK within 100 ms → SIGTERM (the worker exits at its next quiescent point and the context is freed); still alive 3 s later → SIGKILL. A resume without ACK within 1 s is re-sent up to 3 times and then reported.

Both sides are pure state machines in `spm_coexist::handshake`, tested without processes.

## 7. Status

Done and unit-tested: the Prometheus parser (on a constructed exposition in the vLLM layout), the HTTP client (against local servers: `Content-Length`, chunked, read-to-close, split reads, errors, timeout), the yield gate, the SM-utilization fallback (on `nvidia-smi pmon` output from this box), the memory guard (on `/proc` fixtures) and both sides of the handshake. `cargo run --release -p spm-coexist --example vllm_load` polls the live endpoint and prints what the gate would decide.

Still open (TODO M11): yield and yield-release under a real vLLM load, the ≤ 10 ms pause measured with the real worker, `spark-recurso` starting vLLM after a release, and the memory guard refusing a start on the box.
