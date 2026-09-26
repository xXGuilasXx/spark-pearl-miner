//! The fallback busy signal: SM utilization of the other compute processes.
//!
//! The daemon gets per-process utilization from NVML (`spm-governor`, feature `nvml`) or, as a
//! last resort, from `nvidia-smi pmon -c 1 -s u`, whose output this module parses:
//!
//! ```text
//! # gpu         pid   type     sm    mem    enc    dec    jpg    ofa    command
//! # Idx           #    C/G      %      %      %      %      %      %    name
//!     0     1029175     G      5      0      -      -      -      -    gnome-shell
//!     0     1205274     C      -      -      -      -      -      -    VLLM::EngineCor
//! ```
//!
//! Graphics-only processes (the compositor, browsers) never count: only processes with a
//! compute context compete with the miner for the SMs in a way that matters to an LLM server.

/// Utilization of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcUtil {
    pub pid: u32,
    /// Holds a compute context (`C` or `C+G` in pmon).
    pub compute: bool,
    /// SM utilization, percent (0 when the sampler had no sample for it).
    pub sm_pct: u32,
}

/// Parses `nvidia-smi pmon -s u` output. Lines that do not parse are skipped.
pub fn parse_pmon(text: &str) -> Vec<ProcUtil> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 {
                return None;
            }
            let pid: u32 = f[1].parse().ok()?;
            let kind = f[2];
            if !kind.chars().all(|c| matches!(c, 'C' | 'G' | '+')) {
                return None;
            }
            let sm_pct = if f[3] == "-" { 0 } else { f[3].parse().ok()? };
            Some(ProcUtil { pid, compute: kind.contains('C'), sm_pct })
        })
        .collect()
}

/// Total SM utilization of the compute processes other than `exclude` (our worker), capped
/// at 100 %.
pub fn foreign_compute_sm_pct(procs: &[ProcUtil], exclude: &[u32]) -> u32 {
    procs
        .iter()
        .filter(|p| p.compute && !exclude.contains(&p.pid))
        .map(|p| p.sm_pct)
        .fold(0u32, u32::saturating_add)
        .min(100)
}
