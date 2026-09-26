//! Throughput bench of the fused v0 kernel (gemm_v0_cpasync) on a generated job.
//!
//! ```text
//! cargo run --release -p spm-gpu --example bench -- [--m 16384] [--n 16384] [--k 4096] [--seconds 10] [--chunk CTAS]
//! ```
//!
//! Prints credited T-MAC/s (m·n·k per attempt over the wall time of the attempt, with and without
//! the A-side prep), the SM clock sampled with nvidia-smi while it runs, the chunk times and the
//! share of the 96.0 T-MAC/s register-only IMMA peak measured at 2200 MHz (docs/en/BENCHMARKS.md).
//! The default shape and duration fit the "short GPU test" rule of this machine (~10 s, < 2 GiB).

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use spm_gpu::{config52_for, device_info, ChunkStatus, Job, JobParams, Matrices};

/// Register-only IMMA peak at the 2200 MHz cap (MB1).
const PEAK_2200_TMACS: f64 = 96.0;
/// Register-only IMMA rate per SM and clock (MB1: 919 MAC/clk/SM at stock, 914 at 2200 MHz).
const PEAK_MAC_PER_CLK_SM: f64 = 919.0;

struct Args {
    m: u32,
    n: u32,
    k: u32,
    seconds: f64,
    chunk: Option<u32>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        m: 16384,
        n: 16384,
        k: 4096,
        seconds: 10.0,
        chunk: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--m" => a.m = value()?.parse().map_err(|e| format!("--m: {e}"))?,
            "--n" => a.n = value()?.parse().map_err(|e| format!("--n: {e}"))?,
            "--k" => a.k = value()?.parse().map_err(|e| format!("--k: {e}"))?,
            "--seconds" => a.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?,
            "--chunk" => a.chunk = Some(value()?.parse().map_err(|e| format!("--chunk: {e}"))?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

/// Current SM clock (MHz) and board power (W) from nvidia-smi, if available.
fn sample_gpu() -> Option<(f64, Option<f64>)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=clocks.sm,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.lines().next()?.split(',').map(str::trim);
    let clock = fields.next()?.parse().ok()?;
    let power = fields.next().and_then(|w| w.parse().ok());
    Some((clock, power))
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(0.0, f64::max);
    (mean, min, max)
}

fn main() -> anyhow::Result<()> {
    let args = parse_args().map_err(anyhow::Error::msg)?;
    let dev = device_info()?;
    println!(
        "device: {} ({} SMs, cc {}.{}, max clock {} MHz)",
        dev.name,
        dev.sm_count,
        dev.compute_capability.0,
        dev.compute_capability.1,
        dev.sm_clock_mhz
    );

    let mut job = Job::new(&JobParams {
        m: args.m,
        n: args.n,
        k: args.k,
        config52: config52_for(args.k),
        matrices: Matrices::Generated { seed: 0x5eed },
        b_noise_seed: [0x5b; 32],
        bound: [0; 32],
        dump: false,
        chunk_ctas: args.chunk,
        hit_capacity: None,
    })?;
    let info = job.info()?;
    println!(
        "job: m={} n={} k={} | CTA {}x{} | {} CTA tiles in {} chunks of {} | {} CTA/SM, {} B smem | device memory {} B ({:.1} MiB) | create+B prep {:.2} ms",
        info.m,
        info.n,
        info.k,
        info.block.0,
        info.block.1,
        info.cta_tiles,
        info.chunks,
        info.chunk_ctas,
        info.ctas_per_sm,
        info.smem_bytes,
        info.device_bytes,
        info.device_bytes as f64 / (1u64 << 20) as f64,
        info.create_prep_ms
    );
    let macs = info.credited_macs() as f64;

    // Warm-up attempt, chunk by chunk, to record the GPU time of every chunk.
    let mut seed = [0u8; 32];
    let mut chunk_ms = Vec::new();
    job.set_attempt(&seed, None)?;
    loop {
        let st = job.run_chunk()?;
        chunk_ms.push(f64::from(job.info()?.last_chunk_ms));
        if st == ChunkStatus::Done {
            break;
        }
    }

    // Clock and power sampler.
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new((Vec::<f64>::new(), Vec::<f64>::new())));
    let sampler = {
        let stop = Arc::clone(&stop);
        let samples = Arc::clone(&samples);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some((clock, power)) = sample_gpu() {
                    if let Ok(mut s) = samples.lock() {
                        s.0.push(clock);
                        s.1.extend(power);
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };

    let abort = AtomicU32::new(0);
    let mut attempts = 0u64;
    let (mut gemm_s, mut prep_s, mut prep_gpu_ms) = (0.0f64, 0.0f64, Vec::new());
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < args.seconds {
        seed[..8].copy_from_slice(&(attempts + 1).to_le_bytes());
        let t0 = Instant::now();
        job.set_attempt(&seed, None)?;
        let t1 = Instant::now();
        let st = job.run_attempt(&abort)?;
        let t2 = Instant::now();
        anyhow::ensure!(st == ChunkStatus::Done, "attempt ended with {st:?}");
        prep_s += (t1 - t0).as_secs_f64();
        gemm_s += (t2 - t1).as_secs_f64();
        prep_gpu_ms.push(f64::from(job.info()?.last_prep_ms));
        attempts += 1;
    }
    let wall = start.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    sampler.join().ok();
    let (clocks, watts) = samples.lock().map(|s| s.clone()).unwrap_or_default();
    let (clk, clk_min, clk_max) = stats(&clocks);
    let (w_mean, _, w_max) = stats(&watts);
    let (c_mean, c_min, c_max) = stats(&chunk_ms);
    let (p_mean, _, _) = stats(&prep_gpu_ms);

    let tmacs_gemm = macs * attempts as f64 / gemm_s / 1e12;
    let tmacs_total = macs * attempts as f64 / (gemm_s + prep_s) / 1e12;
    let peak_at_clock = PEAK_MAC_PER_CLK_SM * f64::from(dev.sm_count) * clk * 1e6 / 1e12;
    println!("attempts: {attempts} in {wall:.2} s (GEMM {gemm_s:.2} s, A prep {prep_s:.2} s; A prep GPU {p_mean:.2} ms/attempt)");
    println!(
        "chunk GPU time: mean {c_mean:.2} ms, min {c_min:.2} ms, max {c_max:.2} ms ({} chunks)",
        chunk_ms.len()
    );
    println!("SM clock (nvidia-smi): mean {clk:.0} MHz (min {clk_min:.0}, max {clk_max:.0}), power mean {w_mean:.1} W, max {w_max:.1} W");
    println!(
        "credited: {tmacs_gemm:.2} T-MAC/s (fused kernel) | {tmacs_total:.2} T-MAC/s (incl. A prep) | {:.1} % of {PEAK_2200_TMACS} T-MAC/s | {:.1} % of the register-only peak at {clk:.0} MHz ({peak_at_clock:.1} T-MAC/s)",
        100.0 * tmacs_gemm / PEAK_2200_TMACS,
        if peak_at_clock > 0.0 { 100.0 * tmacs_gemm / peak_at_clock } else { 0.0 }
    );
    Ok(())
}
