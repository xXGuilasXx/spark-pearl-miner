//! Gate G0: the GPU kernel reproduces spm-cpuref bit for bit.
//!
//! Built only with `--features gpu` and run only when `SPM_GPU_TESTS=1` (each test is a few
//! seconds of mostly CPU work, the GPU part is short and stays far below 2 GiB):
//!
//! ```text
//! SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --nocapture
//! ```
#![cfg(feature = "gpu")]
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use spm_cpuref::{
    add_noise, build_plain_proof, first_mismatch, noise_factors, tiles_digest, verify_v3,
    Commitment, IncompleteBlockHeader, Oracle, Problem, TileResult, U256,
};
use spm_gpu::{Chunk, DebugBuffer, Job, JobParams, Run, Source, TileRecord};

/// ~1/16 of the tiles hit at k = 2048 (same constant as the spm-cpuref tests).
const EASY_NBITS: u32 = 0x1e03_ffff;

fn enabled() -> bool {
    let on = std::env::var("SPM_GPU_TESTS").ok().as_deref() == Some("1");
    if !on {
        eprintln!("skipped (set SPM_GPU_TESTS=1 to run the GPU tests)");
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

fn u256_le(x: U256) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, limb) in x.0.iter().enumerate() {
        out[8 * i..8 * i + 8].copy_from_slice(&limb.to_le_bytes());
    }
    out
}

fn params<'a>(
    p: &Problem,
    c: &Commitment,
    source: Source<'a>,
    dump: bool,
    bound: [u8; 32],
) -> JobParams<'a> {
    JobParams {
        m: p.m as u32,
        n: p.n as u32,
        k: p.k as u32,
        header76: p.header.to_bytes(),
        config52: p.config.to_bytes(),
        source,
        b_noise_seed: c.b_noise_seed,
        bound,
        dump,
        hit_capacity: 16384,
        chunk_ctas: 0,
        mem_budget_bytes: 0,
    }
}

fn to_tile(r: &TileRecord) -> TileResult {
    TileResult {
        t_rows: r.t_rows,
        t_cols: r.t_cols,
        transcript: r.transcript,
        digest: r.digest,
    }
}

fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn as_bytes(v: &[i8]) -> Vec<u8> {
    v.iter().map(|&x| x as u8).collect()
}

/// Localizes a transcript mismatch: operands first, then the tile's folds.
fn diagnose(job: &mut Job, oracle: &Oracle<'_>, expected: &TileResult, got: &TileResult) -> String {
    let p = oracle.problem();
    let mut out = format!(
        "first mismatching tile ({}, {}):\n  expected {:08x?} {}\n  got      ({}, {}) {:08x?} {}\n",
        expected.t_rows,
        expected.t_cols,
        expected.transcript,
        hex::encode(expected.digest),
        got.t_rows,
        got.t_cols,
        got.transcript,
        hex::encode(got.digest)
    );
    let a = job.read_debug(DebugBuffer::NoisedA).unwrap();
    let bt = job.read_debug(DebugBuffer::NoisedBt).unwrap();
    match first_diff(&a, &as_bytes(oracle.noised_a())) {
        Some(i) => out += &format!("  A' differs first at row {} col {}\n", i / p.k, i % p.k),
        None => out += "  A' equal\n",
    }
    match first_diff(&bt, &as_bytes(oracle.noised_bt())) {
        Some(i) => out += &format!("  B'ᵀ differs first at row {} col {}\n", i / p.k, i % p.k),
        None => out += "  B'ᵀ equal\n",
    }
    if let Ok(trace) = oracle.trace(expected.t_rows, expected.t_cols) {
        out += &format!("  oracle folds {:08x?}\n", trace.folds);
    }
    out
}

