//! Throughput bench of the fused GEMM + transcript kernel (docs/en/KERNEL.md harness).
//!
//! ```text
//! cargo run --release -p spm-gpu --example bench -- [--m 16384] [--n 16384] [--k 4096]
//!     [--seconds 10] [--chunk-ctas 0] [--per-chunk] [--csv out.csv]
//! ```
//!
//! Repeats full attempts (A-side prep + `Job::run`, the production path) for
//! ~`seconds` on a GPU-generated problem and prints the credited rate (m·n·k MACs per full pass)
//! twice: over the kernel's GPU time only, and over wall time including the per-attempt A-side
//! prep. `--per-chunk` drives the attempt with explicit `run_chunk` calls instead and records every
//! chunk's size and time (percentiles, CSV). The SM clock, GPU power and
//! temperature come from `nvidia-smi` sampled every 200 ms during the run (an idle sample is
//! taken first, so the power the kernel adds is visible even with other processes resident).
//! The default shape (16384² × 4096, ~150 MiB of device memory) fits the "short GPU test"
//! rule of this machine.
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use spm_gpu::{Chunk, Job, JobParams, Run, Source};

/// Register-only IMMA peak at the 2200 MHz cap (docs/en/BENCHMARKS.md, MB1).
const PEAK_CAPPED_TMACS: f64 = 96.0;
/// MB1: sustained register-only MACs per clock per SM.
const PEAK_MAC_PER_CLK_SM: f64 = 919.0;

