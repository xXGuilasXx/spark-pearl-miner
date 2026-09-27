//! `spark-pearl-miner` (alias `spm`): daemon, GPU worker and command line in one binary.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::Value;
use spm::control::{self, Request};
use spm::paths::Paths;
use spm_api::ControlOp;

#[derive(Parser)]
#[command(
    name = "spark-pearl-miner",
    about = "Open-source Pearl (PRL) miner for the NVIDIA DGX Spark",
    long_about = None,
    disable_version_flag = true
)]
struct Cli {
    /// Print the version, the commit and the fee constants hash.
    #[arg(short = 'V', long = "version", global = true)]
    version: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (normally started by `systemctl --user start spark-pearl-miner`).
    Daemon {
        /// Do not serve the HTTP API and GUI.
        #[arg(long)]
        no_api: bool,
    },
    /// The GPU worker (normally started by the daemon or the spark-modo `miner` runtime).
    GpuWorker {
        /// Socket of the daemon to attach to (default: $XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock).
        #[arg(long)]
        attach: Option<PathBuf>,
        /// SIMULATION: mine on the CPU with the official reference miner (m = n = 256, k = 2048).
        #[arg(long)]
        sim: bool,
        /// Pause between simulated attempts.
        #[arg(long, default_value_t = 1000)]
        sim_interval_ms: u64,
    },
    /// Show what the daemon is doing.
    Status {
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start mining.
    Start,
    /// Stop mining and release the GPU.
    Stop,
    /// Pause hashing (pools stay connected).
    Pause,
    /// Resume after a pause.
    Resume,
    /// Open the web GUI in the browser.
    Gui {
        /// Only print the login URL with the token (for `ssh -L 4078:127.0.0.1:4078` or another
        /// user account); on this machine, your own account needs no token by default.
        #[arg(long)]
        print_url: bool,
    },
    /// Run one developer-fee cycle with compressed time (PreWarm → StartSlice → EndSlice).
    FeeTest {
        /// Really open the dev session at PreWarm (authorize only, never a share).
        #[arg(long)]
        connect: bool,
        /// Real milliseconds per virtual second (0 = as fast as possible).
        #[arg(long, default_value_t = 0)]
        pace_ms: u64,
    },
    /// Print the version, the commit and the fee constants hash.
    Version,
    /// Print the sudo command that installs the boot-time GPU clock cap (changes nothing).
    InstallClockCap,
    /// Check or locate the settings file (config.toml).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate config.toml the way the daemon does at start; prints `OK: <path>` or one
    /// `field: problem` line per error (exit status 1).
    Check {
        /// Check this file instead of $XDG_CONFIG_HOME/spark-pearl-miner/config.toml.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Print the absolute path of config.toml.
    Path,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.version {
        print!("{}", spm::version_text());
        return ExitCode::SUCCESS;
    }
    let Some(cmd) = cli.cmd else {
        eprintln!("usage: spark-pearl-miner <command>; see --help");
        return ExitCode::from(2);
    };
    match cmd {
        Cmd::Version => {
            print!("{}", spm::version_text());
            ExitCode::SUCCESS
        }
        Cmd::InstallClockCap => {
            print_clock_cap();
            ExitCode::SUCCESS
        }
        Cmd::GpuWorker { attach, sim, sim_interval_ms } => gpu_worker(attach, sim, sim_interval_ms),
        Cmd::FeeTest { connect, pace_ms } => fee_test(connect, pace_ms),
        Cmd::Daemon { no_api } => runtime().block_on(daemon(no_api)),
        Cmd::Status { json } => runtime().block_on(status(json)),
        Cmd::Start => runtime().block_on(ctl(ControlOp::Start)),
        Cmd::Stop => runtime().block_on(ctl(ControlOp::Stop)),
        Cmd::Pause => runtime().block_on(ctl(ControlOp::Pause)),
        Cmd::Resume => runtime().block_on(ctl(ControlOp::Resume)),
        Cmd::Gui { print_url } => runtime().block_on(gui(print_url)),
        Cmd::Config { cmd: ConfigCmd::Check { file } } => config_check(file),
        Cmd::Config { cmd: ConfigCmd::Path } => {
            println!("{}", absolute(&Paths::from_env().config_file()).display());
            ExitCode::SUCCESS
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build().expect("tokio runtime")
}

fn init_tracing(ring: Option<std::sync::Arc<spm::logring::LogRing>>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()));
    let reg = tracing_subscriber::registry().with(filter).with(fmt);
    match ring {
        Some(r) => reg.with(spm::logring::RingLayer(r)).init(),
        None => reg.init(),
    }
}

async fn daemon(no_api: bool) -> ExitCode {
    let mut opts = spm::daemon::DaemonOptions::new(Paths::from_env());
    opts.api = !no_api;
    init_tracing(Some(opts.logs.clone()));
    tracing::info!("{}", spm::version_text().lines().next().unwrap_or_default());
    let handle = match spm::daemon::start(opts).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("cannot start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("signal handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
    handle.shutdown().await;
    ExitCode::SUCCESS
}

fn gpu_worker(attach: Option<PathBuf>, sim: bool, sim_interval_ms: u64) -> ExitCode {
    init_tracing(None);
    let sock = attach.unwrap_or_else(|| Paths::from_env().worker_sock());
    if !sim {
        tracing::info!(sock = %sock.display(), "GPU worker");
        return match spm_worker::run(spm_worker::Args { sock }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("gpu-worker: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    tracing::info!(sock = %sock.display(), "SIMULATED GPU worker (CPU reference miner, m=n=256, k=2048)");
    let opts = spm::worker_sim::SimOptions {
        sock,
        interval: Duration::from_millis(sim_interval_ms.clamp(50, 60_000)),
        connect_timeout: Duration::from_secs(15),
    };
    match spm::worker_sim::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gpu-worker: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn fee_test(connect: bool, pace_ms: u64) -> ExitCode {
    use spm::feetest::{run_cycle, LiveLogin, OfflineLogin};
    let paths = Paths::from_env();
    let wallet = std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|t| spm_api::Config::from_toml(&t).ok())
        .map(|c| c.miner.wallet)
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "prl1-fee-test".to_string());
    println!("{}", spm_fee::banner());
    println!("fee constants hash: {}", spm_fee::constants_hash());
    println!(
        "One cycle with compressed time: a fresh install owes one {} s slice after {} s of hashing.",
        spm_fee::SLICE_SECS,
        spm_fee::SLICE_SECS * spm_fee::DEBT_DEN / spm_fee::DEBT_NUM
    );
    if connect {
        println!("--connect: PreWarm will really log in to the dev pool (authorize only, no share).");
    }
    let rt = runtime();
    let mut live;
    let mut offline = OfflineLogin;
    let login: &mut dyn spm::feetest::DevLogin = if connect {
        live = LiveLogin { rt: rt.handle().clone() };
        &mut live
    } else {
        &mut offline
    };
    let report = run_cycle(&wallet, spm::paths::unix_ms(), login, Duration::from_millis(pace_ms), 20_000, |ev, note| {
        println!("  t = +{:>5} s  {:<12} {note}", ev.t, format!("{:?}", ev.action));
    });
    if !report.enabled {
        println!("The configured wallet is the fee wallet: the fee is off and no slice happens.");
        return ExitCode::SUCCESS;
    }
    println!(
        "user hashing {} s, dev hashing {} s, measured fee {:.3} %",
        report.user_secs, report.dev_secs, report.measured_pct
    );
    let ok = report.events.iter().map(|e| e.action).collect::<Vec<_>>()
        == [spm_fee::FeeAction::PreWarm, spm_fee::FeeAction::StartSlice, spm_fee::FeeAction::EndSlice];
    println!("{}", if ok { "fee-test OK" } else { "fee-test: the cycle did not complete" });
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn send(req: Request) -> Result<Value, String> {
    let path = Paths::from_env().control_sock();
    control::request(&path, &req).await.map_err(|e| {
        format!(
            "the daemon is not reachable at {} ({e}).\nStart it with: systemctl --user start spark-pearl-miner\n\
             (not installed yet? run packaging/install.sh from the repository)",
            path.display()
        )
    })
}

async fn ctl(op: ControlOp) -> ExitCode {
    match send(Request::Control { op }).await {
        Ok(v) if v["ok"] == true => {
            println!("{}", v["message"].as_str().unwrap_or("ok"));
            ExitCode::SUCCESS
        }
        Ok(v) => {
            eprintln!("{}", v["error"].as_str().unwrap_or("refused"));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn status(json: bool) -> ExitCode {
    let st = match send(Request::Status).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("The miner is not running: {e}");
            // A config.toml the daemon refuses is the usual reason it does not come back up.
            let file = Paths::from_env().config_file();
            if let Some(Err(errors)) = check_config_file(&file) {
                eprintln!("\nconfig.toml has errors, so the daemon refuses to start ({}):", file.display());
                for line in errors {
                    eprintln!("  {line}");
                }
                eprintln!("Fix them (or restore config.toml.bak), then: systemctl --user restart spark-pearl-miner");
            }
            return ExitCode::FAILURE;
        }
    };
    let pools = send(Request::Pools).await.unwrap_or(Value::Null);
    if json {
        println!("{}", serde_json::json!({ "status": st["status"], "pools": pools["pools"] }));
        return ExitCode::SUCCESS;
    }
    let s = &st["status"];
    println!("state        {} ({})", s["state"].as_str().unwrap_or("?"), s["manager"].as_str().unwrap_or(""));
    println!("target       {}", s["mining_target"].as_str().unwrap_or("?"));
    println!(
        "hashrate     {:.1} T-MAC/s (credited, 60 s) | {:.1} (10 s, whole attempts)",
        s["hashrate_tmacs_60s"].as_f64().unwrap_or(0.0),
        s["hashrate_tmacs"].as_f64().unwrap_or(0.0)
    );
    let sh = &s["shares"];
    println!(
        "shares       {} accepted, {} rejected, {} stale, {} discarded (dev {}/{})",
        sh["accepted"], sh["rejected"], sh["stale"], sh["discarded"], sh["dev_accepted"], sh["dev_rejected"]
    );
    let w = &s["worker"];
    println!("worker       {} ({}{})", w["state"].as_str().unwrap_or("?"), w["launch"].as_str().unwrap_or("?"), if w["simulated"] == true { ", SIMULATED" } else { "" });
    println!("fee          {}", s["fee_phase"].as_str().unwrap_or("?"));
    if let Some(slots) = pools["pools"]["slots"].as_array() {
        for p in slots {
            let err = p["last_error"]["code"].as_str().map(|c| format!(" — last error: {c}")).unwrap_or_default();
            println!(
                "pool {}       {}:{} [{}] {}{}",
                p["index"], p["host"].as_str().unwrap_or(""), p["port"], p["tls"].as_str().unwrap_or(""), p["state"].as_str().unwrap_or(""), err
            );
        }
    }
    if let Some(alerts) = s["alerts"].as_array() {
        for a in alerts.iter().take(3) {
            println!("alert        {}", a["msg"].as_str().unwrap_or(""));
        }
    }
    ExitCode::SUCCESS
}

async fn gui(print_url: bool) -> ExitCode {
    let paths = Paths::from_env();
    let api = std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|t| spm_api::Config::from_toml(&t).ok())
        .map(|c| c.api)
        .unwrap_or_default();
    let port = api.port;
    let plain = format!("http://127.0.0.1:{port}/");
    let reachable = || async move { tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() };
    if !reachable().await {
        eprintln!("The daemon is not running; starting it with systemctl --user …");
        let _ = tokio::process::Command::new("systemctl").args(["--user", "start", "spark-pearl-miner.service"]).status().await;
        for _ in 0..50 {
            if reachable().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let token = match std::fs::read_to_string(paths.token_file()) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            eprintln!("cannot read the API token {}: {e} (is the daemon installed?)", paths.token_file().display());
            return ExitCode::FAILURE;
        }
    };
    let url = format!("http://127.0.0.1:{port}/#token={token}");
    if print_url {
        // stdout keeps only the URL (scripts read it); the explanation goes to stderr.
        if api.trust_local_user {
            eprintln!(
                "On this machine, {plain} needs no token for this user account (api.trust_local_user).\n\
                 From another account or machine, use the URL with the token:"
            );
        }
        println!("{url}");
        return ExitCode::SUCCESS;
    }
    match std::process::Command::new("xdg-open").arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        Ok(_) => {
            println!("Opened {plain} in the browser.");
            if api.trust_local_user {
                println!("On this machine, {plain} needs no token for this user account; other accounts use the token (gui --print-url).");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not run xdg-open ({e}); open this URL yourself:\n{url}");
            ExitCode::FAILURE
        }
    }
}

/// The clock-cap installer: the copy next to an installed binary
/// (`<prefix>/share/spark-pearl-miner/install-clockcap.sh`, written by packaging/install.sh),
/// else the one in the source tree.
fn clock_cap_script() -> String {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(|bin| bin.parent()).map(|p| p.join("share/spark-pearl-miner/install-clockcap.sh")))
        .filter(|p| p.is_file());
    match installed {
        Some(p) => p.display().to_string(),
        None => "packaging/install-clockcap.sh".to_string(),
    }
}

fn print_clock_cap() {
    let script = clock_cap_script();
    println!(
        "The boot-time GPU clock cap (nvidia-smi -lgc 300,2000, what the Balanced profile expects)\n\
         needs root once. Nothing was changed: this command only prints. Review the script, then run:\n\n\
         \x20 sudo {script} --apply\n\n\
         It installs /etc/systemd/system/spark-pearl-clockcap.service and enables it at boot.\n\
         Undo:\n\n\
         \x20 sudo systemctl disable --now spark-pearl-clockcap.service\n\
         \x20 sudo rm -f /etc/systemd/system/spark-pearl-clockcap.service\n\
         \x20 sudo systemctl daemon-reload\n"
    );
}

fn absolute(p: &std::path::Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Validate a config file exactly as the daemon does at start (`ConfigService::load_or_init`).
/// None when the file does not exist (the daemon writes the defaults on its first start).
fn check_config_file(file: &std::path::Path) -> Option<Result<(), Vec<String>>> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(vec![format!("cannot read the file: {e}")])),
    };
    let lines = |e: spm_api::config::ConfigError| {
        e.fields()
            .into_iter()
            .map(|f| if f.path.is_empty() { f.message } else { format!("{}: {}", f.path, f.message) })
            .collect::<Vec<_>>()
    };
    Some(match spm_api::Config::from_toml(&text) {
        Ok(cfg) => cfg.validate(spm_api::config::Strictness::File).map_err(lines),
        Err(e) => Err(lines(e)),
    })
}

fn config_check(file: Option<PathBuf>) -> ExitCode {
    let file = absolute(&file.unwrap_or_else(|| Paths::from_env().config_file()));
    match check_config_file(&file) {
        None => {
            println!("OK: {} does not exist yet; the daemon writes the Spark defaults on its first start.", file.display());
            ExitCode::SUCCESS
        }
        Some(Ok(())) => {
            println!("OK: {}", file.display());
            ExitCode::SUCCESS
        }
        Some(Err(errors)) => {
            eprintln!("{} is invalid; the daemon keeps the previous settings while running and refuses to start with it:", file.display());
            for line in errors {
                println!("{line}");
            }
            ExitCode::FAILURE
        }
    }
}
