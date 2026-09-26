//! Gate G0: the GPU kernel reproduces spm-cpuref bit for bit.
//!
//! Only touches the GPU with SPM_GPU_TESTS=1:
//!   SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --test-threads=1
//! Each test computes every oracle result on the CPU first and only then opens the CUDA context, so
//! the GPU is held for a short burst (the vLLM-resident rule: ≤ ~10 s, ≤ 2 GiB).
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use spm_cpuref::{
    add_noise, build_plain_proof, first_mismatch, noise_factors, tiles_digest, verify_v3,
    IncompleteBlockHeader, NoiseFactors, Oracle, Problem, TileResult, U256,
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

/// Ragged and long k: k mod 128 = 64 (the last 64 columns never enter the transcript), a slice
/// count that is not a multiple of 16 (the transcript slot wraps mid-way and the epilogue undoes a
/// leftover rotation of 1) and a long k (every slot written three times), on CTA tiles that are
/// half empty along n or m.
#[test]
fn g0_ragged_and_long_k() {
    if !gpu_enabled() {
        return;
    }
    let cases: Vec<Case> = [
        (128usize, 256usize, 2112usize, 11u64),
        (256, 128, 2176, 12),
        (128, 128, 6144, 13),
    ]
    .into_iter()
    .map(|(m, n, k, seed)| prepare(m, n, k, 0x7261_6700 + seed))
    .collect();
    let failures: Vec<String> = cases.iter().filter_map(|c| run_case(c).err()).collect();
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
            spm_pow::check_rank_penalty(&p.config, &h.digest, EASY_NBITS)
                .expect("hit passes the pool-side rank-penalized bound");
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
        let mut bad = build_plain_proof(p, t).unwrap();
        bad.bt.row_indices.iter_mut().for_each(|r| *r += 2);
        assert!(verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err());
    }
    eprintln!("forced hits: {verified} GPU PlainProofs verified");
    assert!(verified >= 100, "only {verified} proofs verified");
}

/// Noise stage: the GPU's uniform factors (A_L, B_Rᵀ) and permutation pairs (A_R, B_L) equal the
/// official zk-pow generators (`spm_cpuref::noise_factors`) for 1000 A seeds and 64 B seeds; E_A,
/// E_Bᵀ, A', B'ᵀ and the generated A, Bᵀ equal the oracle's for the problem's own commitment.
#[test]
fn g0_noise_matches_the_official_generators() {
    if !gpu_enabled() {
        return;
    }
    let (m, n, k) = (128usize, 64usize, 4096usize);
    let p = Problem::generate(m, n, k, header(EASY_NBITS), 99).unwrap();
    let oracle = Oracle::new(&p).unwrap();
    let base = *oracle.commitment();
    let seed_of = |i: u64, salt: u8| -> [u8; 32] {
        let mut s = [0u8; 32];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (i
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .rotate_left(7 * j as u32) as u8)
                ^ salt
                ^ j as u8;
        }
        s
    };
    let factors = |a: &[u8; 32], b: &[u8; 32]| -> NoiseFactors {
        let mut c = base;
        c.a_noise_seed = *a;
        c.b_noise_seed = *b;
        noise_factors(&p, &c).unwrap()
    };
    let bytes = |v: &[i8]| -> Vec<u8> { v.iter().map(|&x| x as u8).collect() };
    let pair_bytes =
        |v: &[[u32; 2]]| -> Vec<u8> { v.iter().flat_map(|&[p, q]| [p as u8, q as u8]).collect() };
    let a_seeds: Vec<[u8; 32]> = (0..1000).map(|i| seed_of(i, 0xa5)).collect();
    let b_seeds: Vec<[u8; 32]> = (0..64).map(|i| seed_of(i, 0x5a)).collect();
    let a_expect: Vec<NoiseFactors> = a_seeds.iter().map(|a| factors(a, &b_seeds[0])).collect();
    let b_expect: Vec<NoiseFactors> = b_seeds.iter().map(|b| factors(&a_seeds[0], b)).collect();
    let noise = oracle.noise().unwrap();

    let host = Operands::Host { a: &p.a, bt: &p.bt };
    let (m32, n32, k32) = (m as u32, n as u32, k as u32);
    let mut job = Job::new(&JobConfig::new(m32, n32, k32, host, b_seeds[0])).unwrap();
    for (i, (a, e)) in a_seeds.iter().zip(&a_expect).enumerate() {
        job.set_attempt(a, &[0u8; 32]).unwrap();
        assert_eq!(
            job.read_buffer(Buffer::AFactor).unwrap(),
            bytes(&e.a_l),
            "A_L, seed {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::APairs).unwrap(),
            pair_bytes(&e.a_r),
            "A_R, seed {i}"
        );
    }
    for (i, (b, e)) in b_seeds.iter().zip(&b_expect).enumerate() {
        let mut job = Job::new(&JobConfig::new(m32, n32, k32, host, *b)).unwrap();
        assert_eq!(
            job.read_buffer(Buffer::BtFactor).unwrap(),
            bytes(&e.b_rt),
            "B_Rᵀ, seed {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::BPairs).unwrap(),
            pair_bytes(&e.b_l),
            "B_L, seed {i}"
        );
    }
    let generated = Operands::Generated { seed: 99 };
    let mut job = Job::new(&JobConfig::new(m32, n32, k32, generated, base.b_noise_seed)).unwrap();
    job.set_attempt(&base.a_noise_seed, &[0u8; 32]).unwrap();
    assert_eq!(
        job.read_buffer(Buffer::ABase).unwrap(),
        bytes(&p.a),
        "A = fill_int7"
    );
    assert_eq!(
        job.read_buffer(Buffer::BtBase).unwrap(),
        bytes(&p.bt),
        "Bᵀ = fill_int7"
    );
    assert_eq!(
        job.read_buffer(Buffer::ANoise).unwrap(),
        bytes(&noise.e_a),
        "E_A"
    );
    assert_eq!(
        job.read_buffer(Buffer::BtNoise).unwrap(),
        bytes(&noise.e_bt),
        "E_Bᵀ"
    );
    assert_eq!(
        job.read_buffer(Buffer::ANoised).unwrap(),
        bytes(oracle.noised_a()),
        "A'"
    );
    assert_eq!(
        job.read_buffer(Buffer::BtNoised).unwrap(),
        bytes(oracle.noised_bt()),
        "B'ᵀ"
    );
}

