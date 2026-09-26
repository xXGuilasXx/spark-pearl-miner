//! Prints what the governor would see: NVML power, SM clock, GPU temperature, clock event
//! reasons, the hottest acpitz zone and per-process SM utilization, at 10 Hz. Read-only.
//!
//! `cargo run --release -p spm-governor --features nvml --example telemetry -- [seconds]`

use std::path::Path;
use std::time::{Duration, Instant};

use spm_governor::nvml::NvmlSampler;
use spm_governor::thermal::{read_acpitz_max_c, THERMAL_ROOT};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2);
    let mut nvml = NvmlSampler::init(0)?;
    let start = Instant::now();
    println!("t_s,power_w,sm_mhz,temp_gpu_c,acpitz_max_c,event_reasons");
    while start.elapsed() < Duration::from_secs(seconds) {
        let r = nvml.read()?;
        let acpitz = read_acpitz_max_c(Path::new(THERMAL_ROOT))?;
        println!(
            "{:.1},{:.2},{},{:.0},{},{}",
            start.elapsed().as_secs_f64(),
            r.power_w,
            r.sm_mhz,
            r.temp_gpu_c,
            acpitz.map_or("-".into(), |t| format!("{t:.1}")),
            r.event_reasons.map_or("-".into(), |b| format!("{b:#018x}")),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    for p in nvml.process_sm_util()? {
        println!("pid {} sm {}% compute {}", p.pid, p.sm_pct, p.compute);
    }
    Ok(())
}
