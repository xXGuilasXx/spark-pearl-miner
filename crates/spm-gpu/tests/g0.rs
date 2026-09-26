//! Gate G0: the GPU kernel reproduces spm-cpuref bit for bit.
//!
//! Only touches the GPU with SPM_GPU_TESTS=1:
//!   SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --test-threads=1
//! Each test computes every oracle result on the CPU first and only then opens the CUDA context, so
//! the GPU is held for a short burst (the vLLM-resident rule: ≤ ~10 s, ≤ 2 GiB).
use std::time::{Duration, Instant};

use spm_cpuref::{
    build_plain_proof, first_mismatch, tiles_digest, verify_v3, IncompleteBlockHeader, Oracle,
    Problem, TileResult, U256,
};
use spm_gpu::{Buffer, ChunkStatus, Job, JobConfig, Operands, TileRecord};

/// ~1/16 of the tiles hit at k = 2048 (same constant as the spm-cpuref proof tests).
const EASY_NBITS: u32 = 0x1e03_ffff;

fn gpu_enabled() -> bool {
    let on = std::env::var("SPM_GPU_TESTS").ok().as_deref() == Some("1");
    if !on {
        eprintln!("skipped (set SPM_GPU_TESTS=1)");
    }
    on
}

fn header(nbits: u32) -> IncompleteBlockHeader {
    IncompleteBlockHeader {
        version: 0x2000_0000,
        prev_block: [1; 32],
        merkle_root: [2; 32],
        timestamp: 0x6666_6666,
        nbits,
    }
}

fn le_bytes(x: U256) -> [u8; 32] {
    let mut out = [0u8; 32];
    x.to_little_endian(&mut out);
    out
}

fn to_tile(r: &TileRecord) -> TileResult {
    TileResult {
        t_rows: r.t_rows,
        t_cols: r.t_cols,
        transcript: r.transcript,
        digest: r.digest,
    }
}

/// Everything the GPU run needs, computed on the CPU beforehand.
struct Case {
    label: String,
    m: u32,
    n: u32,
    k: u32,
    seed: u64,
    b_noise_seed: [u8; 32],
    a_noise_seed: [u8; 32],
    bound: U256,
    expected: Vec<TileResult>,
    /// Reference operands for stage localization on a mismatch.
    noised_a: Vec<i8>,
    noised_bt: Vec<i8>,
}

fn prepare(m: usize, n: usize, k: usize, seed: u64) -> Case {
    let p = Problem::generate(m, n, k, header(EASY_NBITS), seed).expect("problem");
    let oracle = Oracle::new(&p).expect("oracle");
    let c = *oracle.commitment();
    Case {
        label: format!("m={m} n={n} k={k} seed={seed}"),
        m: m as u32,
        n: n as u32,
        k: k as u32,
        seed,
        b_noise_seed: c.b_noise_seed,
        a_noise_seed: c.a_noise_seed,
        bound: spm_pow::extract_difficulty_bound(EASY_NBITS, &p.config),
        expected: oracle.transcripts().expect("transcripts"),
        noised_a: oracle.noised_a().to_vec(),
        noised_bt: oracle.noised_bt().to_vec(),
    }
}

fn first_diff(want: &[i8], got: &[u8]) -> Option<usize> {
    want.iter().zip(got).position(|(&w, &g)| w as u8 != g)
}

/// Localizes a mismatch: operand build first, then the GEMM/transcript/hash of the first bad tile.
fn diagnose(case: &Case, job: &mut Job, got: &[TileResult], idx: usize) -> String {
    let mut msg = format!("{}: first mismatching tile #{idx}", case.label);
    let (e, g) = (&case.expected[idx], &got[idx]);
    msg += &format!(
        "\n  expected ({}, {}) t={:08x?}\n  got      ({}, {}) t={:08x?}",
        e.t_rows, e.t_cols, e.transcript, g.t_rows, g.t_cols, g.transcript
    );
    if e.transcript == g.transcript {
        msg += "\n  transcripts equal, digests differ: BLAKE3 epilogue";
    }
    let a = job.read_buffer(Buffer::ANoised).expect("A'");
    let bt = job.read_buffer(Buffer::BtNoised).expect("B'ᵀ");
    match first_diff(&case.noised_a, &a) {
        Some(i) => {
            msg += &format!(
                "\n  A' differs first at entry {i} (row {}, col {})",
                i / case.k as usize,
                i % case.k as usize
            )
        }
        None => msg += "\n  A' equal",
    }
    match first_diff(&case.noised_bt, &bt) {
        Some(i) => {
            msg += &format!(
                "\n  B'ᵀ differs first at entry {i} (row {}, col {})",
                i / case.k as usize,
                i % case.k as usize
            )
        }
        None => msg += "\n  B'ᵀ equal",
    }
    msg
}

