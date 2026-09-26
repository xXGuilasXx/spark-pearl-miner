//! Memory guard and the SM-utilization fallback on fixture strings.

use std::fs;

use spm_coexist::memguard::{
    check_running, check_start, parse_meminfo_available, parse_psi_some_avg10, read_snapshot,
    MemSnapshot, MemVerdict, GIB, WORKER_BUDGET_BYTES,
};
use spm_coexist::pmon::{foreign_compute_sm_pct, parse_pmon, ProcUtil};

/// The head of `/proc/meminfo` on the author's box with vLLM resident (2026-09-26).
const MEMINFO: &str = "\
MemTotal:       127598548 kB
MemFree:         3316908 kB
MemAvailable:   62482444 kB
Buffers:           52248 kB
Cached:         58112400 kB
SwapCached:            0 kB
";

const PSI_CALM: &str = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=452901083
full avg10=0.00 avg60=0.00 avg300=0.00 total=452372805
";

const PSI_THRASHING: &str = "\
some avg10=23.51 avg60=8.02 avg300=1.97 total=9452901083
full avg10=11.20 avg60=3.10 avg300=0.61 total=4452372805
";

fn snap(available_gib: f64, psi: Option<f64>) -> MemSnapshot {
    MemSnapshot { available_bytes: (available_gib * GIB as f64) as u64, psi_some_avg10: psi }
}

#[test]
fn meminfo_and_psi_parse() {
    assert_eq!(parse_meminfo_available(MEMINFO), Some(62_482_444 * 1024));
    assert_eq!(parse_meminfo_available("MemTotal: 1 kB\n"), None);
    assert_eq!(parse_meminfo_available("MemAvailable: lots kB\n"), None);
    assert_eq!(parse_psi_some_avg10(PSI_CALM), Some(0.0));
    assert_eq!(parse_psi_some_avg10(PSI_THRASHING), Some(23.51));
    assert_eq!(parse_psi_some_avg10("full avg10=5.00 avg60=0 avg300=0 total=1\n"), None);
    assert_eq!(parse_psi_some_avg10(""), None);
}

#[test]
fn start_needs_20_gib_after_the_budget() {
    // 62 GiB available (vLLM resident): fine.
    let today = MemSnapshot {
        available_bytes: parse_meminfo_available(MEMINFO).unwrap(),
        psi_some_avg10: Some(0.0),
    };
    assert!(check_start(&today, WORKER_BUDGET_BYTES).is_ok());
    assert!(check_start(&snap(22.0, None), WORKER_BUDGET_BYTES).is_ok());
    let refused = check_start(&snap(21.9, None), WORKER_BUDGET_BYTES).unwrap_err();
    assert!(refused.to_string().contains("20 GiB"), "{refused}");
    assert!(check_start(&snap(1.0, None), WORKER_BUDGET_BYTES).is_err());
}

#[test]
fn running_exits_below_16_gib_or_above_10_percent_pressure() {
    assert_eq!(check_running(&snap(40.0, Some(0.0))), MemVerdict::Ok);
    assert_eq!(check_running(&snap(16.0, Some(10.0))), MemVerdict::Ok);
    assert!(matches!(check_running(&snap(15.9, Some(0.0))), MemVerdict::ExitLowMemory { .. }));
    assert_eq!(
        check_running(&snap(40.0, Some(10.01))),
        MemVerdict::ExitPressure { some_avg10: 10.01 }
    );
    assert!(check_running(&snap(40.0, parse_psi_some_avg10(PSI_THRASHING))).must_exit());
    // No PSI on this kernel: only the MemAvailable rule applies.
    assert_eq!(check_running(&snap(17.0, None)), MemVerdict::Ok);
}

#[test]
fn hysteresis_between_exit_and_restart() {
    // Exited at 15.9 GiB; at 18 GiB it may keep running but may not start again.
    let s = snap(18.0, Some(0.0));
    assert_eq!(check_running(&s), MemVerdict::Ok);
    assert!(check_start(&s, WORKER_BUDGET_BYTES).is_err());
}

#[test]
fn snapshot_from_files_tolerates_missing_psi() {
    let dir = std::env::temp_dir().join(format!("spm-coexist-mem-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let meminfo = dir.join("meminfo");
    fs::write(&meminfo, MEMINFO).unwrap();
    let psi = dir.join("memory");
    let s = read_snapshot(&meminfo, &psi).unwrap();
    assert_eq!(s.psi_some_avg10, None);
    fs::write(&psi, PSI_THRASHING).unwrap();
    assert_eq!(read_snapshot(&meminfo, &psi).unwrap().psi_some_avg10, Some(23.51));
    fs::write(&meminfo, "MemTotal: 1 kB\n").unwrap();
    assert!(read_snapshot(&meminfo, &psi).is_err());
    fs::remove_dir_all(&dir).unwrap();
}

/// `nvidia-smi pmon -c 1 -s u` on the author's box (desktop session, idle vLLM).
const PMON: &str = "\
# gpu         pid   type     sm    mem    enc    dec    jpg    ofa    command
# Idx           #    C/G      %      %      %      %      %      %    name
    0     443959     G      -      -      -      -      -      -    firefox        
    0    1029175     G      5      0      -      -      -      -    gnome-shell
    0    1205274     C      -      -      -      -      -      -    VLLM::EngineCor
";

#[test]
fn pmon_idle_vllm_and_graphics_do_not_count() {
    let procs = parse_pmon(PMON);
    assert_eq!(procs.len(), 3);
    assert_eq!(procs[1], ProcUtil { pid: 1029175, compute: false, sm_pct: 5 });
    assert_eq!(foreign_compute_sm_pct(&procs, &[]), 0);
}

#[test]
fn pmon_busy_vllm_counts_and_our_worker_is_excluded() {
    let busy = PMON.replace("1205274     C      -", "1205274     C     87")
        + "    0       4242     C     99      3      -      -      -      -    spark-pearl-min\n"
        + "    0       5555   C+G      8      0      -      -      -      -    other\n";
    let procs = parse_pmon(&busy);
    assert_eq!(foreign_compute_sm_pct(&procs, &[4242]), 95);
    assert_eq!(foreign_compute_sm_pct(&procs, &[]), 100);
}
