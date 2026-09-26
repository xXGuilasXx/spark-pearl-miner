//! Throughput bench of the fused GEMM + hash kernel (strategy C).
//!
//!   cargo run --release -p spm-gpu --features gpu --example bench -- [--seconds 10] [--m 16384]
//!       [--n 16384] [--k 4096] [--band 16] [--chunk-ms 0] [--chunk-tiles 0] [--csv FILE]
//!       [--abort-trials 3] [--prepare 0]
//!
//! Runs attempts back to back for ~`seconds` (each attempt: A-side prep for a new a_noise_seed,
//! then every tile in chunks driven by `run_chunk`, the worker's path) and prints:
//!
//! * credited T-MAC/s (m·n·k per attempt) over the kernel time and over wall time;
//! * every chunk's kernel time: mean, p50, p99, max and how many exceeded 8 / 10 ms (`--csv` writes
//!   one row per chunk);
//! * the abort latency: another thread raises the abort flag in the middle of an attempt, and the
//!   latency is the time until `run_chunk` reports the abort;
//! * SM clock, GPU power, temperature and the clock event reasons sampled with nvidia-smi every
//!   100 ms, the power before the run (idle, same process) and the delta, T-MAC/s per W;
//! * the other processes holding a CUDA context during the run (anything besides the resident,
//!   idle vLLM makes the numbers unreliable) and the host CPU utilization (the GB10's CPU and GPU
//!   share one power budget and the memory).
//!
//! `--prepare 1` builds the next attempt's A' with `Job::prepare_attempt` while the current one
//! runs (double buffering), so `set_attempt` only swaps buffers.
//!
//! `--seconds 0` runs exactly one timed attempt after the warm-up (a quick check at the 131072²
//! default job shape). `--chunk-ms 0` (default) keeps the library's adaptive target; `--chunk-tiles`
//! fixes the chunk size instead.
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use spm_gpu::{ChunkStatus, Job, JobConfig, Operands};

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
    chunk_tiles: u32,
    csv: Option<String>,
    abort_trials: u32,
    prepare: bool,
}

const USAGE: &str = "bench [--seconds 10] [--m 16384] [--n 16384] [--k 4096] [--band 16] \
                     [--chunk-ms 0] [--chunk-tiles 0] [--csv FILE] [--abort-trials 3] \
                     [--prepare 0]";

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args {
        m: 16384,
        n: 16384,
        k: 4096,
        seconds: 10.0,
        band: 0,
        chunk_ms: 0.0,
        chunk_tiles: 0,
        csv: None,
        abort_trials: 3,
        prepare: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        let val = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--m" => a.m = val.parse()?,
            "--n" => a.n = val.parse()?,
            "--k" => a.k = val.parse()?,
            "--seconds" => a.seconds = val.parse()?,
            "--band" => a.band = val.parse()?,
            "--chunk-ms" => a.chunk_ms = val.parse()?,
            "--chunk-tiles" => a.chunk_tiles = val.parse()?,
            "--csv" => a.csv = Some(val.clone()),
            "--abort-trials" => a.abort_trials = val.parse()?,
            "--prepare" => a.prepare = val.parse::<u8>()? != 0,
            other => anyhow::bail!("unknown flag {other}\n{USAGE}"),
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
    /// `clocks_event_reasons.active` bitmask (why the clock is below its maximum).
    reasons: u64,
}

/// Runs `nvidia-smi <args> -lms <period>` in the background and hands every output line, with its
/// arrival time, to `on_line`.
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