struct Args {
    m: u32,
    n: u32,
    k: u32,
    seconds: f64,
    chunk_ctas: u32,
    per_chunk: bool,
    csv: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        m: 16384,
        n: 16384,
        k: 4096,
        seconds: 10.0,
        chunk_ctas: 0,
        per_chunk: false,
        csv: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--m" => a.m = value()?.parse()?,
            "--n" => a.n = value()?.parse()?,
            "--k" => a.k = value()?.parse()?,
            "--seconds" => a.seconds = value()?.parse()?,
            "--chunk-ctas" => a.chunk_ctas = value()?.parse()?,
            "--csv" => a.csv = Some(value()?),
            "--per-chunk" => a.per_chunk = true,
            "-h" | "--help" => {
                println!(
                    "bench [--m 16384] [--n 16384] [--k 4096] [--seconds 10] [--chunk-ctas 0] [--per-chunk] [--csv FILE]"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    sm_mhz: f64,
    watts: f64,
    temp_c: f64,
    util: f64,
    /// `clocks_event_reasons.active` bitmask (why the clock is below its maximum).
    reasons: u64,
}

/// Runs `nvidia-smi <args> -lms <period>` in the background and hands every output line, with
/// its arrival time, to `on_line`.
struct Poller {
    child: Option<Child>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Poller {
    fn start(
        args: &[&str],
        period_ms: u32,
        mut on_line: impl FnMut(Instant, &str) + Send + 'static,
    ) -> Self {
        let period = period_ms.to_string();
        let child = Command::new("nvidia-smi")
            .args(args)
            .args(["-lms", period.as_str()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let mut me = Self {
            child,
            reader: None,
        };
        if let Some(out) = me.child.as_mut().and_then(|c| c.stdout.take()) {
            me.reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    on_line(Instant::now(), &line);
                }
            }));
        }
        me
    }

    fn stop(mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// Clock/power/temperature/utilization samples every 200 ms, and the other processes holding a
/// CUDA context (polled every 250 ms), so a run disturbed by another GPU user is visible in the
/// report.
struct Sampler {
    samples: Arc<Mutex<Vec<Sample>>>,
    others: Arc<Mutex<Vec<(Instant, String)>>>,
    gpu: Poller,
    apps: Poller,
}

impl Sampler {
    fn start() -> Self {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let others = Arc::new(Mutex::new(Vec::new()));
        let sink = samples.clone();
        let gpu = Poller::start(
            &[
                "--query-gpu=clocks.sm,power.draw,temperature.gpu,utilization.gpu,clocks_event_reasons.active",
                "--format=csv,noheader,nounits",
            ],
            200,
            move |at, line| {
                let fields: Vec<&str> = line.split(',').map(str::trim).collect();
                let f: Vec<f64> = fields.iter().filter_map(|x| x.parse().ok()).collect();
                let reasons = fields
                    .get(4)
                    .and_then(|r| u64::from_str_radix(r.trim_start_matches("0x"), 16).ok());
                if let (4, Some(reasons)) = (f.len(), reasons) {
                    let s = Sample {
                        at,
                        sm_mhz: f[0],
                        watts: f[1],
                        temp_c: f[2],
                        util: f[3],
                        reasons,
                    };
                    sink.lock().expect("sampler lock").push(s);
                }
            },
        );
        let me = std::process::id().to_string();
        let sink = others.clone();
        let apps = Poller::start(
            &[
                "--query-compute-apps=pid,process_name",
                "--format=csv,noheader",
            ],
            250,
            move |at, line| {
                let pid = line.split(',').next().map(str::trim).unwrap_or("");
                if !pid.is_empty() && pid != me {
                    sink.lock()
                        .expect("sampler lock")
                        .push((at, line.trim().to_owned()));
                }
            },
        );
        Self {
            samples,
            others,
            gpu,
            apps,
        }
    }

    fn between(&self, from: Instant, to: Instant) -> Vec<Sample> {
        let all = self.samples.lock().expect("sampler lock");
        all.iter()
            .filter(|s| s.at >= from && s.at <= to)
            .copied()
            .collect()
    }

    /// Distinct other compute processes seen in [from, to].
    fn others_between(&self, from: Instant, to: Instant) -> Vec<String> {
        let all = self.others.lock().expect("sampler lock");
        let mut v: Vec<String> = all
            .iter()
            .filter(|(at, _)| *at >= from && *at <= to)
            .map(|(_, p)| p.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    fn stop(self) {
        self.gpu.stop();
        self.apps.stop();
    }
}

/// Aggregate CPU time counters from /proc/stat: (busy, total) jiffies. On the GB10 the CPU cores
/// share the SoC power budget and the LPDDR5x memory with the GPU, so heavy host load lowers the
/// GPU clock and bandwidth; the bench reports it next to the throughput.
fn cpu_jiffies() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    let total: u64 = v.iter().sum();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

/// Names of the `clocks_event_reasons` bits (nvidia-smi --help-query-gpu).
fn clock_reasons(mask: u64) -> Vec<&'static str> {
    const NAMES: [(u64, &str); 9] = [
        (0x1, "gpu_idle"),
        (0x2, "applications_clocks_setting"),
        (0x4, "sw_power_cap"),
        (0x8, "hw_slowdown"),
        (0x10, "sync_boost"),
        (0x20, "sw_thermal_slowdown"),
        (0x40, "hw_thermal_slowdown"),
        (0x80, "hw_power_brake_slowdown"),
        (0x100, "display_clock_setting"),
    ];
    NAMES
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|&(_, name)| name)
        .collect()
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (s, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 {
        f64::NAN
    } else {
        s / n as f64
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let dev = spm_gpu::device_info()?;
    println!(
        "device: {} ({} SMs, cc {}.{}), libspm_cuda {}",
        dev.name,
        dev.sm_count,
        dev.compute_capability.0,
        dev.compute_capability.1,
        spm_gpu::version()
    );
    let config52 = spm_pow::mining_config(args.k)?.to_bytes();
    // Throughput does not depend on the data; the seeds are arbitrary and no share is produced.
    let mut job = Job::create(&JobParams {
        m: args.m,
        n: args.n,
        k: args.k,
        header76: [0; 76],
        config52,
        source: Source::Fill { seed: 1 },
        b_noise_seed: [0x5a; 32],
        bound: [0; 32],
        dump: false,
        hit_capacity: 0,
        chunk_ctas: args.chunk_ctas,
        mem_budget_bytes: 0,
    })?;
    let info = job.info()?;
    println!(
        "job: m={} n={} k={} | {} CTA tiles/pass | {} CTAs/SM | smem {} B/CTA | device memory {:.1} MiB | B-side prep {:.2} ms",
        info.m,
        info.n,
        info.k,
        info.cta_tiles,
        info.ctas_per_sm,
        info.smem_bytes,
        info.device_bytes as f64 / (1 << 20) as f64,
        info.b_prep_ms
    );

    let sampler = Sampler::start();
    std::thread::sleep(Duration::from_millis(1200)); // idle baseline
    let idle_to = Instant::now();

    // Warm-up pass (also settles the adaptive chunk size).
    let mut seed = [0u8; 32];
    job.set_attempt(&seed, None)?;
    job.run(None)?;

    let macs_per_pass = f64::from(args.m) * f64::from(args.n) * f64::from(args.k);
    let (mut passes, mut kernel_ms, mut prep_ms) = (0u64, 0.0f64, 0.0f64);
    let mut chunk_ms: Vec<f64> = Vec::new(); // --per-chunk only
    let mut chunk_ctas: Vec<u32> = Vec::new(); // --per-chunk only
    let (mut chunks, mut chunk_max, mut long_chunks) = (0u64, 0.0f64, 0u64);
    let mut csv = if args.per_chunk {
        String::from("pass,chunk,start_ms,ctas,gpu_ms\n")
    } else {
        String::from("pass,chunks,gpu_ms,max_chunk_ms,wall_ms\n")
    };
    let cpu_before = cpu_jiffies();
    let started = Instant::now();
    while started.elapsed().as_secs_f64() < args.seconds {
        seed[..8].copy_from_slice(&(passes + 1).to_le_bytes());
        let t_pass = Instant::now();
        job.set_attempt(&seed, None)?;
        prep_ms += f64::from(job.info()?.last_prep_ms);
        if args.per_chunk {
            for c in 0.. {
                let t0 = started.elapsed().as_secs_f64() * 1e3;
                let status = job.run_chunk()?;
                let i = job.info()?;
                chunk_ms.push(f64::from(i.last_chunk_ms));
                chunk_ctas.push(i.chunk_ctas);
                csv += &format!(
                    "{passes},{c},{t0:.3},{},{:.4}\n",
                    i.chunk_ctas, i.last_chunk_ms
                );
                if status == Chunk::Done {
                    break;
                }
            }
        } else {
            job.run(None)?;
        }
        let i = job.info()?;
        kernel_ms += f64::from(i.attempt_gpu_ms);
        chunks += u64::from(i.attempt_chunks);
        chunk_max = chunk_max.max(f64::from(i.attempt_max_chunk_ms));
        long_chunks += u64::from(i.attempt_max_chunk_ms > 10.0);
        if !args.per_chunk {
            csv += &format!(
                "{passes},{},{:.4},{:.4},{:.4}\n",
                i.attempt_chunks,
                i.attempt_gpu_ms,
                i.attempt_max_chunk_ms,
                t_pass.elapsed().as_secs_f64() * 1e3
            );
        }
        passes += 1;
    }
    let wall = started.elapsed().as_secs_f64();
    let ended = Instant::now();
    let host_cpu_pct = match (cpu_before, cpu_jiffies()) {
        (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => 100.0 * (b1 - b0) as f64 / (t1 - t0) as f64,
        _ => f64::NAN,
    };

    // Cancellation: another thread raises the abort flag mid-attempt; the latency is the time
    // until `run` returns (it checks the flag before every chunk: at most the running chunk).
    let mut abort_ms = Vec::new();
    for trial in 0..3u64 {
        seed[..8].copy_from_slice(&(u64::MAX - trial).to_le_bytes());
        job.set_attempt(&seed, None)?;
        let abort = Arc::new(AtomicU32::new(0));
        let raised = Arc::new(Mutex::new(None::<Instant>));
        let (flag, when) = (abort.clone(), raised.clone());
        let delay = Duration::from_micros(3000 + 2300 * trial);
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(delay);
            *when.lock().expect("abort lock") = Some(Instant::now());
            flag.store(1, Ordering::Release);
        });
        let outcome = job.run(Some(&abort))?;
        let returned = Instant::now();
        raiser.join().expect("abort thread");
        let at = raised.lock().expect("abort lock").expect("abort raised");
        if outcome == Run::Aborted {
            abort_ms.push(returned.saturating_duration_since(at).as_secs_f64() * 1e3);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let idle = sampler.between(idle_to - Duration::from_millis(1100), idle_to);
    let busy = sampler.between(started + Duration::from_millis(500), ended);
    let others = sampler.others_between(idle_to - Duration::from_millis(1100), ended);
    sampler.stop();
    if let Some(path) = &args.csv {
        std::fs::write(path, &csv).with_context(|| format!("writing {path}"))?;
    }

    let kernel_tmacs = passes as f64 * macs_per_pass / (kernel_ms / 1e3) / 1e12;
    let wall_tmacs = passes as f64 * macs_per_pass / wall / 1e12;
    let sm_mhz = mean(busy.iter().map(|s| s.sm_mhz));
    let watts = mean(busy.iter().map(|s| s.watts));
    let watts_max = busy.iter().map(|s| s.watts).fold(f64::NAN, f64::max);
    let idle_watts = mean(idle.iter().map(|s| s.watts));
    let idle_util = mean(idle.iter().map(|s| s.util));
    let temp_max = busy.iter().map(|s| s.temp_c).fold(f64::NAN, f64::max);
    let reasons = busy.iter().fold(0u64, |m, s| m | s.reasons);
    let mut sorted = chunk_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let pct = |q: f64| {
        sorted
            .get(((sorted.len() as f64 - 1.0) * q).round() as usize)
            .copied()
            .unwrap_or(f64::NAN)
    };
    let mut sizes = chunk_ctas.clone();
    sizes.sort_unstable();
    let peak_at_clock = PEAK_MAC_PER_CLK_SM * f64::from(dev.sm_count) * sm_mhz * 1e6 / 1e12;

    // A resident process that stays idle (vLLM between requests) does not disturb the run;
    // anything else holding a CUDA context during the run makes the numbers unreliable.
    let foreign: Vec<&String> = others.iter().filter(|p| !p.contains("VLLM")).collect();
    println!(
        "other GPU processes during the run: {others:?} -> {}",
        if foreign.is_empty() {
            "exclusive apart from the resident vLLM"
        } else {
            "SHARED: numbers not reliable"
        }
    );
    println!(
        "host CPU busy during the run: {host_cpu_pct:.0} % of {} cores",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!(
        "passes: {passes} in {wall:.2} s ({:.2} ms GPU per pass, {:.2} ms A-side prep per pass)",
        kernel_ms / passes as f64,
        prep_ms / passes as f64
    );
    if args.per_chunk {
        println!(
            "chunks (run_chunk): {chunks} in {passes} passes, median {} CTAs | chunk ms p50 {:.2} p99 {:.2} max {chunk_max:.2} | {} over 10 ms",
            sizes.get(sizes.len() / 2).copied().unwrap_or(0),
            pct(0.5),
            pct(0.99),
            chunk_ms.iter().filter(|&&t| t > 10.0).count()
        );
    } else {
        println!(
            "chunks (run): {chunks} in {passes} passes, mean {:.2} ms, max {chunk_max:.2} ms | {long_chunks} passes had a chunk over 10 ms",
            kernel_ms / chunks.max(1) as f64
        );
    }
    println!(
        "abort latency (flag raised mid-attempt -> run returns): {:.2?} ms",
        abort_ms
    );
    println!("credited T-MAC/s (kernel GPU time): {kernel_tmacs:.2}");
    println!("credited T-MAC/s (wall, incl. prep): {wall_tmacs:.2}");
    println!(
        "SM clock: {sm_mhz:.0} MHz avg ({} samples, clock event reasons {:?}) | {:.1} % of {PEAK_CAPPED_TMACS} T-MAC/s | {:.1} % of the MB1 register-only peak at this clock ({peak_at_clock:.1} T-MAC/s)",
        busy.len(),
        clock_reasons(reasons),
        100.0 * kernel_tmacs / PEAK_CAPPED_TMACS,
        100.0 * kernel_tmacs / peak_at_clock
    );
    println!(
        "GPU power: {watts:.1} W avg, {watts_max:.1} W max while mining, {idle_watts:.1} W before the run at {idle_util:.0} % utilization (+{:.1} W) | {:.2} T-MAC/s per GPU W | max temp {temp_max:.0} C",
        watts - idle_watts,
        kernel_tmacs / watts
    );
    Ok(())
}