/// Runs one problem in dump mode and compares every tile.
fn check_problem(p: &Problem, source: Source<'_>, label: &str) -> [u8; 32] {
    let oracle = Oracle::new(p).unwrap();
    let c = *oracle.commitment();
    let expected = oracle.transcripts().unwrap();
    let mut job = Job::create(&params(p, &c, source, true, [0; 32])).unwrap();
    assert_eq!(job.info().unwrap().job_key, c.job_key, "{label}: job key");
    job.set_attempt(&c.a_noise_seed, None).unwrap();
    assert_eq!(job.run(None).unwrap(), Run::Done);
    let got: Vec<TileResult> = job.read_dump().unwrap().iter().map(to_tile).collect();
    if let Some(i) = first_mismatch(&expected, &got) {
        let why = diagnose(&mut job, &oracle, &expected[i], &got[i]);
        panic!("{label}: tile #{i} of {} differs\n{why}", expected.len());
    }
    let digest = tiles_digest(&got);
    assert_eq!(digest, tiles_digest(&expected), "{label}: tiles digest");
    digest
}

#[test]
fn g0_every_shape_is_bit_exact() {
    if !enabled() {
        return;
    }
    let started = std::time::Instant::now();
    let mut count = 0;
    for m in [256usize, 512, 1024] {
        for n in [256usize, 512, 1024] {
            for k in [2048usize, 4096] {
                for seed in [1u64, 2, 3] {
                    let p = Problem::generate(m, n, k, header(EASY_NBITS), seed).unwrap();
                    let label = format!("m={m} n={n} k={k} seed={seed}");
                    let d = check_problem(&p, Source::Fill { seed }, &label);
                    eprintln!(
                        "G0 ok  {label:<32} tiles={:>5} tiles_blake3={}",
                        m * n / 128,
                        hex::encode(&d[..8])
                    );
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 54);
    eprintln!(
        "G0: 54/54 problems bit-exact in {:.1} s",
        started.elapsed().as_secs_f64()
    );
}

#[test]
fn ragged_and_long_k_are_bit_exact() {
    if !enabled() {
        return;
    }
    // k mod 128 = 64 (the last 64 columns never enter the transcript), S mod 16 != 0 (the
    // transcript slot wraps mid-way) and a long k (each slot written three times).
    for (m, n, k, seed) in [
        (128usize, 256usize, 2112usize, 11u64),
        (256, 128, 2176, 12),
        (128, 128, 6144, 13),
    ] {
        let p = Problem::generate(m, n, k, header(EASY_NBITS), seed).unwrap();
        check_problem(
            &p,
            Source::Fill { seed },
            &format!("m={m} n={n} k={k} seed={seed}"),
        );
    }
}

#[test]
fn noise_factors_and_operands_match_zk_pow() {
    if !enabled() {
        return;
    }
    let p = Problem::generate(256, 256, 2048, header(EASY_NBITS), 5).unwrap();
    let base = spm_cpuref::commit(&p).unwrap();
    let pairs_bytes = |v: &[[u32; 2]]| {
        v.iter()
            .flat_map(|&[a, b]| [a as u8, b as u8])
            .collect::<Vec<u8>>()
    };
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
    // The B side is built at create, the A side at every attempt: one job per seed pair.
    let seeds = 1000;
    for _ in 0..seeds {
        let mut c = base;
        c.b_noise_seed = next_seed();
        c.a_noise_seed = next_seed();
        let f = noise_factors(&p, &c).unwrap();
        let e = f.expand().unwrap();
        let mut job =
            Job::create(&params(&p, &c, Source::Fill { seed: 5 }, true, [0; 32])).unwrap();
        assert_eq!(job.read_debug(DebugBuffer::BRt).unwrap(), as_bytes(&f.b_rt));
        assert_eq!(
            job.read_debug(DebugBuffer::PairsB).unwrap(),
            pairs_bytes(&f.b_l)
        );
        assert_eq!(
            job.read_debug(DebugBuffer::NoisedBt).unwrap(),
            as_bytes(&add_noise(&p.bt, &e.e_bt).unwrap())
        );
        job.set_attempt(&c.a_noise_seed, None).unwrap();
        assert_eq!(job.read_debug(DebugBuffer::AL).unwrap(), as_bytes(&f.a_l));
        assert_eq!(
            job.read_debug(DebugBuffer::PairsA).unwrap(),
            pairs_bytes(&f.a_r)
        );
        assert_eq!(
            job.read_debug(DebugBuffer::NoisedA).unwrap(),
            as_bytes(&add_noise(&p.a, &e.e_a).unwrap())
        );
    }
    eprintln!(
        "noise: {seeds} B seeds and {seeds} A seeds bit-exact (factors, pairs, noised operands)"
    );
}

#[test]
fn host_matrices_and_nonce_patch_are_bit_exact() {
    if !enabled() {
        return;
    }
    let (m, n, k) = (256usize, 384usize, 2048usize);
    let g = Problem::generate(m, n, k, header(EASY_NBITS), 21).unwrap();
    // Host source with extreme entries (the verifier accepts [-64, 64]).
    let mut a = g.a.clone();
    let mut bt = g.bt.clone();
    for i in (0..a.len()).step_by(97) {
        a[i] = if i % 2 == 0 { 64 } else { -64 };
    }
    for i in (0..bt.len()).step_by(89) {
        bt[i] = if i % 2 == 0 { 64 } else { -64 };
    }
    let p = Problem::from_matrices(g.header, m, n, k, a.clone(), bt.clone()).unwrap();
    check_problem(
        &p,
        Source::Host { a: &a, bt: &bt },
        "host source, extreme entries",
    );

    // A nonce patched into chunk 0 of A: same result through the fill source (override region)
    // and through the host source (written into the stored A).
    let nonce: Vec<i8> = (0..64).map(|i| ((i * 37) % 129) as i8 - 64).collect();
    let offset = 300u64;
    let mut patched = g.a.clone();
    patched[offset as usize..offset as usize + nonce.len()].copy_from_slice(&nonce);
    let pp = Problem::from_matrices(g.header, m, n, k, patched.clone(), g.bt.clone()).unwrap();
    let oracle = Oracle::new(&pp).unwrap();
    let c = *oracle.commitment();
    let expected = oracle.transcripts().unwrap();
    for source in [
        Source::Fill { seed: 21 },
        Source::Host { a: &g.a, bt: &g.bt },
    ] {
        let mut job = Job::create(&params(&pp, &c, source, true, [0; 32])).unwrap();
        job.patch_a(offset, &nonce).unwrap();
        job.set_attempt(&c.a_noise_seed, None).unwrap();
        job.run(None).unwrap();
        let got: Vec<TileResult> = job.read_dump().unwrap().iter().map(to_tile).collect();
        assert_eq!(
            first_mismatch(&expected, &got),
            None,
            "patched A ({source:?})"
        );
    }
}

#[test]
fn chunked_and_aborted_runs_give_the_same_tiles() {
    if !enabled() {
        return;
    }
    let p = Problem::generate(512, 512, 4096, header(EASY_NBITS), 8).unwrap();
    let oracle = Oracle::new(&p).unwrap();
    let c = *oracle.commitment();
    let expected = oracle.transcripts().unwrap();
    let mut prm = params(&p, &c, Source::Fill { seed: 8 }, true, [0; 32]);
    prm.chunk_ctas = 3; // 16 CTA tiles -> 6 chunks, the last one short
    let mut job = Job::create(&prm).unwrap();
    job.set_attempt(&c.a_noise_seed, None).unwrap();
    let mut chunks = 1;
    while job.run_chunk().unwrap() == Chunk::More {
        chunks += 1;
    }
    assert_eq!(chunks, 6);
    let got: Vec<TileResult> = job.read_dump().unwrap().iter().map(to_tile).collect();
    assert_eq!(first_mismatch(&expected, &got), None, "chunked");

    // Abort before the first chunk, then resume.
    job.set_attempt(&c.a_noise_seed, None).unwrap();
    let abort = AtomicU32::new(1);
    assert_eq!(job.run(Some(&abort)).unwrap(), Run::Aborted);
    assert_eq!(job.info().unwrap().next_cta, 0);
    job.run_chunk().unwrap();
    assert_eq!(job.run(Some(&abort)).unwrap(), Run::Aborted);
    assert_eq!(job.info().unwrap().next_cta, 3);
    abort.store(0, Ordering::Release);
    assert_eq!(job.run(Some(&abort)).unwrap(), Run::Done);
    let got: Vec<TileResult> = job.read_dump().unwrap().iter().map(to_tile).collect();
    assert_eq!(first_mismatch(&expected, &got), None, "aborted and resumed");
}

#[test]
fn forced_hits_become_verified_plain_proofs() {
    if !enabled() {
        return;
    }
    let hdr = header(EASY_NBITS);
    let mut proofs = 0;
    for (m, n, k, seed) in [
        (512usize, 512usize, 2048usize, 41u64),
        (256, 512, 4096, 42),
        (1024, 256, 2048, 43),
    ] {
        let p = Problem::generate(m, n, k, hdr, seed).unwrap();
        let oracle = Oracle::new(&p).unwrap();
        let c = *oracle.commitment();
        let bound = spm_pow::extract_difficulty_bound(EASY_NBITS, &p.config);
        let mut expected: Vec<(u32, u32, [u8; 32])> = oracle
            .find_hits(bound)
            .unwrap()
            .iter()
            .map(|t| (t.t_rows, t.t_cols, t.digest))
            .collect();
        expected.sort();

        let mut job = Job::create(&params(
            &p,
            &c,
            Source::Fill { seed },
            false,
            u256_le(bound),
        ))
        .unwrap();
        job.set_attempt(&c.a_noise_seed, None).unwrap();
        assert_eq!(job.run(None).unwrap(), Run::Done);
        let hits = job.read_hits().unwrap();
        assert_eq!(hits.lost, 0);
        let mut got: Vec<(u32, u32, [u8; 32])> = hits
            .hits
            .iter()
            .map(|h| (h.t_rows, h.t_cols, h.digest))
            .collect();
        got.sort();
        assert_eq!(
            got, expected,
            "m={m} n={n} k={k}: GPU hits differ from the oracle's"
        );
        assert!(!got.is_empty());

        for &(t_rows, t_cols, digest) in &got {
            let tile = TileResult {
                t_rows,
                t_cols,
                transcript: [0; 16],
                digest,
            };
            let proof = build_plain_proof(&p, &tile).unwrap();
            verify_v3(&hdr, &proof, None).unwrap();
            verify_v3(&hdr, &proof, Some(EASY_NBITS)).unwrap();
            spm_pow::check_rank_penalty(&p.config, &digest, EASY_NBITS).unwrap();
            proofs += 1;
        }
        // A mutated proof fails.
        let (t_rows, t_cols, digest) = got[0];
        let mut bad = build_plain_proof(
            &p,
            &TileResult {
                t_rows,
                t_cols,
                transcript: [0; 16],
                digest,
            },
        )
        .unwrap();
        bad.a.row_indices[0] ^= 1;
        assert!(
            verify_v3(&hdr, &bad, None).is_err(),
            "mutated row index must fail"
        );

        // Smallest digest: with the bound set to it exactly, the GPU reports that tile alone.
        let min = got
            .iter()
            .min_by_key(|h| U256::from_little_endian(&h.2))
            .copied()
            .unwrap();
        job.set_attempt(&c.a_noise_seed, Some(&min.2)).unwrap();
        job.run(None).unwrap();
        let only = job.read_hits().unwrap();
        assert_eq!(only.hits.len(), 1);
        assert_eq!(
            (
                only.hits[0].t_rows,
                only.hits[0].t_cols,
                only.hits[0].digest
            ),
            min
        );
    }
    assert!(proofs >= 100, "only {proofs} proofs");
    eprintln!("forced hits: {proofs} GPU hits turned into PlainProofs, all pass verify_v3 and the rank penalty");
}