/// Clock / power / temperature samples every 100 ms and the other processes holding a CUDA
/// context (every 250 ms).
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
                "--query-gpu=clocks.sm,power.draw,temperature.gpu,clocks_event_reasons.active",
                "--format=csv,noheader,nounits",
            ],
            100,
            move |at, line| {
                let fields: Vec<&str> = line.split(',').map(str::trim).collect();
                let num = |i: usize| fields.get(i).and_then(|x| x.parse::<f64>().ok());
                let reasons = fields
                    .get(3)
                    .and_then(|r| u64::from_str_radix(r.trim_start_matches("0x"), 16).ok());
                if let (Some(sm_mhz), Some(watts), Some(temp_c), Some(reasons)) =
                    (num(0), num(1), num(2), reasons)
                {
                    if let Ok(mut s) = sink.lock() {
                        s.push(Sample {
                            at,
                            sm_mhz,
                            watts,
                            temp_c,
                            reasons,
                        });
                    }
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
                    if let Ok(mut s) = sink.lock() {
                        s.push((at, line.trim().to_owned()));
                    }
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
        let all = self.samples.lock().map(|s| s.clone()).unwrap_or_default();
        all.into_iter()
            .filter(|s| s.at >= from && s.at <= to)
            .collect()
    }

    /// Distinct other compute processes seen in [from, to].
    fn others_between(&self, from: Instant, to: Instant) -> Vec<String> {
        let all = self.others.lock().map(|s| s.clone()).unwrap_or_default();
        let mut v: Vec<String> = all
            .into_iter()
            .filter(|(at, _)| *at >= from && *at <= to)
            .map(|(_, p)| p)
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

/// Aggregate CPU time counters from /proc/stat: (busy, total) jiffies.
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

/// Nearest-rank percentile of an ascending slice.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn seed_bytes(i: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    for (j, b) in s.iter_mut().enumerate() {
        *b = (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> ((j % 8) * 8)) as u8 ^ j as u8;
    }
    s
}

/// One chunk as seen by the host.
struct ChunkRow {
    attempt: u64,
    index: u32,
    /// When `run_chunk` returned, relative to the start of the timed run.
    at_ms: f64,
    tile_begin: u32,
    tile_end: u32,
    ctas: u32,
    kernel_ms: f64,
}

/// Runs one attempt chunk by chunk; returns (completed, kernel time) and appends its chunks.
fn run_attempt_chunks(
    job: &mut Job,
    attempt: u64,
    origin: Instant,
    rows: &mut Vec<ChunkRow>,
) -> anyhow::Result<(bool, Duration)> {
    let mut kernel = Duration::ZERO;
    for index in 0u32.. {
        let c = job.run_chunk()?;
        if c.ctas > 0 {
            kernel += c.kernel;
            rows.push(ChunkRow {
                attempt,
                index,
                at_ms: origin.elapsed().as_secs_f64() * 1e3,
                tile_begin: c.tile_begin,
                tile_end: c.tile_end,
                ctas: c.ctas,
                kernel_ms: c.kernel.as_secs_f64() * 1e3,
            });
        }
        match c.status {
            ChunkStatus::More => {}
            ChunkStatus::Done => return Ok((true, kernel)),
            ChunkStatus::Aborted => return Ok((false, kernel)),
        }
    }
    unreachable!("the chunk loop only ends by returning")
}