/// Noise stage over 1000 random seed pairs, one job each: the B side at creation (B_Rᵀ, B_L,
/// B'ᵀ = Bᵀ + E_Bᵀ) and the A side of an attempt (A_L, A_R, A' = A + E_A) equal the official
/// zk-pow generators and the reference noise expansion.
#[test]
fn g0_noise_over_1000_random_seed_pairs() {
    if !gpu_enabled() {
        return;
    }
    let (m, n, k) = (256usize, 256usize, 2048usize);
    let p = Problem::generate(m, n, k, header(EASY_NBITS), 5).unwrap();
    let base = *Oracle::new(&p).unwrap().commitment();
    let bytes = |v: &[i8]| -> Vec<u8> { v.iter().map(|&x| x as u8).collect() };
    let pair_bytes =
        |v: &[[u32; 2]]| -> Vec<u8> { v.iter().flat_map(|&[p, q]| [p as u8, q as u8]).collect() };
    // xorshift64 seeds: unrelated to every seed derivation of the protocol.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next_seed = || {
        let mut s = [0u8; 32];
        for chunk in s.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes());
        }
        s
    };
    let started = Instant::now();
    let seeds = 1000;
    for i in 0..seeds {
        let mut c = base;
        c.b_noise_seed = next_seed();
        c.a_noise_seed = next_seed();
        let f = noise_factors(&p, &c).unwrap();
        let e = f.expand().unwrap();
        let cfg = JobConfig::new(
            m as u32,
            n as u32,
            k as u32,
            Operands::Generated { seed: 5 },
            c.b_noise_seed,
        );
        let mut job = Job::new(&cfg).unwrap();
        assert_eq!(
            job.read_buffer(Buffer::BtFactor).unwrap(),
            bytes(&f.b_rt),
            "B_Rᵀ, pair {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::BPairs).unwrap(),
            pair_bytes(&f.b_l),
            "B_L, pair {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::BtNoised).unwrap(),
            bytes(&add_noise(&p.bt, &e.e_bt).unwrap()),
            "B'ᵀ, pair {i}"
        );
        job.set_attempt(&c.a_noise_seed, &[0u8; 32]).unwrap();
        assert_eq!(
            job.read_buffer(Buffer::AFactor).unwrap(),
            bytes(&f.a_l),
            "A_L, pair {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::APairs).unwrap(),
            pair_bytes(&f.a_r),
            "A_R, pair {i}"
        );
        assert_eq!(
            job.read_buffer(Buffer::ANoised).unwrap(),
            bytes(&add_noise(&p.a, &e.e_a).unwrap()),
            "A', pair {i}"
        );
    }
    eprintln!(
        "noise: {seeds} random (B, A) seed pairs bit-exact (factors, pairs, noised operands) in {:.1} s",
        started.elapsed().as_secs_f64()
    );
}

/// A full hit ring keeps counting: with every tile a hit (all-ones bound) and room for 100, the
/// count is exact, 100 entries are stored and each is a real tile with its real digest.
#[test]
fn g0_hit_ring_overflow_is_counted() {
    if !gpu_enabled() {
        return;
    }
    let case = prepare(256, 256, 2048, 0x0f10);
    let mut cfg = JobConfig::new(
        case.m,
        case.n,
        case.k,
        Operands::Generated { seed: case.seed },
        case.b_noise_seed,
    );
    cfg.hit_capacity = 100;
    let mut job = Job::new(&cfg).expect("job");
    job.set_attempt(&case.a_noise_seed, &[0xff; 32]).unwrap();
    assert!(job.run_attempt().unwrap().completed);
    let (hits, total) = job.hits().unwrap();
    assert_eq!(
        total as usize,
        case.expected.len(),
        "every tile meets the all-ones bound"
    );
    assert_eq!(hits.len(), 100, "the ring holds its capacity");
    let mut seen = std::collections::HashSet::new();
    for h in &hits {
        let t = case
            .expected
            .iter()
            .find(|t| (t.t_rows, t.t_cols) == (h.t_rows, h.t_cols))
            .expect("a real tile");
        assert_eq!(t.digest, h.digest);
        assert!(seen.insert((h.t_rows, h.t_cols)), "a tile stored twice");
    }
}

