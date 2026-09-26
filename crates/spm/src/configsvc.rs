//! ConfigService: loads, validates, saves (atomic write + `.bak`) and hot-reloads
//! `config.toml`, and appends every change to an audit log with its source.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use spm_api::config::{Config, ConfigError, Strictness};
use spm_api::ChangeSource;

use crate::paths::{iso8601, unix_ms, write_atomic, Paths};

/// What a reload found.
#[derive(Debug)]
pub enum Reload {
    Unchanged,
    Changed(Box<Config>),
    /// The file changed but is invalid: the previous configuration stays in force.
    Invalid(ConfigError),
}

#[derive(Debug)]
pub struct ConfigService {
    file: PathBuf,
    audit: PathBuf,
    current: Config,
    stamp: Option<(SystemTime, u64)>,
}

fn stamp_of(path: &PathBuf) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Keys whose change is worth auditing, in a stable order.
fn changed_keys(old: &Config, new: &Config) -> Vec<String> {
    let a = serde_json::to_value(old).unwrap_or_default();
    let b = serde_json::to_value(new).unwrap_or_default();
    let mut out = Vec::new();
    if let (Some(a), Some(b)) = (a.as_object(), b.as_object()) {
        for (k, va) in a {
            match (va.as_object(), b.get(k).and_then(|v| v.as_object())) {
                (Some(sa), Some(sb)) => {
                    for (kk, vv) in sa {
                        if sb.get(kk) != Some(vv) {
                            out.push(format!("{k}.{kk}"));
                        }
                    }
                }
                _ => {
                    if b.get(k) != Some(va) {
                        out.push(k.clone());
                    }
                }
            }
        }
    }
    out
}

impl ConfigService {
    /// Load the file, writing the defaults when it does not exist. An invalid file is an error
    /// (the daemon refuses to start rather than guess).
    pub fn load_or_init(paths: &Paths) -> Result<ConfigService, String> {
        let file = paths.config_file();
        let audit = paths.audit_file();
        let current = match fs::read_to_string(&file) {
            Ok(text) => {
                let cfg = Config::from_toml(&text).map_err(|e| format!("{}: {e}", file.display()))?;
                cfg.validate(Strictness::File).map_err(|e| format!("{}: {e}", file.display()))?;
                cfg
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cfg = Config::default();
                write_atomic(&file, cfg.to_toml().as_bytes(), 0o600).map_err(|e| format!("{}: {e}", file.display()))?;
                cfg
            }
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        let stamp = stamp_of(&file);
        Ok(ConfigService { file, audit, current, stamp })
    }

    pub fn current(&self) -> &Config {
        &self.current
    }

    pub fn path(&self) -> &PathBuf {
        &self.file
    }

    /// Save a validated configuration: the previous file becomes `config.toml.bak`, the new one is
    /// written atomically (0600) and the change is audited. Returns whether the wallet changed.
    pub fn save(&mut self, cfg: Config, source: ChangeSource) -> io::Result<bool> {
        if self.file.exists() {
            let bak = self.file.with_extension("toml.bak");
            let old = fs::read(&self.file)?;
            write_atomic(&bak, &old, 0o600)?;
        }
        write_atomic(&self.file, cfg.to_toml().as_bytes(), 0o600)?;
        self.stamp = stamp_of(&self.file);
        let wallet_changed = self.audit(&cfg, source);
        self.current = cfg;
        Ok(wallet_changed)
    }

    fn audit(&self, new: &Config, source: ChangeSource) -> bool {
        let keys = changed_keys(&self.current, new);
        let wallet_changed = self.current.miner.wallet != new.miner.wallet;
        let line = serde_json::json!({
            "at": iso8601(unix_ms()),
            "source": source.as_str(),
            "changed": keys,
            "wallet_changed": wallet_changed,
            "wallet": spm_api::views::abbreviate_wallet(&new.miner.wallet),
        });
        let res = fs::OpenOptions::new().create(true).append(true).open(&self.audit).and_then(|mut f| {
            writeln!(f, "{line}")
        });
        if let Err(e) = res {
            tracing::warn!(error = %e, "could not append to the config audit log");
        }
        tracing::info!(source = source.as_str(), changed = ?keys, wallet_changed, "configuration changed");
        wallet_changed
    }

    /// Check the file for edits made outside the daemon (hot reload).
    pub fn poll(&mut self) -> Reload {
        let stamp = stamp_of(&self.file);
        if stamp == self.stamp {
            return Reload::Unchanged;
        }
        self.stamp = stamp;
        let text = match fs::read_to_string(&self.file) {
            Ok(t) => t,
            Err(e) => return Reload::Invalid(ConfigError::Parse(e.to_string())),
        };
        let cfg = match Config::from_toml(&text) {
            Ok(c) => c,
            Err(e) => return Reload::Invalid(e),
        };
        if let Err(e) = cfg.validate(Strictness::File) {
            return Reload::Invalid(e);
        }
        if cfg == self.current {
            return Reload::Unchanged;
        }
        Reload::Changed(Box::new(cfg))
    }

    /// Adopt a configuration read by [`ConfigService::poll`] (audited as a file edit).
    pub fn adopt(&mut self, cfg: Config) -> bool {
        let wallet_changed = self.audit(&cfg, ChangeSource::File);
        self.current = cfg;
        wallet_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spm-configsvc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn init_save_backup_and_hot_reload() {
        let root = tmp("a");
        let paths = Paths::under(&root);
        paths.ensure().unwrap();
        let mut svc = ConfigService::load_or_init(&paths).unwrap();
        assert!(paths.config_file().exists());
        let mut cfg = svc.current().clone();
        cfg.miner.wallet = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh".into();
        cfg.pools[0].tls = spm_api::config::TlsSetting::Off;
        assert!(svc.save(cfg.clone(), ChangeSource::Api).unwrap());
        assert!(paths.config_file().with_extension("toml.bak").exists());
        assert!(matches!(svc.poll(), Reload::Unchanged));
        // TLS mode persists across a reload from disk.
        let again = ConfigService::load_or_init(&paths).unwrap();
        assert_eq!(again.current().pools[0].tls, spm_api::config::TlsSetting::Off);
        // An outside edit is picked up; an invalid one is reported and ignored.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let text = fs::read_to_string(paths.config_file()).unwrap().replace("worker = \"spark\"", "worker = \"rig7\"");
        fs::write(paths.config_file(), text).unwrap();
        match svc.poll() {
            Reload::Changed(c) => {
                assert_eq!(c.miner.worker, "rig7");
                svc.adopt(*c);
            }
            other => panic!("{other:?}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(paths.config_file(), "schema_version = 1\n[miner]\ndev_fee = 0\n").unwrap();
        assert!(matches!(svc.poll(), Reload::Invalid(ConfigError::FeeKey(_))));
        assert_eq!(svc.current().miner.worker, "rig7");
        let audit = fs::read_to_string(paths.audit_file()).unwrap();
        assert_eq!(audit.lines().count(), 2);
        assert!(audit.contains("\"source\":\"api\"") && audit.contains("\"source\":\"file\""));
        fs::remove_dir_all(&root).unwrap();
    }
}
