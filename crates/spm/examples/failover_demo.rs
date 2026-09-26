//! The P0 failover demo for the GUI, on localhost only (no real pool is contacted):
//! two mock pools, the daemon with the simulated CPU worker, and the web GUI.
//!
//! ```text
//! cargo run --release -p spark-pearl-miner --example failover_demo -- [--port 4078] [--dir /tmp/spm-demo]
//! ```
//!
//! Pool 1 refuses connections for the first 30 s (the GUI shows pool 2 ACTIVE within seconds),
//! then comes back (the GUI shows the return to pool 1 after the probe cadence, shortened here to
//! 20 s + 10 s). Open the printed URL; over SSH use `ssh -L 4078:127.0.0.1:4078 <host>`.

use std::path::PathBuf;
use std::time::Duration;

use spm::daemon::DaemonOptions;
use spm::paths::Paths;
use spm_api::config::{Config, DialectSetting, PoolEntry, TlsSetting};
use spm_mockpool::{Faults, MockConfig, MockPool};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(4078);
    let dir = PathBuf::from(arg("--dir").unwrap_or_else(|| format!("/tmp/spm-demo-{}", std::process::id())));

    let mut refusing = MockConfig::trivial();
    refusing.faults = Faults { refuse_connections: true, ..Faults::default() };
    refusing.job_interval = Some(Duration::from_secs(20));
    let pool1 = MockPool::start(refusing).await?;
    let mut second = MockConfig::trivial();
    second.job_interval = Some(Duration::from_secs(20));
    let pool2 = MockPool::start(second).await?;

    let mut cfg = Config::default();
    cfg.miner.wallet = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh".into();
    cfg.miner.worker = "demo".into();
    cfg.miner.disclosure_accepted = true;
    let slot = |name: &str, port: u16| {
        let mut p = PoolEntry::new(name, "127.0.0.1", port, TlsSetting::Off);
        p.dialect = DialectSetting::Object;
        p
    };
    cfg.pools = vec![slot("Mock pool 1", pool1.port()), slot("Mock pool 2", pool2.port())];
    cfg.worker.simulate = true;
    cfg.worker.sim_interval_ms = 2000;
    cfg.failover.backoff_s = vec![2, 4, 8];
    cfg.failover.failback_probe_every_s = 20;
    cfg.failover.failback_stable_s = 10;
    cfg.api.port = port;

    let paths = Paths::under(&dir);
    paths.ensure()?;
    std::fs::write(paths.config_file(), cfg.to_toml())?;
    let mut opts = DaemonOptions::new(paths);
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
    let port = d.api_port.unwrap_or(port);
    println!("\nGUI: http://127.0.0.1:{port}/#token={}\n(state in {})\n", d.token, dir.display());
    d.bridge.control_op(spm_api::ControlOp::Start).await.map_err(anyhow::Error::msg)?;

    tokio::time::sleep(Duration::from_secs(30)).await;
    println!("pool 1 is back");
    pool1.set_faults(Faults::default());
    tokio::signal::ctrl_c().await?;
    d.shutdown().await;
    Ok(())
}
