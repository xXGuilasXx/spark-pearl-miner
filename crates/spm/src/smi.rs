//! GPU telemetry for the GUI from `nvidia-smi --query-gpu` (NVML underneath). It never creates a
//! CUDA context, so the daemon never shows up as a compute process.

use std::time::Duration;

use spm_api::SmiView;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::daemon::Msg;

const EVERY: Duration = Duration::from_secs(10);
const QUERY: &str = "name,temperature.gpu,power.draw,clocks.sm,clocks.max.sm,utilization.gpu";

fn num(s: &str) -> Option<f32> {
    s.trim().parse().ok()
}

/// Parse one CSV line (`--format=csv,noheader,nounits`); `[N/A]` fields become `None`.
pub fn parse(line: &str) -> Option<SmiView> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 6 || f[0].is_empty() {
        return None;
    }
    Some(SmiView {
        name: f[0].to_string(),
        temperature_c: num(f[1]),
        power_w: num(f[2]),
        sm_clock_mhz: num(f[3]).map(|v| v as u32),
        max_sm_clock_mhz: num(f[4]).map(|v| v as u32),
        utilization_pct: num(f[5]),
        at_ms: crate::paths::unix_ms(),
    })
}

async fn query() -> Option<SmiView> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("nvidia-smi").arg(format!("--query-gpu={QUERY}")).arg("--format=csv,noheader,nounits").kill_on_drop(true).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().next().and_then(parse)
}

/// Poll forever (stops quietly when nvidia-smi is missing).
pub async fn poll(tx: mpsc::UnboundedSender<Msg>) {
    loop {
        let v = query().await;
        let missing = v.is_none();
        if tx.send(Msg::Smi(v)).is_err() {
            return;
        }
        tokio::time::sleep(if missing { EVERY * 6 } else { EVERY }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gb10_line() {
        let v = parse("NVIDIA GB10, 41, 11.62, 208, 3003, 0").unwrap();
        assert_eq!(v.name, "NVIDIA GB10");
        assert_eq!(v.sm_clock_mhz, Some(208));
        let v = parse("NVIDIA GB10, [N/A], [N/A], 208, 3003, [N/A]").unwrap();
        assert_eq!(v.power_w, None);
        assert!(parse("").is_none());
    }
}