fn run_case(case: &Case) -> Result<(), String> {
    let mut cfg = JobConfig::new(
        case.m,
        case.n,
        case.k,
        Operands::Generated { seed: case.seed },
        case.b_noise_seed,
    );
    cfg.dump = true;
    let mut job = Job::new(&cfg).map_err(|e| format!("{}: create: {e}", case.label))?;
    job.set_attempt(&case.a_noise_seed, &le_bytes(case.bound))
        .map_err(|e| format!("{}: attempt: {e}", case.label))?;
    let stats = job
        .run_attempt()
        .map_err(|e| format!("{}: run: {e}", case.label))?;
    if !stats.completed {
        return Err(format!("{}: attempt did not complete", case.label));
    }
    let got: Vec<TileResult> = job
        .dump_records()
        .map_err(|e| format!("{}: dump: {e}", case.label))?
        .iter()
        .map(to_tile)
        .collect();
    if let Some(idx) = first_mismatch(&case.expected, &got) {
        if idx >= got.len() || idx >= case.expected.len() {
            return Err(format!(
                "{}: {} tiles expected, {} dumped",
                case.label,
                case.expected.len(),
                got.len()
            ));
        }
        return Err(diagnose(case, &mut job, &got, idx));
    }
    if tiles_digest(&case.expected) != tiles_digest(&got) {
        return Err(format!("{}: tiles_digest differs", case.label));
    }
    // The hit ring holds exactly the tiles whose digest is <= bound.
    let (mut hits, total) = job
        .hits()
        .map_err(|e| format!("{}: hits: {e}", case.label))?;
    let mut want: Vec<(u32, u32, [u8; 32])> = case
        .expected
        .iter()
        .filter(|t| t.meets(case.bound))
        .map(|t| (t.t_rows, t.t_cols, t.digest))
        .collect();
    hits.sort_by_key(|h| (h.t_rows, h.t_cols));
    let got_hits: Vec<(u32, u32, [u8; 32])> = hits
        .iter()
        .map(|h| (h.t_rows, h.t_cols, h.digest))
        .collect();
    want.sort_by_key(|h| (h.0, h.1));
    if total as usize != want.len() || got_hits != want {
        return Err(format!(
            "{}: {} GPU hits, {} expected",
            case.label,
            total,
            want.len()
        ));
    }
    Ok(())
}

