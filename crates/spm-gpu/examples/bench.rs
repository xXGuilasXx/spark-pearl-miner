//! Throughput bench of the fused GEMM + hash kernel (strategy C).
//!
//!   cargo run --release -p spm-gpu --features gpu --example bench -- [--seconds 10] [--m 16384]
//!       [--n 16384] [--k 4096] [--band 16] [--chunk-ms 8]
//!
//! Runs attempts back to back for ~`seconds` (each attempt: A-side prep for a new a_noise_seed,
//! then every tile in chunks) and prints credited T-MAC/s (m·n·k per attempt), the SM clock and
//! board power sampled with nvidia-smi, the chunk times, and the rate relative to the MB1 peaks.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spm_gpu::{Job, JobConfig, Operands};

/// MB1 (docs/en/BENCHMARKS.md): register-only IMMA peak at the 2200 MHz cap, and MAC/clk/SM.
const PEAK_2200_TMACS: f64 = 96.0;
const MB1_MAC_PER_CLK_PER_SM: f64 = 919.0;

struct Args {
    m: u32,
    n: u32,
    k: u32,
    seconds: f64,
    band: u32,
    chunk_ms: f64,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args {
        m: 16384,
        n: 16384,
        k: 4096,
        seconds: 10.0,
        band: 0,
        chunk_ms: 8.0,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        let val = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
        match flag.as_str() {
            "--m" => a.m = val.parse()?,
            "--n" => a.n = val.parse()?,
            "--k" => a.k = val.parse()?,
            "--seconds" => a.seconds = val.parse()?,
            "--band" => a.band = val.parse()?,
            "--chunk-ms" => a.chunk_ms = val.parse()?,
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    Ok(a)
}

/// Samples (SM MHz, board W) every 100 ms with nvidia-smi while the bench runs.
struct Sampler {
    child: Option<std::process::Child>,
    samples: Arc<Mutex<Vec<(f64, f64)>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    fn start() -> Self {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let child = Command::new("nvidia-smi")
            .args([
                "--query-gpu=clocks.sm,power.draw",
                "--format=csv,noheader,nounits",
                "-lms",
                "100",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let mut me = Self {
            child,
            samples: samples.clone(),
            reader: None,
        };
        if let Some(out) = me.child.as_mut().and_then(|c| c.stdout.take()) {
            me.reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let mut f = line.split(',').map(|s| s.trim().parse::<f64>());
                    if let (Some(Ok(mhz)), Some(Ok(w))) = (f.next(), f.next()) {
                        if let Ok(mut s) = samples.lock() {
                            s.push((mhz, w));
                        }
                    }
                }
            }));
        }
        me
    }

    fn clear(&self) {
        if let Ok(mut s) = self.samples.lock() {
            s.clear();
        }
    }

    /// (mean MHz, mean W, max W, samples)
    fn stop(mut self) -> (f64, f64, f64, usize) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let s = self.samples.lock().map(|s| s.clone()).unwrap_or_default();
        if s.is_empty() {
            return (0.0, 0.0, 0.0, 0);
        }
        let n = s.len() as f64;
        let mhz = s.iter().map(|x| x.0).sum::<f64>() / n;
        let w = s.iter().map(|x| x.1).sum::<f64>() / n;
        let wmax = s.iter().map(|x| x.1).fold(0.0, f64::max);
        (mhz, w, wmax, s.len())
    }
}

