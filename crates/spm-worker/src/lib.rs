//! spm-worker — the GPU worker process of spark-pearl-miner (`spark-pearl-miner gpu-worker`, or
//! the standalone `spark-pearl-gpu-worker`).
//!
//! It attaches to the daemon's `worker.sock`, runs a known-answer test, reports `Ready`, and then
//! mines the work units it is given: per job the host builds the Merkle layer caches of the
//! seed-generated A and Bᵀ and the B-side seed, the GPU builds B'ᵀ; per attempt a nonce is
//! patched into chunk 0 of A (one leaf and ~19 parents rehashed), the GPU builds A' and runs the
//! fused GEMM + transcript + BLAKE3 kernel in abortable chunks; a canary tile is recomputed on the
//! CPU, and every share is turned into a PlainProof and verified with the official verifier
//! before it is sent. See README.md.
#![forbid(unsafe_code)]

pub mod cpu;
pub mod engine;
pub mod gpu;
pub mod host;
pub mod kat;
pub mod telemetry;
pub mod worker;

use std::path::PathBuf;

pub use engine::{AbortFlag, ChunkReport, ChunkStatus, Engine, EngineError, Hit, JobSpec};
pub use host::{AttemptHost, JobHost, JobKey};
pub use worker::{canary_bound, run_to_exit, run_with, Exit, Options, Outcome, Timings};

/// What `spark-pearl-miner gpu-worker` passes in.
#[derive(Debug, Clone)]
pub struct Args {
    /// The daemon's `worker.sock`.
    pub sock: PathBuf,
}

/// Runs the GPU worker until the daemon releases it; an error for anything else (a fault, a
/// lost daemon), after the fault has been reported over the socket.
pub fn run(args: Args) -> anyhow::Result<()> {
    run_to_exit(Options::new(args.sock), gpu::GpuEngine::new)
}

/// `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock`, or `$XDG_STATE_HOME` (else
/// `~/.local/state`)`/spark-pearl-miner/run/worker.sock` without a runtime directory — the
/// daemon's default.
pub fn default_sock() -> PathBuf {
    let env_dir = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let app = "spark-pearl-miner";
    match env_dir("XDG_RUNTIME_DIR") {
        Some(d) => d.join(app).join("worker.sock"),
        None => {
            let home = env_dir("HOME").unwrap_or_else(|| PathBuf::from("/tmp"));
            env_dir("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local/state"))
                .join(app)
                .join("run/worker.sock")
        }
    }
}
