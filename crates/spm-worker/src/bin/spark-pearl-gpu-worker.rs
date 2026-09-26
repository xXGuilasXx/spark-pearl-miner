//! `spark-pearl-gpu-worker`: the GPU worker on its own (the same code as
//! `spark-pearl-miner gpu-worker`), for a unit that runs it outside the daemon's process tree.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "spark-pearl-gpu-worker",
    about = "GPU worker of spark-pearl-miner: attaches to the daemon's worker.sock and mines on the GPU"
)]
struct Cli {
    /// Socket of the daemon (default: $XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock).
    #[arg(long)]
    attach: Option<PathBuf>,
    /// Seconds to keep retrying the connection.
    #[arg(long, default_value_t = 15)]
    connect_timeout_s: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let sock = cli.attach.unwrap_or_else(spm_worker::default_sock);
    let mut opts = spm_worker::Options::new(sock);
    opts.connect_timeout = Duration::from_secs(cli.connect_timeout_s);
    match spm_worker::run_to_exit(opts, spm_worker::gpu::GpuEngine::new) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("spark-pearl-gpu-worker: {e:#}");
            ExitCode::FAILURE
        }
    }
}
