//! Where things live (XDG base directories).
//!
//! | What | Path |
//! |---|---|
//! | config | `$XDG_CONFIG_HOME/spark-pearl-miner/config.toml` (+ `config.toml.bak`) |
//! | API token | `$XDG_CONFIG_HOME/spark-pearl-miner/api-token` (0600) |
//! | state | `$XDG_STATE_HOME/spark-pearl-miner/state.json` |
//! | config audit | `$XDG_STATE_HOME/spark-pearl-miner/audit.log` |
//! | sockets | `$XDG_RUNTIME_DIR/spark-pearl-miner/{control,worker}.sock` (dir 0700, sockets 0600) |

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const APP: &str = "spark-pearl-miner";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute())
}

impl Paths {
    /// From the XDG variables, falling back to `~/.config`, `~/.local/state` and
    /// `/run/user/<uid>` (or the state dir when there is no runtime dir).
    pub fn from_env() -> Paths {
        let home = env_dir("HOME").unwrap_or_else(|| PathBuf::from("/tmp"));
        let config = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join(APP);
        let state = env_dir("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")).join(APP);
        let runtime = env_dir("XDG_RUNTIME_DIR").map(|d| d.join(APP)).unwrap_or_else(|| state.join("run"));
        Paths { config_dir: config, state_dir: state, runtime_dir: runtime }
    }

    /// Everything under one directory (tests).
    pub fn under(root: &Path) -> Paths {
        Paths { config_dir: root.join("config"), state_dir: root.join("state"), runtime_dir: root.join("run") }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn token_file(&self) -> PathBuf {
        self.config_dir.join("api-token")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    pub fn audit_file(&self) -> PathBuf {
        self.state_dir.join("audit.log")
    }
    pub fn control_sock(&self) -> PathBuf {
        self.runtime_dir.join("control.sock")
    }
    pub fn worker_sock(&self) -> PathBuf {
        self.runtime_dir.join("worker.sock")
    }

    /// Create the directories: config and state 0700, runtime 0700.
    pub fn ensure(&self) -> io::Result<()> {
        for d in [&self.config_dir, &self.state_dir, &self.runtime_dir] {
            fs::create_dir_all(d)?;
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Write `data` to `path` atomically (temp file in the same directory, fsync, rename) with `mode`.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Unix time in milliseconds.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Unix time in seconds.
pub fn unix_s() -> u64 {
    unix_ms() / 1000
}

/// `2026-09-26T19:15:02Z` from Unix milliseconds (UTC, no dependency).
pub fn iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_790_445_863_721), "2026-09-26T18:04:23Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn layout() {
        let p = Paths::under(Path::new("/x"));
        assert_eq!(p.config_file(), PathBuf::from("/x/config/config.toml"));
        assert_eq!(p.worker_sock(), PathBuf::from("/x/run/worker.sock"));
    }
}
