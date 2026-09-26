//! `running.marker`: detects that the previous run did not stop cleanly.
//!
//! The daemon writes the marker (durably) before the worker computes anything and removes it on
//! a clean stop. If a marker is already there at start, the previous run ended without cleaning
//! up: a crash, a `SIGKILL`, or the hard power-off this whole crate exists to avoid. The next
//! run then uses a profile one notch more conservative than the one that was running, and the
//! user gets an alert. The step-down applies to that run only; after a clean stop the
//! configured profile is used again, and repeated unclean stops keep stepping down
//! (Max → Balanced → Eco).
//!
//! Location: `$XDG_STATE_HOME/spark-pearl-miner/running.marker` (the daemon picks the path).
//! Format: `key=value` lines (`profile`, `started_unix_s`, `pid`); unknown keys are ignored and
//! an unreadable or empty marker still counts as an unclean stop.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use crate::profile::Profile;

/// File name of the marker inside the daemon's state directory.
pub const MARKER_FILE: &str = "running.marker";

/// What the marker records about a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkerInfo {
    /// The profile the run was using (after any step-down).
    pub profile: Option<Profile>,
    /// Wall-clock start time, seconds since the Unix epoch.
    pub started_unix_s: Option<u64>,
    /// Daemon process id.
    pub pid: Option<u32>,
}

impl MarkerInfo {
    /// The marker's text form.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        if let Some(p) = self.profile {
            s.push_str(&format!("profile={p}\n"));
        }
        if let Some(t) = self.started_unix_s {
            s.push_str(&format!("started_unix_s={t}\n"));
        }
        if let Some(pid) = self.pid {
            s.push_str(&format!("pid={pid}\n"));
        }
        s
    }

    /// Parses a marker leniently: whatever is readable is kept, the rest is `None`.
    pub fn parse(text: &str) -> MarkerInfo {
        let mut info = MarkerInfo::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "profile" => info.profile = v.parse().ok(),
                "started_unix_s" => info.started_unix_s = v.parse().ok(),
                "pid" => info.pid = v.parse().ok(),
                _ => {}
            }
        }
        info
    }
}

/// How the next run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPlan {
    /// The profile to run with.
    pub profile: Profile,
    /// The marker left by the previous run, when it did not stop cleanly.
    pub unclean: Option<MarkerInfo>,
    /// Alert for the user when the previous run did not stop cleanly.
    pub alert: Option<String>,
}

/// Decides the profile for a new run. `previous` is the marker found at start (`None`: the
/// previous run stopped cleanly or this is the first run).
pub fn plan_start(configured: Profile, previous: Option<&MarkerInfo>) -> StartPlan {
    let Some(prev) = previous else {
        return StartPlan { profile: configured, unclean: None, alert: None };
    };
    let ran_with = prev.profile.unwrap_or(configured);
    let profile = configured.min(ran_with.step_down());
    let started = prev
        .started_unix_s
        .map_or_else(|| "at an unknown time".to_string(), |t| format!("at unix time {t}"));
    let alert = format!(
        "The previous run (started {started}, profile {ran_with}) did not stop cleanly: \
         a crash, a kill or a power-off. Running this session at the {profile} profile; \
         see docs/en/POWER-THERMAL.md."
    );
    StartPlan { profile, unclean: Some(prev.clone()), alert: Some(alert) }
}

/// Reads the marker. `Ok(None)` when there is none; an unparsable marker is returned as an
/// empty [`MarkerInfo`] (it still means an unclean stop).
pub fn read_marker(path: &Path) -> io::Result<Option<MarkerInfo>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(MarkerInfo::parse(&String::from_utf8_lossy(&bytes)))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes the marker durably: temporary file, fsync, rename, fsync of the directory. It must be
/// on disk before the GPU is loaded, or a power-off right after start would go unnoticed.
pub fn write_marker(path: &Path, info: &MarkerInfo) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("marker.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(info.encode().as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// Removes the marker on a clean stop. A missing marker is not an error.
pub fn clear_marker(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Start of a run: reads any marker left behind, plans the profile and writes the new marker
/// with the profile actually used. Call [`clear_marker`] on a clean stop.
pub fn begin_run(
    path: &Path,
    configured: Profile,
    now_unix_s: u64,
    pid: u32,
) -> io::Result<StartPlan> {
    let previous = read_marker(path)?;
    let plan = plan_start(configured, previous.as_ref());
    write_marker(
        path,
        &MarkerInfo {
            profile: Some(plan.profile),
            started_unix_s: Some(now_unix_s),
            pid: Some(pid),
        },
    )?;
    Ok(plan)
}
