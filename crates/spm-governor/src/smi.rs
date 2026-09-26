//! Fallback telemetry through `nvidia-smi`, for when NVML cannot be loaded in-process.
//!
//! The daemon runs, at [`FALLBACK_PERIOD`] (2 Hz: each call forks a process and takes tens of
//! milliseconds):
//!
//! ```text
//! nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active --format=csv,noheader
//! 16.05 W, 2424 MHz, 50, 0x0000000000000000
//! ```
//!
//! and parses the line here. Like NVML it is read-only and never creates a CUDA context.

use std::time::Duration;

/// The `--query-gpu` fields, in the order [`parse_query_line`] expects them.
pub const QUERY_FIELDS: &str = "power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active";
/// Sampling period of the fallback (2 Hz).
pub const FALLBACK_PERIOD: Duration = Duration::from_millis(500);

/// The arguments of the query (`nvidia-smi` plus these).
pub fn query_args() -> [String; 2] {
    [format!("--query-gpu={QUERY_FIELDS}"), "--format=csv,noheader".to_string()]
}

/// One `nvidia-smi` reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmiReading {
    pub power_w: f64,
    pub sm_mhz: u32,
    pub temp_gpu_c: f64,
    /// `clocks_event_reasons.active` bits; `None` when not reported.
    pub event_reasons: Option<u64>,
}

fn field<'a>(f: &[&'a str], i: usize, what: &str) -> Result<&'a str, String> {
    let v = f.get(i).map(|s| s.trim()).ok_or_else(|| format!("nvidia-smi: {what} missing"))?;
    if v.is_empty() || v.contains("N/A") || v.contains("Not Supported") {
        return Err(format!("nvidia-smi: {what} not available ({v})"));
    }
    Ok(v)
}

fn number(v: &str, unit: &str, what: &str) -> Result<f64, String> {
    let n = v.strip_suffix(unit).unwrap_or(v).trim();
    n.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| format!("nvidia-smi: {what} is not a number ({v})"))
}

/// Parses one line of the query (with or without units). Power, clock and temperature are
/// required; the event reasons are optional.
pub fn parse_query_line(line: &str) -> Result<SmiReading, String> {
    let f: Vec<&str> = line.split(',').collect();
    let power_w = number(field(&f, 0, "power.draw")?, "W", "power.draw")?;
    let sm = number(field(&f, 1, "clocks.sm")?, "MHz", "clocks.sm")?;
    let temp_gpu_c = number(field(&f, 2, "temperature.gpu")?, "C", "temperature.gpu")?;
    let event_reasons = field(&f, 3, "clocks_event_reasons.active").ok().and_then(|v| {
        let hex = v.trim_start_matches("0x").trim_start_matches("0X");
        u64::from_str_radix(hex, 16).ok()
    });
    if !(0.0..=100_000.0).contains(&sm) {
        return Err(format!("nvidia-smi: clocks.sm out of range ({sm})"));
    }
    Ok(SmiReading { power_w, sm_mhz: sm as u32, temp_gpu_c, event_reasons })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_gb10_line() {
        let r = parse_query_line("16.05 W, 2424 MHz, 50, 0x0000000000000000\n").unwrap();
        assert_eq!(r, SmiReading { power_w: 16.05, sm_mhz: 2424, temp_gpu_c: 50.0, event_reasons: Some(0) });
        let r = parse_query_line("88.40, 2200, 71, 0x0000000000000004").unwrap();
        assert_eq!((r.power_w, r.sm_mhz, r.event_reasons), (88.4, 2200, Some(4)));
    }

    #[test]
    fn missing_values_are_errors_except_the_reasons() {
        assert!(parse_query_line("[N/A], 2424 MHz, 50, 0x0").is_err());
        assert!(parse_query_line("16 W, [N/A], 50, 0x0").is_err());
        assert!(parse_query_line("16 W, 2424 MHz").is_err());
        assert!(parse_query_line("").is_err());
        let r = parse_query_line("16 W, 2424 MHz, 50, [N/A]").unwrap();
        assert_eq!(r.event_reasons, None);
        assert_eq!(query_args()[0], "--query-gpu=power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active");
    }
}