fn seed_bytes(i: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    for (j, b) in s.iter_mut().enumerate() {
        *b = (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> ((j % 8) * 8)) as u8 ^ j as u8;
    }
    s
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let dev = spm_gpu::device_info()?;
    let mut cfg = JobConfig::new(
        args.m,
        args.n,
        args.k,
        Operands::Generated { seed: 0x5eed },
        seed_bytes(u64::MAX),
    );
    cfg.band_rows = args.band;
    cfg.target_chunk = Duration::from_secs_f64(args.chunk_ms / 1e3);
    let t_create = Instant::now();
    let mut job = Job::new(&cfg)?;
    let create = t_create.elapsed();
    let info = job.info()?;
    println!(
        "device {} ({} SMs, cc {}.{}), libspm_cuda: {}",
        dev.name,
        dev.sm_count,
        dev.compute_capability.0,
        dev.compute_capability.1,
        spm_gpu::version()
    );
    println!(
        "job m={} n={} k={}: {} x {} CTA tiles, {} slices, {:.1} MiB on device, B side built in {:.1} ms",
        args.m,
        args.n,
        args.k,
        info.tiles_m,
        info.tiles_n,
        info.k_slices,
        info.device_bytes as f64 / (1 << 20) as f64,
        create.as_secs_f64() * 1e3
    );
    println!(
        "kernel: {} threads, {} regs/thread, {} B local, {} B smem, {} persistent CTAs",
        info.threads, info.regs_per_thread, info.local_bytes, info.smem_bytes, info.ctas
    );

    // Warm-up attempt (also lets the adaptive chunk size settle).
    let never = [0u8; 32];
    job.set_attempt(&seed_bytes(0), &never)?;
    job.run_attempt()?;

    let sampler = Sampler::start();
    std::thread::sleep(Duration::from_millis(150));
    sampler.clear();
    let start = Instant::now();
    let (mut attempts, mut chunks) = (0u64, 0u64);
    let (mut kernel, mut prep) = (Duration::ZERO, Duration::ZERO);
    let mut max_chunk = Duration::ZERO;
    while start.elapsed().as_secs_f64() < args.seconds {
        attempts += 1;
        let t = Instant::now();
        job.set_attempt(&seed_bytes(attempts), &never)?;
        prep += t.elapsed();
        let s = job.run_attempt()?;
        anyhow::ensure!(s.completed, "attempt aborted");
        chunks += u64::from(s.chunks);
        kernel += s.kernel;
        max_chunk = max_chunk.max(s.max_chunk);
    }
    let wall = start.elapsed();
    let (mhz, watts, watts_max, nsamples) = sampler.stop();

    let credited = job.credited_macs() as f64 * attempts as f64;
    let kernel_rate = credited / kernel.as_secs_f64() / 1e12;
    let wall_rate = credited / wall.as_secs_f64() / 1e12;
    let chunk_ms = kernel.as_secs_f64() * 1e3 / chunks.max(1) as f64;
    let peak_at_clock = MB1_MAC_PER_CLK_PER_SM * f64::from(dev.sm_count) * mhz * 1e6 / 1e12;
    println!(
        "{attempts} attempts in {:.2} s ({chunks} chunks, A-side prep {:.2} ms/attempt)",
        wall.as_secs_f64(),
        prep.as_secs_f64() * 1e3 / attempts as f64
    );
    println!("credited T-MAC/s: {kernel_rate:.2} kernel-only, {wall_rate:.2} end-to-end (prep + launch gaps included)");
    println!(
        "chunk: {chunk_ms:.2} ms mean, {:.2} ms max",
        max_chunk.as_secs_f64() * 1e3
    );
    println!(
        "SM clock {mhz:.0} MHz, board power {watts:.1} W mean / {watts_max:.1} W max ({nsamples} nvidia-smi samples)"
    );
    println!(
        "{:.1} % of {PEAK_2200_TMACS} T-MAC/s (MB1 peak at 2200 MHz); {:.1} % of the MB1 peak at the measured clock ({peak_at_clock:.1} T-MAC/s)",
        100.0 * kernel_rate / PEAK_2200_TMACS,
        if peak_at_clock > 0.0 { 100.0 * kernel_rate / peak_at_clock } else { 0.0 }
    );
    if watts > 0.0 {
        println!(
            "energy: {:.2} T-MAC/J end-to-end at the mean board power",
            wall_rate / watts
        );
    }
    Ok(())
}