#[test]
fn g0_every_tile_matches_the_oracle() {
    if !gpu_enabled() {
        return;
    }
    let t0 = Instant::now();
    let mut cases = Vec::new();
    for m in [256usize, 512, 1024] {
        for n in [256usize, 512, 1024] {
            for k in [2048usize, 4096] {
                for s in 0..3u64 {
                    let seed = 0x6730_0000 + (m as u64) * 7 + (n as u64) * 13 + (k as u64) * 17 + s;
                    cases.push(prepare(m, n, k, seed));
                }
            }
        }
    }
    let cpu = t0.elapsed();
    let t1 = Instant::now();
    let failures: Vec<String> = cases.iter().filter_map(|c| run_case(c).err()).collect();
    let gpu = t1.elapsed();
    let tiles: usize = cases.iter().map(|c| c.expected.len()).sum();
    eprintln!(
        "G0: {} problems, {tiles} tiles, oracle {:.1} s, GPU phase {:.2} s, {} failures",
        cases.len(),
        cpu.as_secs_f64(),
        gpu.as_secs_f64(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Odd shapes through the same kernel: partial CTA tiles (m % 128 = 64, n % 256 != 0), a k whose
/// last 64 columns are committed but never hashed, extreme committed entries (±64), host-supplied
/// operands, and an attempt with a nonce patch over the first entries of A.
#[test]
fn g0_odd_shapes_host_operands_and_prefix() {
    if !gpu_enabled() {
        return;
    }
    struct Odd {
        p: Problem,
        prefix: Vec<i8>,
        expected: Vec<TileResult>,
        b_seed: [u8; 32],
        a_seed: [u8; 32],
    }
    let mut odd = Vec::new();
    for (m, n, k, seed, prefix_len) in [
        (192usize, 320usize, 2112usize, 41u64, 0usize),
        (64, 64, 2048, 42, 0),
        (320, 192, 4160, 43, 1024),
        (128, 512, 2048, 44, 4096),
    ] {
        let mut p = Problem::generate(m, n, k, header(EASY_NBITS), seed).unwrap();
        // Extreme entries the verifier accepts but the generator never draws.
        p.a[5] = 64;
        p.a[m * k - 1] = -64;
        p.bt[7] = 64;
        p.bt[n * k - 2] = -64;
        let full = p.a.clone();
        let prefix: Vec<i8> = (0..prefix_len)
            .map(|i| ((i * 37 + 11) % 129) as i8 - 64)
            .collect();
        let base = Problem::from_matrices(p.header, m, n, k, full, p.bt.clone()).unwrap();
        let mut patched = base.clone();
        patched.a[..prefix_len].copy_from_slice(&prefix);
        // B side comes from the unpatched job; the A side (and so a_noise_seed) from the patched A.
        let b_seed = Oracle::new(&base).unwrap().commitment().b_noise_seed;
        let oracle = Oracle::new(&patched).unwrap();
        assert_eq!(
            oracle.commitment().b_noise_seed,
            b_seed,
            "the prefix only touches A"
        );
        odd.push(Odd {
            expected: oracle.transcripts().unwrap(),
            a_seed: oracle.commitment().a_noise_seed,
            b_seed,
            prefix,
            p: base,
        });
    }
    let mut failures = Vec::new();
    for o in &odd {
        let (m, n, k) = (o.p.m as u32, o.p.n as u32, o.p.k as u32);
        let mut cfg = JobConfig::new(
            m,
            n,
            k,
            Operands::Host {
                a: &o.p.a,
                bt: &o.p.bt,
            },
            o.b_seed,
        );
        cfg.dump = true;
        let mut job = Job::new(&cfg).expect("job");
        job.set_attempt_with_prefix(&o.a_seed, &[0u8; 32], &o.prefix)
            .expect("attempt");
        assert!(job.run_attempt().expect("run").completed);
        let got: Vec<TileResult> = job.dump_records().unwrap().iter().map(to_tile).collect();
        if let Some(i) = first_mismatch(&o.expected, &got) {
            failures.push(format!(
                "m={m} n={n} k={k} prefix={}: first mismatch at tile {i}",
                o.prefix.len()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Forced hits: with the bound set to the smallest digest of the problem, the GPU reports exactly
/// that tile; its CPU-built PlainProof passes the V3 verifier. Then every GPU hit at an easy bound
/// is turned into a proof and verified (≥ 100 proofs), and a mutated proof fails.
#[test]
fn g0_forced_hits_verify() {
    if !gpu_enabled() {
        return;
    }
    let hdr = header(EASY_NBITS);
    let problems: Vec<Problem> = (0..4u64)
        .map(|s| Problem::generate(256, 256, 2048, hdr, 0x4849_5400 + s).unwrap())
        .collect();
    struct Prep {
        tiles: Vec<TileResult>,
        b_seed: [u8; 32],
        a_seed: [u8; 32],
    }
    let preps: Vec<Prep> = problems
        .iter()
        .map(|p| {
            let o = Oracle::new(p).unwrap();
            Prep {
                tiles: o.transcripts().unwrap(),
                b_seed: o.commitment().b_noise_seed,
                a_seed: o.commitment().a_noise_seed,
            }
        })
        .collect();
    let easy = spm_pow::extract_difficulty_bound(EASY_NBITS, &problems[0].config);
    let mut verified = 0usize;
    for (p, prep) in problems.iter().zip(&preps) {
        let cfg = JobConfig::new(
            256,
            256,
            2048,
            Operands::Generated {
                seed: p.seed.unwrap(),
            },
            prep.b_seed,
        );
        let mut job = Job::new(&cfg).expect("job");

        // 1. Bound = the smallest digest: exactly that tile hits.
        let min = prep.tiles.iter().min_by_key(|t| t.digest_value()).unwrap();
        job.set_attempt(&prep.a_seed, &le_bytes(min.digest_value()))
            .unwrap();
        assert!(job.run_attempt().unwrap().completed);
        let (hits, total) = job.hits().unwrap();
        let ties = prep
            .tiles
            .iter()
            .filter(|t| t.digest_value() <= min.digest_value())
            .count();
        assert_eq!(total as usize, ties);
        let hit = hits
            .iter()
            .find(|h| (h.t_rows, h.t_cols) == (min.t_rows, min.t_cols))
            .expect("min tile hit");
        assert_eq!(hit.digest, min.digest);
        let tile = TileResult {
            t_rows: hit.t_rows,
            t_cols: hit.t_cols,
            transcript: min.transcript,
            digest: hit.digest,
        };
        let proof = build_plain_proof(p, &tile).unwrap();
        verify_v3(&hdr, &proof, Some(EASY_NBITS)).expect("forced hit verifies");
        verified += 1;

        // 2. Easy bound: every GPU hit becomes a verified proof.
        job.set_attempt(&prep.a_seed, &le_bytes(easy)).unwrap();
        assert!(job.run_attempt().unwrap().completed);
        let (hits, total) = job.hits().unwrap();
        assert_eq!(
            total as usize,
            prep.tiles.iter().filter(|t| t.meets(easy)).count()
        );
        for h in &hits {
            let t = prep
                .tiles
                .iter()
                .find(|t| (t.t_rows, t.t_cols) == (h.t_rows, h.t_cols))
                .unwrap();
            assert_eq!(t.digest, h.digest);
            let proof = build_plain_proof(p, t).unwrap();
            verify_v3(&hdr, &proof, Some(EASY_NBITS)).expect("hit verifies");
            verified += 1;
        }
        // 3. A tampered proof of a GPU hit fails.
        let t = prep
            .tiles
            .iter()
            .find(|t| (t.t_rows, t.t_cols) == (hits[0].t_rows, hits[0].t_cols))
            .unwrap();
        let mut bad = build_plain_proof(p, t).unwrap();
        bad.a.proof.leaf_data[0][3] ^= 1;
        assert!(verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err());
    }
    eprintln!("forced hits: {verified} GPU PlainProofs verified");
    assert!(verified >= 100, "only {verified} proofs verified");
}

/// The abort flag stops a running chunk within one CTA tile (< 1 ms), and chunks refuse to start
/// while it is set; clearing it resumes the attempt from the aborted chunk.
#[test]
fn g0_abort_is_prompt() {
    if !gpu_enabled() {
        return;
    }
    let (m, n, k) = (16384u32, 16384u32, 4096u32);
    let mut cfg = JobConfig::new(m, n, k, Operands::Generated { seed: 7 }, [3u8; 32]);
    cfg.chunk_tiles = Some(u32::MAX); // one chunk for the whole attempt (~14 ms)
    let mut job = Job::new(&cfg).expect("job");
    job.set_attempt(&[4u8; 32], &[0u8; 32]).unwrap();
    assert_eq!(
        job.run_chunk().unwrap().status,
        ChunkStatus::Done,
        "warm-up (module load)"
    );
    job.set_attempt(&[5u8; 32], &[0u8; 32]).unwrap();
    let full = job.run_chunk().unwrap();
    assert_eq!(full.status, ChunkStatus::Done);

    let abort = job.abort_handle();
    job.set_attempt(&[6u8; 32], &[0u8; 32]).unwrap();
    let delay = full.kernel * 2 / 5;
    let setter = std::thread::spawn(move || {
        std::thread::sleep(delay);
        let at = Instant::now();
        abort.set();
        at
    });
    let chunk = job.run_chunk().unwrap();
    let returned = Instant::now();
    let set_at = setter.join().unwrap();
    assert_eq!(
        chunk.status,
        ChunkStatus::Aborted,
        "chunk of {:?} not aborted after {delay:?}",
        full.kernel
    );
    let latency = returned.saturating_duration_since(set_at);
    eprintln!(
        "full chunk {:?}, abort set after {delay:?}, run_chunk returned {latency:?} later",
        full.kernel
    );
    assert!(latency < Duration::from_millis(1), "abort took {latency:?}");
    assert_eq!(
        job.run_chunk().unwrap().status,
        ChunkStatus::Aborted,
        "flag still set"
    );
    job.abort_handle().clear();
    assert_eq!(job.run_chunk().unwrap().status, ChunkStatus::Done);
}
