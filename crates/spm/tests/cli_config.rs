//! `spark-pearl-miner config check|path`: what the systemd unit's ExecStartPre and the installer
//! rely on. Runs the real binary against files in a temp dir; no daemon, no port, no GPU.

use std::path::Path;
use std::process::{Command, Output};

fn cli(xdg_config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spark-pearl-miner"))
        .args(args)
        .env("XDG_CONFIG_HOME", xdg_config)
        .output()
        .expect("run spark-pearl-miner")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("spm-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn config_path_follows_xdg_config_home() {
    let d = temp_dir("path");
    let out = cli(&d, &["config", "path"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), d.join("spark-pearl-miner/config.toml").display().to_string());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn config_check_accepts_a_missing_file_and_the_defaults() {
    let d = temp_dir("ok");
    // No file yet: the daemon writes the defaults on its first start, so ExecStartPre must pass.
    let out = cli(&d, &["config", "check"]);
    assert!(out.status.success(), "{out:?}");
    let file = d.join("spark-pearl-miner/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, spm_api::Config::default().to_toml()).unwrap();
    let out = cli(&d, &["config", "check"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("OK: "));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn config_check_lists_every_error_and_fails() {
    let d = temp_dir("bad");
    let mut cfg = spm_api::Config::default();
    cfg.miner.wallet = "prl1qnotawallet".into();
    cfg.miner.worker = "has space".into();
    let file = d.join("bad.toml");
    std::fs::write(&file, cfg.to_toml()).unwrap();
    let out = cli(&d, &["config", "check", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().any(|l| l.starts_with("miner.wallet: ")), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("miner.worker: ")), "{stdout}");

    // A fee key is refused like everywhere else.
    std::fs::write(&file, format!("{}\ndev_fee = 0\n", spm_api::Config::default().to_toml())).unwrap();
    let out = cli(&d, &["config", "check", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("dev_fee"));
    let _ = std::fs::remove_dir_all(&d);
}
