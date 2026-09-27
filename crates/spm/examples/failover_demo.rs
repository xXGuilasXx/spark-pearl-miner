//! The P0 failover demo for the GUI, on localhost only (no real pool is contacted):
//! three mock pools, the daemon with the simulated CPU worker, and the web GUI.
//!
//! ```text
//! cargo run --release -p spark-pearl-miner --example failover_demo -- --port <free port> \
//!     [--dir /tmp/spm-demo] [--wallet prl1p…] [--pool1-down-s 30] [--all-down]
//! ```
//!
//! Pool 1 refuses connections for the first `--pool1-down-s` seconds (the GUI shows pool 2 active
//! within seconds), then comes back (the GUI shows the return to pool 1 after the probe cadence,
//! shortened here to 20 s + 10 s). `--all-down` keeps every pool refusing (the "no pool
//! reachable" state). `--port` is required and cannot be 4078, the port of a real miner.
//!
//! The GPU is never used: the worker is the CPU simulation and the memory guard is off (the
//! simulation takes no GPU memory). Power readings come from the real GPU when NVML or
//! nvidia-smi answers (read-only), so the power card shows what the GPU is really doing.
//! Open the printed URL; over SSH use `ssh -L <port>:127.0.0.1:<port> <host>`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use spm::daemon::DaemonOptions;
use spm::paths::Paths;
use spm_api::config::{Config, DialectSetting, PoolEntry, Strictness, TlsSetting};
use spm_mockpool::{Faults, MockConfig, MockPool};

/// A valid Pearl address that nobody uses (bech32m of SHA-256("spark-pearl-miner example
/// wallet")): the demo and the manual's screenshots never show a real wallet.
const PLACEHOLDER_WALLET: &str = "prl1pg69hxg0gx3dhlqj0nvxt4w833px6vmx6v45esqw8vayn7ky8jxjswf035d";

const USAGE: &str = "usage: failover_demo --port <PORT> [--dir DIR] [--wallet prl1p…] [--pool1-down-s N] [--all-down]\n\
                     --port is required and must not be 4078 (the port of a real miner on this machine).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port = match arg("--port").and_then(|p| p.parse::<u16>().ok()) {
        Some(p) if p != 0 && p != spm_api::config::DEFAULT_API_PORT => p,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let opts = Opts {
        port,
        dir: PathBuf::from(arg("--dir").unwrap_or_else(|| format!("/tmp/spm-demo-{}", std::process::id()))),
        wallet: arg("--wallet").unwrap_or_else(|| PLACEHOLDER_WALLET.to_string()),
        pool1_down: Duration::from_secs(arg("--pool1-down-s").and_then(|s| s.parse().ok()).unwrap_or(30)),
        all_down: args.iter().any(|a| a == "--all-down"),
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("failover_demo: {e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Opts {
    port: u16,
    dir: PathBuf,
    wallet: String,
    pool1_down: Duration,
    all_down: bool,
}

async fn run(o: Opts) -> anyhow::Result<()> {
    let refusing = || {
        let mut c = MockConfig::trivial();
        c.faults = Faults { refuse_connections: true, ..Faults::default() };
        c.job_interval = Some(Duration::from_secs(20));
        c
    };
    let healthy = || {
        let mut c = MockConfig::trivial();
        c.job_interval = Some(Duration::from_secs(20));
        c
    };
    let pool1 = MockPool::start(refusing()).await?;
    let pool2 = MockPool::start(if o.all_down { refusing() } else { healthy() }).await?;
    let pool3 = MockPool::start(if o.all_down { refusing() } else { healthy() }).await?;

    let mut cfg = Config::default();
    cfg.miner.wallet = o.wallet.clone();
    cfg.miner.worker = "demo".into();
    cfg.miner.disclosure_accepted = true;
    let slot = |name: &str, port: u16| {
        let mut p = PoolEntry::new(name, "127.0.0.1", port, TlsSetting::Off);
        p.dialect = DialectSetting::Object;
        p
    };
    cfg.pools = vec![slot("Mock pool 1", pool1.port()), slot("Mock pool 2", pool2.port()), slot("Mock pool 3", pool3.port())];
    cfg.worker.simulate = true;
    cfg.worker.sim_interval_ms = 2000;
    cfg.failover.backoff_s = vec![2, 4, 8];
    cfg.failover.failback_probe_every_s = 20;
    cfg.failover.failback_stable_s = 10;
    cfg.api.port = o.port;
    cfg.validate(Strictness::Submit).map_err(|e| anyhow::anyhow!("demo config: {e}"))?;

    let paths = Paths::under(&o.dir);
    paths.ensure()?;
    std::fs::write(paths.config_file(), cfg.to_toml())?;
    let mut opts = DaemonOptions::new(paths);
    // The simulated worker takes no GPU memory: the guard would only hold the demo on a box whose
    // memory is busy with something else.
    opts.memory = None;
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("info"))
            .with(tracing_subscriber::fmt::layer())
            .with(spm::logring::RingLayer(opts.logs.clone()))
            .init();
    }
    opts.worker_exe = std::env::current_exe()?
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("spark-pearl-miner"))
        .unwrap_or_else(|| PathBuf::from("spark-pearl-miner"));
    let d = spm::daemon::start(opts).await?;
    let port = d.api_port.unwrap_or(o.port);
    println!("\nGUI: http://127.0.0.1:{port}/#token={}\n(state in {})\n", d.token, o.dir.display());
    d.bridge.control_op(spm_api::ControlOp::Start).await.map_err(anyhow::Error::msg)?;

    if !o.all_down {
        tokio::time::sleep(o.pool1_down).await;
        println!("pool 1 is back");
        pool1.set_faults(Faults::default());
    }
    wait_for_exit().await;
    d.shutdown().await;
    Ok(())
}

/// Ctrl-C or SIGTERM (the screenshot script stops the demo with `kill`).
async fn wait_for_exit() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match term.as_mut() {
                Some(t) => { t.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
}
