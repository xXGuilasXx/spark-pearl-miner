//! Polls the vLLM metrics endpoint and prints what the yield gate would decide. Read-only.
//!
//! `cargo run --release -p spm-coexist --example vllm_load -- [url] [seconds]`

use std::time::{Duration, Instant};

use spm_coexist::http::{fetch_vllm_load, HttpUrl};
use spm_coexist::{CoexistConfig, LoadSignal, YieldGate, DEFAULT_METRICS_URL, METRICS_TIMEOUT};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = HttpUrl::parse(&args.next().unwrap_or_else(|| DEFAULT_METRICS_URL.to_string()))?;
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let cfg = CoexistConfig::default();
    let mut gate = YieldGate::new(&cfg);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let t0 = Instant::now();
        let signal = match fetch_vllm_load(&url, METRICS_TIMEOUT).await {
            Ok(load) => LoadSignal::Vllm(load),
            Err(e) => {
                eprintln!("{e}");
                LoadSignal::Unavailable
            }
        };
        let fetch_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let decision = gate.observe(start.elapsed(), signal);
        println!(
            "{:>6.2}s fetch {fetch_ms:5.1} ms  {signal:?} -> {decision:?}",
            start.elapsed().as_secs_f64()
        );
        tokio::time::sleep(cfg.poll_interval()).await;
    }
    Ok(())
}
