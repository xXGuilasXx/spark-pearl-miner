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

#[derive(Debug, Clone, Default)]
struct Samples {
    clock_mhz: Vec<f64>,
    power_w: Vec<f64>,
    others: Vec<String>,
}

/// Compute processes on the GPU other than this one and the resident vLLM engine.
fn other_compute_processes() -> Vec<String> {
    let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name",
            "--format=csv,noheader",
        ])
        .output()
    else {
        return Vec::new();
    };
    let me = std::process::id().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.contains("VLLM") && !l.trim().is_empty())
        .filter(|l| l.split(',').next().map(str::trim) != Some(me.as_str()))
        .map(|l| l.trim().to_string())
        .collect()
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

    // Clock, power and "who else is on the GPU" sampler.
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Samples::default()));
    let sampler = {
        let stop = Arc::clone(&stop);
        let samples = Arc::clone(&samples);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let gpu = sample_gpu();
                let others = other_compute_processes();
                if let Ok(mut s) = samples.lock() {
                    if let Some((clock, power)) = gpu {
                        s.clock_mhz.push(clock);
                        s.power_w.extend(power);
                    }
                    s.others.extend(others);
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };

    // Phase 1 (kernel only): whole attempts run chunk by chunk, each chunk timed with CUDA events,
    // so the rate excludes the host, the A-side prep and whatever else runs between chunks.
    let mut seed = [0u8; 32];
    let mut attempts = 0u64;
    let mut chunk_ms = Vec::new();
    let mut kernel_attempt_ms = Vec::new();
    let start = Instant::now();
    while attempts < 3 || start.elapsed().as_secs_f64() < args.seconds * 0.25 {
        attempts += 1;
        seed[..8].copy_from_slice(&attempts.to_le_bytes());
        job.set_attempt(&seed, None)?;
        let mut sum = 0.0;
        loop {
            let st = job.run_chunk()?;
            let ms = f64::from(job.info()?.last_chunk_ms);
            chunk_ms.push(ms);
            sum += ms;
            if st == ChunkStatus::Done {
                break;
            }
        }
        kernel_attempt_ms.push(sum);
    }
    let kernel_attempts = attempts;

    // Phase 2 (sustained): back-to-back attempts, chunks pipelined two deep (run_attempt), timed
    // on the wall clock -- what a miner gets on this machine right now, vLLM included.
    let abort = AtomicU32::new(0);
    let (mut gemm_s, mut prep_s, mut prep_gpu_ms) = (0.0f64, 0.0f64, Vec::new());
    let mut sustained = 0u64;
    let phase2 = Instant::now();
    while start.elapsed().as_secs_f64() < args.seconds {
        attempts += 1;
        seed[..8].copy_from_slice(&attempts.to_le_bytes());
        let t0 = Instant::now();
        job.set_attempt(&seed, None)?;
        let t1 = Instant::now();
        let st = job.run_attempt(&abort)?;
        let t2 = Instant::now();
        anyhow::ensure!(st == ChunkStatus::Done, "attempt ended with {st:?}");
        prep_s += (t1 - t0).as_secs_f64();
        gemm_s += (t2 - t1).as_secs_f64();
        prep_gpu_ms.push(f64::from(job.info()?.last_prep_ms));
        sustained += 1;
    }
    let wall2 = phase2.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    sampler.join().ok();
    let samples = samples.lock().map(|s| s.clone()).unwrap_or_default();
    let (clk, clk_min, clk_max) = stats(&samples.clock_mhz);
    let (w_mean, _, w_max) = stats(&samples.power_w);
    let (c_mean, c_min, c_max) = stats(&chunk_ms);
    let (p_mean, _, _) = stats(&prep_gpu_ms);
    let (k_mean, k_best, _) = stats(&kernel_attempt_ms);

    let tmacs_kernel = macs / (k_mean / 1e3) / 1e12;
    let tmacs_kernel_best = macs / (k_best / 1e3) / 1e12;
    let tmacs_gemm = macs * sustained as f64 / gemm_s / 1e12;
    let tmacs_total = macs * sustained as f64 / (gemm_s + prep_s) / 1e12;
    let peak_at_clock = PEAK_MAC_PER_CLK_SM * f64::from(dev.sm_count) * clk * 1e6 / 1e12;
    let pct = |x: f64| 100.0 * x / PEAK_2200_TMACS;
    let pct_clk = |x: f64| {
        if peak_at_clock > 0.0 {
            100.0 * x / peak_at_clock
        } else {
            0.0
        }
    };
    println!(
        "chunks: {} timed, GPU time mean {c_mean:.2} ms, min {c_min:.2} ms, max {c_max:.2} ms",
        chunk_ms.len()
    );
    println!(
        "SM clock (nvidia-smi): mean {clk:.0} MHz (min {clk_min:.0}, max {clk_max:.0}); power mean {w_mean:.1} W, max {w_max:.1} W"
    );
    println!(
        "kernel only ({kernel_attempts} attempts, sum of chunk GPU times): {tmacs_kernel:.2} T-MAC/s mean, {tmacs_kernel_best:.2} best = {:.1} % of {PEAK_2200_TMACS} T-MAC/s, {:.1} % of the register-only peak at {clk:.0} MHz ({peak_at_clock:.1} T-MAC/s)",
        pct(tmacs_kernel),
        pct_clk(tmacs_kernel)
    );
    if sustained > 0 {
        println!(
            "sustained ({sustained} attempts in {wall2:.2} s, wall clock): {tmacs_gemm:.2} T-MAC/s fused kernel, {tmacs_total:.2} T-MAC/s incl. A prep ({p_mean:.2} ms GPU per attempt) = {:.1} % of {PEAK_2200_TMACS} T-MAC/s",
            pct(tmacs_gemm)
        );
    }
    let mut others: Vec<String> = samples.others.into_iter().collect();
    others.sort();
    others.dedup();
    if others.is_empty() {
        println!("other CUDA processes seen during the run: none besides the resident vLLM");
    } else {
        println!(
            "other CUDA processes seen during the run (numbers are contended): {}",
            others.join(", ")
        );
    }
    Ok(())
}