/// Raises the abort flag from another thread `delay` into an attempt and measures how long
/// `run_chunk` takes to report it. `None` when the attempt finished before the flag.
fn abort_latency(job: &mut Job, seed: u64, delay: Duration) -> anyhow::Result<Option<Duration>> {
    let never = [0u8; 32];
    job.set_attempt(&seed_bytes(seed), &never)?;
    let abort = job.abort_handle();
    let raised = Arc::new(Mutex::new(None::<Instant>));
    let (flag, when) = (abort.clone(), raised.clone());
    let raiser = std::thread::spawn(move || {
        std::thread::sleep(delay);
        if let Ok(mut w) = when.lock() {
            *w = Some(Instant::now());
        }
        flag.set();
    });
    let outcome = loop {
        let c = job.run_chunk()?;
        match c.status {
            ChunkStatus::More => {}
            other => break other,
        }
    };
    let returned = Instant::now();
    raiser
        .join()
        .map_err(|_| anyhow::anyhow!("abort thread panicked"))?;
    abort.clear();
    let at = raised.lock().ok().and_then(|w| *w);
    Ok(match (outcome, at) {
        (ChunkStatus::Aborted, Some(at)) => Some(returned.saturating_duration_since(at)),
        _ => None,
    })
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let dev = spm_gpu::device_info()?;
    let sampler = Sampler::start();
    let mut cfg = JobConfig::new(
        args.m,
        args.n,
        args.k,
        Operands::Generated { seed: 0x5eed },
        seed_bytes(u64::MAX),
    );
    cfg.band_rows = args.band;
    if args.chunk_ms > 0.0 {
        cfg.target_chunk = Duration::from_secs_f64(args.chunk_ms / 1e3);
    }
    if args.chunk_tiles > 0 {
        cfg.chunk_tiles = Some(args.chunk_tiles);
    }
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
    let chunking = match (args.chunk_tiles, args.chunk_ms) {
        (t, _) if t > 0 => format!("fixed {t} CTA tiles"),
        (_, ms) if ms > 0.0 => format!("adaptive, target {ms} ms"),
        _ => "adaptive, library default target".to_owned(),
    };
    println!(
        "chunks: {chunking}; first chunk {} CTA tiles; next A' {}",
        info.chunk_tiles,
        if args.prepare {
            "prepared during the current attempt (double buffering)"
        } else {
            "built by set_attempt"
        }
    );

    // Idle baseline: the process holds its context but the GPU does no work.
    std::thread::sleep(Duration::from_millis(1100));
    let idle_to = Instant::now();

    // Warm-up attempt (module load, clocks, and it lets the adaptive chunk size settle).
    let never = [0u8; 32];
    let origin = Instant::now();
    let mut warm_rows = Vec::new();
    job.set_attempt(&seed_bytes(0), &never)?;
    if args.prepare {
        job.prepare_attempt(&seed_bytes(1))?;
    }
    run_attempt_chunks(&mut job, 0, origin, &mut warm_rows)?;
    let settled = job.info()?.chunk_tiles;

    let mut rows: Vec<ChunkRow> = Vec::new();
    let cpu_before = cpu_jiffies();
    let start = Instant::now();
    let (mut attempts, mut kernel, mut prep) = (0u64, Duration::ZERO, Duration::ZERO);
    loop {
        attempts += 1;
        let t = Instant::now();
        job.set_attempt(&seed_bytes(attempts), &never)?;
        prep += t.elapsed();
        if args.prepare {
            job.prepare_attempt(&seed_bytes(attempts + 1))?;
        }
        let (completed, k) = run_attempt_chunks(&mut job, attempts, start, &mut rows)?;
        anyhow::ensure!(completed, "attempt aborted");
        kernel += k;
        if start.elapsed().as_secs_f64() >= args.seconds {
            break;
        }
    }
    let wall = start.elapsed();
    let ended = Instant::now();
    let host_cpu_pct = match (cpu_before, cpu_jiffies()) {
        (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => 100.0 * (b1 - b0) as f64 / (t1 - t0) as f64,
        _ => f64::NAN,
    };

    // Cancellation, measured the way the worker sees it. The flag is raised at 25 / 50 / 75 % of
    // an attempt's kernel time (a new attempt each time).
    let attempt_kernel = kernel / attempts.max(1) as u32;
    let mut aborts = Vec::new();
    for trial in 0..args.abort_trials {
        let delay = attempt_kernel * (trial % 3 + 1) / 4;
        match abort_latency(&mut job, (1 << 40) | u64::from(trial), delay)? {
            Some(l) => aborts.push(l.as_secs_f64() * 1e3),
            None => {
                println!("abort trial {trial}: the attempt finished before the flag ({delay:?})")
            }
        }
    }

    std::thread::sleep(Duration::from_millis(150));
    let idle = sampler.between(idle_to - Duration::from_millis(1000), idle_to);
    let busy = sampler.between(start, ended);
    let others = sampler.others_between(idle_to - Duration::from_millis(1000), ended);
    sampler.stop();

    if let Some(path) = &args.csv {
        let mut csv =
            String::from("attempt,chunk,at_ms,tile_begin,tile_end,tiles,ctas,kernel_ms\n");
        for r in &rows {
            csv += &format!(
                "{},{},{:.3},{},{},{},{},{:.4}\n",
                r.attempt,
                r.index,
                r.at_ms,
                r.tile_begin,
                r.tile_end,
                r.tile_end - r.tile_begin,
                r.ctas,
                r.kernel_ms
            );
        }
        std::fs::write(path, csv).with_context(|| format!("writing {path}"))?;
    }

    let credited = job.credited_macs() as f64 * attempts as f64;
    let kernel_rate = credited / kernel.as_secs_f64() / 1e12;
    let wall_rate = credited / wall.as_secs_f64() / 1e12;
    let mut ms: Vec<f64> = rows.iter().map(|r| r.kernel_ms).collect();
    ms.sort_by(f64::total_cmp);
    let mut tiles: Vec<u32> = rows.iter().map(|r| r.tile_end - r.tile_begin).collect();
    tiles.sort_unstable();
    let sm_mhz = mean(busy.iter().map(|s| s.sm_mhz));
    let mhz_min = busy.iter().map(|s| s.sm_mhz).fold(f64::NAN, f64::min);
    let watts = mean(busy.iter().map(|s| s.watts));
    let watts_max = busy.iter().map(|s| s.watts).fold(f64::NAN, f64::max);
    let idle_watts = mean(idle.iter().map(|s| s.watts));
    let temp_max = busy.iter().map(|s| s.temp_c).fold(f64::NAN, f64::max);
    let reasons = busy.iter().fold(0u64, |m, s| m | s.reasons);
    let peak_at_clock = MB1_MAC_PER_CLK_PER_SM * f64::from(dev.sm_count) * sm_mhz * 1e6 / 1e12;

    // A resident process that stays idle (vLLM between requests) does not disturb the run;
    // anything else holding a CUDA context during it makes the numbers unreliable.
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
        "{attempts} attempts in {:.2} s ({} chunks, set_attempt {:.2} ms/attempt)",
        wall.as_secs_f64(),
        rows.len(),
        prep.as_secs_f64() * 1e3 / attempts as f64
    );
    println!("credited T-MAC/s: {kernel_rate:.2} kernel-only, {wall_rate:.2} end-to-end (prep + launch gaps included)");
    println!(
        "chunk kernel ms: mean {:.2}, p50 {:.2}, p99 {:.2}, max {:.2}; {} over 8 ms, {} over 10 ms | CTA tiles per chunk: {} after warm-up, median {}, max {}",
        mean(ms.iter().copied()),
        percentile(&ms, 0.5),
        percentile(&ms, 0.99),
        ms.last().copied().unwrap_or(f64::NAN),
        ms.iter().filter(|&&t| t > 8.0).count(),
        ms.iter().filter(|&&t| t > 10.0).count(),
        settled,
        tiles.get(tiles.len() / 2).copied().unwrap_or(0),
        tiles.last().copied().unwrap_or(0)
    );
    if !aborts.is_empty() {
        println!(
            "abort latency (flag raised mid-attempt -> run_chunk reports it): {} ms",
            aborts
                .iter()
                .map(|x| format!("{x:.3}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "SM clock {sm_mhz:.0} MHz mean, {mhz_min:.0} MHz min ({} nvidia-smi samples; clock event reasons {:?}), max temp {temp_max:.0} C",
        busy.len(),
        clock_reasons(reasons)
    );
    println!(
        "GPU power {watts:.1} W mean / {watts_max:.1} W max; {idle_watts:.1} W idle before the run (+{:.1} W)",
        watts - idle_watts
    );
    println!(
        "{:.1} % of {PEAK_2200_TMACS} T-MAC/s (MB1 peak at 2200 MHz); {:.1} % of the MB1 peak at the measured clock ({peak_at_clock:.1} T-MAC/s)",
        100.0 * kernel_rate / PEAK_2200_TMACS,
        if peak_at_clock > 0.0 { 100.0 * kernel_rate / peak_at_clock } else { 0.0 }
    );
    if watts > 0.0 {
        println!(
            "efficiency: {:.2} T-MAC/s per W kernel-only ({:.2} per W over idle); {:.2} T-MAC/J end-to-end",
            kernel_rate / watts,
            if watts > idle_watts { kernel_rate / (watts - idle_watts) } else { f64::NAN },
            wall_rate / watts
        );
    }
    if let Some(path) = &args.csv {
        println!("per-chunk CSV: {path} ({} rows)", rows.len());
    }
    Ok(())
}
