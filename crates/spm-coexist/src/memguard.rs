//! Unified-memory guard.
//!
//! GB10 shares one LPDDR5X pool between CPU and GPU. When it runs short the box does not OOM
//! cleanly: it wedges. On the author's box the orchestration also kills vLLM below 12 GiB
//! available. So the miner stays well clear:
//!
//! * start only if `MemAvailable − budget ≥ 20 GiB` (the budget is what the worker is about to
//!   allocate, ≤ 2 GiB);
//! * while running, exit if `MemAvailable < 16 GiB` or the memory PSI `some avg10 > 10 %`.
//!
//! The gap between 20 and 16 GiB is the hysteresis: after a memory exit the worker comes back
//! only once the start condition holds again.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// One GiB in bytes.
pub const GIB: u64 = 1 << 30;
/// Fixed memory budget of the GPU worker.
pub const WORKER_BUDGET_BYTES: u64 = 2 * GIB;
/// Headroom that must remain after the worker's budget for it to start.
pub const START_HEADROOM_BYTES: u64 = 20 * GIB;
/// Below this much available memory a running worker exits.
pub const EXIT_BELOW_AVAILABLE_BYTES: u64 = 16 * GIB;
/// Above this memory pressure (PSI `some avg10`, percent) a running worker exits.
pub const EXIT_ABOVE_PSI_SOME_AVG10: f64 = 10.0;

/// `/proc/meminfo`.
pub const MEMINFO_PATH: &str = "/proc/meminfo";
/// `/proc/pressure/memory`.
pub const PSI_MEMORY_PATH: &str = "/proc/pressure/memory";

/// `MemAvailable` from `/proc/meminfo` text, in bytes.
pub fn parse_meminfo_available(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let mut it = rest.split_whitespace();
        let value: u64 = it.next()?.parse().ok()?;
        match it.next() {
            Some("kB") | None => value.checked_mul(1024),
            Some(_) => None,
        }
    })
}

/// `some avg10` from `/proc/pressure/memory` text, in percent.
pub fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
        .filter(|v: &f64| v.is_finite())
}

/// A memory reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemSnapshot {
    /// `MemAvailable`, bytes.
    pub available_bytes: u64,
    /// PSI memory `some avg10`, percent; `None` on kernels without PSI.
    pub psi_some_avg10: Option<f64>,
}

/// Reads `/proc/meminfo` and `/proc/pressure/memory` (paths injectable for tests). A missing
/// PSI file is not an error; an unreadable `MemAvailable` is.
pub fn read_snapshot(meminfo: &Path, psi: &Path) -> io::Result<MemSnapshot> {
    let text = fs::read_to_string(meminfo)?;
    let available_bytes = parse_meminfo_available(&text).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "MemAvailable missing from meminfo")
    })?;
    let psi_some_avg10 = fs::read_to_string(psi).ok().and_then(|t| parse_psi_some_avg10(&t));
    Ok(MemSnapshot { available_bytes, psi_some_avg10 })
}

/// Why the worker may not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartRefused {
    pub available_bytes: u64,
    pub budget_bytes: u64,
}

impl fmt::Display for StartRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not starting: {:.1} GiB available minus the {:.1} GiB worker budget leaves less than \
             {} GiB of headroom",
            self.available_bytes as f64 / GIB as f64,
            self.budget_bytes as f64 / GIB as f64,
            START_HEADROOM_BYTES / GIB
        )
    }
}

impl std::error::Error for StartRefused {}

/// Start check: `MemAvailable − budget ≥ 20 GiB`.
pub fn check_start(m: &MemSnapshot, budget_bytes: u64) -> Result<(), StartRefused> {
    if m.available_bytes.saturating_sub(budget_bytes) >= START_HEADROOM_BYTES {
        Ok(())
    } else {
        Err(StartRefused { available_bytes: m.available_bytes, budget_bytes })
    }
}

/// Verdict for a running worker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemVerdict {
    Ok,
    /// `MemAvailable` fell below 16 GiB.
    ExitLowMemory {
        available_bytes: u64,
    },
    /// PSI memory `some avg10` above 10 %.
    ExitPressure {
        some_avg10: f64,
    },
}

impl MemVerdict {
    /// Whether the worker must exit.
    pub fn must_exit(&self) -> bool {
        !matches!(self, MemVerdict::Ok)
    }
}

/// Running check: exit below 16 GiB available or above 10 % pressure.
pub fn check_running(m: &MemSnapshot) -> MemVerdict {
    if m.available_bytes < EXIT_BELOW_AVAILABLE_BYTES {
        return MemVerdict::ExitLowMemory { available_bytes: m.available_bytes };
    }
    match m.psi_some_avg10 {
        Some(p) if p > EXIT_ABOVE_PSI_SOME_AVG10 => MemVerdict::ExitPressure { some_avg10: p },
        _ => MemVerdict::Ok,
    }
}