/// Launch chunking never changes the result: on a 16384 x 16384 x 2048 job (8192 CTA tiles) the
/// hit sets of adaptive chunks (three attempts, the size adapting in between), whole waves (240
/// CTA tiles), an odd fixed size (1000) and tiny chunks (7) are identical.
#[test]
fn g0_chunking_does_not_change_the_hits() {
    if !gpu_enabled() {
        return;
    }
    let (m, n, k) = (16384u32, 16384u32, 2048u32);
    // Digest <= 2^250: about 1/64 of the 2^21 hash tiles hit.
    let mut bound = [0u8; 32];
    bound[31] = 0x04;
    let run = |chunk: Option<u32>, attempts: usize| -> Vec<Vec<(u32, u32, [u8; 32])>> {
        let mut cfg = JobConfig::new(m, n, k, Operands::Generated { seed: 0xc4 }, [0x42; 32]);
        cfg.chunk_tiles = chunk;
        cfg.hit_capacity = 1 << 16;
        let mut job = Job::new(&cfg).expect("job");
        (0..attempts)
            .map(|_| {
                job.set_attempt(&[0x17; 32], &bound).expect("attempt");
                let stats = job.run_attempt().expect("run");
                assert!(stats.completed);
                let (hits, total) = job.hits().expect("hits");
                assert_eq!(total as usize, hits.len(), "no hit dropped");
                let mut v: Vec<_> = hits
                    .iter()
                    .map(|h| (h.t_rows, h.t_cols, h.digest))
                    .collect();
                v.sort_unstable();
                v
            })
            .collect()
    };
    let auto = run(None, 3);
    assert!(
        auto[0].len() > 20_000 && auto[0].len() < 50_000,
        "expected ~32768 hits, got {}",
        auto[0].len()
    );
    assert_eq!(auto[0], auto[1], "adaptive, second attempt");
    assert_eq!(auto[0], auto[2], "adaptive, third attempt");
    assert_eq!(auto[0], run(Some(48 * 5), 1)[0], "fixed whole waves");
    assert_eq!(auto[0], run(Some(1000), 1)[0], "fixed odd size");
    assert_eq!(auto[0], run(Some(7), 1)[0], "tiny chunks");
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

/// Pipelined chunks: a 2048 x 2048 x 4096 attempt split into 8-tile chunks, aborted between two
/// calls (so a queued chunk sees the flag mid-flight), cleared and resumed. The dump must still be
/// the oracle's, tile for tile, and the hit ring must contain every hit (repeats allowed).
#[test]
fn g0_pipelined_chunks_abort_and_resume() {
    if !gpu_enabled() {
        return;
    }
    let (m, n, k, seed) = (2048usize, 2048usize, 4096usize, 0x7069_7065u64);
    let case = prepare(m, n, k, seed);
    let mut cfg = JobConfig::new(
        m as u32,
        n as u32,
        k as u32,
        Operands::Generated { seed },
        case.b_noise_seed,
    );
    cfg.dump = true;
    cfg.chunk_tiles = Some(8);
    cfg.hit_capacity = 16384; // ~1/8 of the 32768 tiles hit at this bound, plus repeats
    let mut job = Job::new(&cfg).expect("job");
    job.set_attempt(&case.a_noise_seed, &le_bytes(case.bound))
        .unwrap();
    let first = job.run_chunk().unwrap();
    assert_eq!(first.status, ChunkStatus::More);
    assert_eq!((first.tile_begin, first.tile_end), (0, 8));
    let abort = job.abort_handle();
    abort.set();
    let mut aborted = 0;
    let mut done = false;
    for _ in 0..4 {
        match job.run_chunk().unwrap().status {
            ChunkStatus::Aborted => aborted += 1,
            ChunkStatus::More => {}
            ChunkStatus::Done => done = true,
        }
    }
    assert!(aborted >= 1 && !done, "the flag must stop the attempt");
    abort.clear();
    let stats = job.run_attempt().unwrap();
    assert!(stats.completed);
    let got: Vec<TileResult> = job.dump_records().unwrap().iter().map(to_tile).collect();
    assert_eq!(
        first_mismatch(&case.expected, &got),
        None,
        "dump after abort + resume"
    );
    let (hits, _) = job.hits().unwrap();
    let mut seen: Vec<(u32, u32)> = hits.iter().map(|h| (h.t_rows, h.t_cols)).collect();
    seen.sort_unstable();
    seen.dedup();
    let want = case.expected.iter().filter(|t| t.meets(case.bound)).count();
    assert_eq!(seen.len(), want, "every hit found (repeats allowed)");
}
