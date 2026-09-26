//! Gate G0: the fused GPU kernel against the CPU oracle (`spm-cpuref`).
//!
//! Built only with `--features gpu` and run only with `SPM_GPU_TESTS=1` (a live CUDA context
//! blocks the owner's vLLM orchestration, so nothing here starts one by accident). Run it in
//! release mode: the oracle evaluates every tile of every problem on the CPU.
//!
//! ```text
//! SPM_GPU_TESTS=1 cargo test --release -p spm-gpu --features gpu --test g0 -- --nocapture
//! ```
//!
//! Each test does its CPU work first and only then creates GPU jobs, so the CUDA context lives
//! for the GPU part only (well under a second per test).

use std::sync::atomic::{AtomicU32, Ordering};

use spm_cpuref::{
    build_plain_proof, fill_int7, first_mismatch, tiles_digest, verify_v3, Commitment,
    IncompleteBlockHeader, Oracle, Problem, TileResult, DOMAIN_A, DOMAIN_BT, SEED_LABEL_A,
    SEED_LABEL_B, U256,
};
use spm_gpu::{
    config52_for, debug_blake3_keyed64, Buffer, ChunkStatus, ErrorKind, Job, JobParams, Matrices,
    TileRecord,
};
use zk_pow::circuit::pearl_noise::{generate_permutation_matrix, generate_uniform_random_matrix};

/// ~1/16 of the tiles hit at k = 2048 (same constant as the spm-cpuref proof tests).
const EASY_NBITS: u32 = 0x1e03_ffff;
/// target·h·w·k overflows 256 bits: the consensus bound saturates, every tile verifies.
const SATURATED_NBITS: u32 = 0x207f_ffff;

fn enabled() -> bool {
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

fn tile_of(r: &TileRecord) -> TileResult {
    TileResult {
        t_rows: r.t_rows,
        t_cols: r.t_cols,
        transcript: r.transcript,
        digest: r.digest,
    }
}

fn le_bytes(v: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    v.to_little_endian(&mut b);
    b
}

fn params<'a>(
    p: &Problem,
    c: &Commitment,
    matrices: Matrices<'a>,
    bound: [u8; 32],
    dump: bool,
) -> JobParams<'a> {
    JobParams {
        m: p.m as u32,
        n: p.n as u32,
        k: p.k as u32,
        config52: p.config.to_bytes(),
        matrices,
        b_noise_seed: c.b_noise_seed,
        bound,
        dump,
        chunk_ctas: None,
        hit_capacity: None,
    }
}

fn as_bytes(v: &[i8]) -> Vec<u8> {
    v.iter().map(|&x| x as u8).collect()
}

/// Where a mismatching problem first differs: operands, factors or the tiles themselves.
fn diagnose(p: &Problem, job: &mut Job, expected: &[TileResult], got: &[TileResult]) -> String {
    let mut out = String::new();
    let oracle = Oracle::new(p).expect("oracle");
    let f = oracle.noise_factors().expect("factors");
    let checks: [(&str, Buffer, Vec<u8>); 7] = [
        ("A_base", Buffer::ABase, as_bytes(&p.a)),
        ("A_L", Buffer::AL, as_bytes(&f.a_l)),
        ("B_Rt", Buffer::BRt, as_bytes(&f.b_rt)),
        (
            "A pairs",
            Buffer::APairs,
            f.a_r
                .iter()
                .flat_map(|pq| [pq[0] as u8, pq[1] as u8])
                .collect(),
        ),
        (
            "B pairs",
            Buffer::BPairs,
            f.b_l
                .iter()
                .flat_map(|pq| [pq[0] as u8, pq[1] as u8])
                .collect(),
        ),
        ("A'", Buffer::ANoised, as_bytes(oracle.noised_a())),
        ("B't", Buffer::BtNoised, as_bytes(oracle.noised_bt())),
    ];
    for (name, buf, want) in checks {
        let have = job.read_whole_buffer(buf).expect("read buffer");
        match have.iter().zip(&want).position(|(a, b)| a != b) {
            None if have.len() == want.len() => out += &format!("  {name}: equal\n"),
            None => out += &format!("  {name}: length {} vs {}\n", have.len(), want.len()),
            Some(i) => {
                out += &format!(
                    "  {name}: first difference at byte {i}: gpu {} cpu {}\n",
                    have[i], want[i]
                )
            }
        }
    }
    if let Some(i) = first_mismatch(expected, got) {
        out += &format!(
            "  first mismatching tile #{i}\n    cpu {:?}\n",
            expected.get(i)
        );
        out += &format!("    gpu {:?}\n", got.get(i));
        if let Some(e) = expected.get(i) {
            let trace = oracle.trace(e.t_rows, e.t_cols).expect("trace");
            out += &format!("    cpu per-slice folds {:08x?}\n", trace.folds);
        }
    }
    out
}

struct Case {
    m: usize,
    n: usize,
    k: usize,
    seed: u64,
    commit: Commitment,
    tiles: Vec<TileResult>,
}

/// The G0 gate proper: every tile of 54 problems (m, n ∈ {256, 512, 1024}, k ∈ {2048, 4096},
/// 3 seeds) equals the oracle in debug-dump mode.
#[test]
fn g0_every_tile_matches_the_oracle() {
    if !enabled() {
        return;
    }
    let hdr = header(SATURATED_NBITS);
    let dims = [256usize, 512, 1024];
    let mut cases = Vec::new();
    for &m in &dims {
        for &n in &dims {
            for &k in &[2048usize, 4096] {
                for s in 0..3u64 {
                    let seed = 0x6730_0000 + (m as u64) * 7 + (n as u64) * 13 + (k as u64) * 17 + s;
                    let p = Problem::generate(m, n, k, hdr, seed).expect("problem");
                    assert_eq!(
                        config52_for(k as u32),
                        p.config.to_bytes(),
                        "config52_for({k})"
                    );
                    let oracle = Oracle::new(&p).expect("oracle");
                    let tiles = oracle.transcripts().expect("transcripts");
                    assert_eq!(tiles.len(), m * n / 128);
                    cases.push(Case {
                        m,
                        n,
                        k,
                        seed,
                        commit: *oracle.commitment(),
                        tiles,
                    });
                }
            }
        }
    }

    let mut failures = Vec::new();
    let mut total_tiles = 0usize;
    for c in &cases {
        let p = Problem::generate(c.m, c.n, c.k, hdr, c.seed).expect("problem");
        let mut job = Job::new(&params(
            &p,
            &c.commit,
            Matrices::Generated { seed: c.seed },
            [0; 32],
            true,
        ))
        .expect("job");
        job.set_attempt(&c.commit.a_noise_seed, None)
            .expect("attempt");
        job.run_to_completion().expect("run");
        let got: Vec<TileResult> = job
            .dump_records()
            .expect("dump")
            .iter()
            .map(tile_of)
            .collect();
        total_tiles += got.len();
        let hits = job.hits().expect("hits");
        assert_eq!(hits.total, 0, "bound 0 cannot be met");
        if let Some(i) = first_mismatch(&c.tiles, &got) {
            failures.push(format!(
                "{}x{}x{} seed {:#x}: first mismatch at tile #{i}\n{}",
                c.m,
                c.n,
                c.k,
                c.seed,
                diagnose(&p, &mut job, &c.tiles, &got)
            ));
        } else {
            assert_eq!(tiles_digest(&c.tiles), tiles_digest(&got));
        }
    }
    eprintln!(
        "G0: {} problems, {total_tiles} tiles compared, {} failing",
        cases.len(),
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "G0 mismatches:\n{}",
        failures.join("\n")
    );
}

/// Forced hits: with an easy difficulty the GPU's hit ring equals the oracle's hit list, and
/// every GPU hit becomes a PlainProof the official V3 verifier accepts (at least 100 proofs).
/// Then the bound is set to the smallest digest of a problem: exactly that tile must hit.
#[test]
fn forced_hits_verify_as_plain_proofs() {
    if !enabled() {
        return;
    }
    let hdr = header(EASY_NBITS);
    struct HitCase {
        p: Problem,
        commit: Commitment,
        bound: U256,
        expected: Vec<TileResult>,
        min_tile: TileResult,
    }
    let mut cases = Vec::new();
    for seed in [0x4800u64, 0x4801, 0x4802, 0x4803, 0x4804] {
        let p = Problem::generate(256, 256, 2048, hdr, seed).expect("problem");
        let bound = spm_pow::extract_difficulty_bound(EASY_NBITS, &p.config);
        let oracle = Oracle::new(&p).expect("oracle");
        let tiles = oracle.transcripts().expect("tiles");
        let expected: Vec<TileResult> = tiles.iter().filter(|t| t.meets(bound)).copied().collect();
        let min_tile = *tiles
            .iter()
            .min_by_key(|t| t.digest_value())
            .expect("tiles");
        let commit = *oracle.commitment();
        cases.push(HitCase {
            p,
            commit,
            bound,
            expected,
            min_tile,
        });
    }

    let mut proofs = 0usize;
    for c in &cases {
        let seed = c.p.seed.expect("generated");
        let mut job = Job::new(&params(
            &c.p,
            &c.commit,
            Matrices::Generated { seed },
            le_bytes(c.bound),
            false,
        ))
        .expect("job");
        job.set_attempt(&c.commit.a_noise_seed, None)
            .expect("attempt");
        job.run_to_completion().expect("run");
        let hits = job.hits().expect("hits");
        assert_eq!(hits.dropped(), 0);
        let mut got: Vec<(u32, u32, [u8; 32])> = hits
            .hits
            .iter()
            .map(|h| (h.t_rows, h.t_cols, h.digest))
            .collect();
        got.sort_unstable();
        let mut want: Vec<(u32, u32, [u8; 32])> = c
            .expected
            .iter()
            .map(|t| (t.t_rows, t.t_cols, t.digest))
            .collect();
        want.sort_unstable();
        assert!(
            !want.is_empty(),
            "seed {seed}: the easy bound should give hits"
        );
        assert_eq!(
            got, want,
            "seed {seed}: GPU hit set differs from the oracle"
        );
        for h in &hits.hits {
            // The proof opens only the tile's rows; the verifier recomputes the noise, the
            // transcript and the digest from the committed matrices on its own.
            let tile = TileResult {
                t_rows: h.t_rows,
                t_cols: h.t_cols,
                transcript: [0; 16],
                digest: h.digest,
            };
            let proof = build_plain_proof(&c.p, &tile).expect("proof");
            verify_v3(&hdr, &proof, Some(EASY_NBITS)).expect("GPU hit must verify");
            spm_pow::check_rank_penalty(&c.p.config, &h.digest, EASY_NBITS).expect("rank penalty");
            proofs += 1;
            if proofs % 25 == 1 {
                // Mutations of a valid GPU proof must all be rejected.
                let mut bad = proof.clone();
                bad.a.proof.leaf_data[0][5] ^= 1;
                assert!(
                    verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err(),
                    "flipped A byte"
                );
                let mut bad = proof.clone();
                bad.bt.proof.leaf_data[0][7] ^= 0x80;
                assert!(
                    verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err(),
                    "flipped Bᵀ byte"
                );
                let mut bad = proof.clone();
                bad.a.row_indices.iter_mut().for_each(|r| *r += 1);
                assert!(
                    verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err(),
                    "shifted rows"
                );
                let mut bad = proof.clone();
                bad.noise_rank = 64;
                assert!(
                    verify_v3(&hdr, &bad, Some(EASY_NBITS)).is_err(),
                    "noise rank"
                );
                let mut other = hdr;
                other.timestamp ^= 1;
                assert!(
                    verify_v3(&other, &proof, Some(EASY_NBITS)).is_err(),
                    "other header"
                );
            }
        }

        // Bound = smallest digest: exactly one hit, that tile.
        job.set_attempt(&c.commit.a_noise_seed, Some(&c.min_tile.digest))
            .expect("attempt");
        job.run_to_completion().expect("run");
        let hits = job.hits().expect("hits");
        assert_eq!(
            hits.total, 1,
            "seed {seed}: exactly the minimum tile must hit"
        );
        let h = hits.hits[0];
        assert_eq!(
            (h.t_rows, h.t_cols, h.digest),
            (c.min_tile.t_rows, c.min_tile.t_cols, c.min_tile.digest)
        );
        let proof = build_plain_proof(&c.p, &c.min_tile).expect("proof");
        verify_v3(&hdr, &proof, Some(EASY_NBITS)).expect("minimum tile must verify");
        // A tampered proof (wrong column base) must fail.
        let mut wrong = c.min_tile;
        wrong.t_cols ^= 2;
        let proof = build_plain_proof(&c.p, &wrong).expect("proof");
        if !(c
            .expected
            .iter()
            .any(|t| t.t_rows == wrong.t_rows && t.t_cols == wrong.t_cols))
        {
            assert!(
                verify_v3(&hdr, &proof, Some(EASY_NBITS)).is_err(),
                "non-hit tile must not verify"
            );
        }
    }
    eprintln!("forced hits: {proofs} GPU hits verified as PlainProofs");
    assert!(proofs >= 100, "only {proofs} proofs");
}

/// The noise generators on the GPU equal the official zk-pow ones: A side for 1000 seeds on one
/// job, B side for 24 jobs.
#[test]
fn noise_factors_match_the_official_generators() {
    if !enabled() {
        return;
    }
    let (m, n, k) = (64usize, 64usize, 2048usize);
    let mut rng = 0x243f_6a88_85a3_08d3u64;
    let mut next_seed = || {
        let mut s = [0u8; 32];
        for chunk in s.chunks_mut(8) {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            chunk.copy_from_slice(&rng.to_le_bytes());
        }
        s
    };
    let rows_m: Vec<usize> = (0..m).collect();
    let rows_n: Vec<usize> = (0..n).collect();
    let a_seeds: Vec<[u8; 32]> = (0..1000).map(|_| next_seed()).collect();
    let a_expected: Vec<(Vec<u8>, Vec<u8>)> = a_seeds
        .iter()
        .map(|s| {
            let al = generate_uniform_random_matrix(&SEED_LABEL_A, s, &rows_m, 128).concat();
            let pairs = generate_permutation_matrix(&SEED_LABEL_A, s, k, 128);
            (
                as_bytes(&al),
                pairs
                    .iter()
                    .flat_map(|pq| [pq[0] as u8, pq[1] as u8])
                    .collect(),
            )
        })
        .collect();
    let b_seeds: Vec<[u8; 32]> = (0..24).map(|_| next_seed()).collect();
    let b_expected: Vec<(Vec<u8>, Vec<u8>)> = b_seeds
        .iter()
        .map(|s| {
            let brt = generate_uniform_random_matrix(&SEED_LABEL_B, s, &rows_n, 128).concat();
            let pairs = generate_permutation_matrix(&SEED_LABEL_B, s, k, 128);
            (
                as_bytes(&brt),
                pairs
                    .iter()
                    .flat_map(|pq| [pq[0] as u8, pq[1] as u8])
                    .collect(),
            )
        })
        .collect();

    let config52 = config52_for(k as u32);
    let base = |b_seed: [u8; 32]| JobParams {
        m: m as u32,
        n: n as u32,
        k: k as u32,
        config52,
        matrices: Matrices::Generated { seed: 5 },
        b_noise_seed: b_seed,
        bound: [0; 32],
        dump: false,
        chunk_ctas: None,
        hit_capacity: None,
    };
    let mut job = Job::new(&base([0; 32])).expect("job");
    for (s, (al, pairs)) in a_seeds.iter().zip(&a_expected) {
        job.set_attempt(s, None).expect("attempt");
        assert_eq!(
            &job.read_whole_buffer(Buffer::AL).unwrap(),
            al,
            "A_L for seed {s:02x?}"
        );
        assert_eq!(
            &job.read_whole_buffer(Buffer::APairs).unwrap(),
            pairs,
            "A pairs for seed {s:02x?}"
        );
    }
    for (s, (brt, pairs)) in b_seeds.iter().zip(&b_expected) {
        let mut job = Job::new(&base(*s)).expect("job");
        assert_eq!(
            &job.read_whole_buffer(Buffer::BRt).unwrap(),
            brt,
            "B_Rt for seed {s:02x?}"
        );
        assert_eq!(
            &job.read_whole_buffer(Buffer::BPairs).unwrap(),
            pairs,
            "B pairs for seed {s:02x?}"
        );
    }
}

/// Generated matrices, noised operands and the device BLAKE3 against the CPU.
#[test]
fn operands_and_blake3_match_the_cpu() {
    if !enabled() {
        return;
    }
    let hdr = header(SATURATED_NBITS);
    let p = Problem::generate(192, 320, 4096, hdr, 77).expect("problem");
    let oracle = Oracle::new(&p).expect("oracle");
    let c = *oracle.commitment();
    let mut cases = Vec::new();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..200 {
        let mut key = [0u8; 32];
        let mut msg = [0u8; 64];
        for b in key.iter_mut().chain(msg.iter_mut()) {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 56) as u8;
        }
        cases.push((key, msg, *blake3::keyed_hash(&key, &msg).as_bytes()));
    }

    for (key, msg, want) in &cases {
        assert_eq!(&debug_blake3_keyed64(key, msg).expect("blake3"), want);
    }
    let mut job = Job::new(&params(
        &p,
        &c,
        Matrices::Generated { seed: 77 },
        [0; 32],
        false,
    ))
    .expect("job");
    job.set_attempt(&c.a_noise_seed, None).expect("attempt");
    assert_eq!(
        job.read_whole_buffer(Buffer::ABase).unwrap(),
        as_bytes(&fill_int7(77, DOMAIN_A, 192 * 4096))
    );
    assert_eq!(
        job.read_whole_buffer(Buffer::ANoised).unwrap(),
        as_bytes(oracle.noised_a())
    );
    assert_eq!(
        job.read_whole_buffer(Buffer::BtNoised).unwrap(),
        as_bytes(oracle.noised_bt())
    );
    // Bᵀ itself is noised in place; check the generator through a job built from host matrices.
    assert_eq!(p.bt, fill_int7(77, DOMAIN_BT, 320 * 4096));
}

/// Host matrices with the extreme entries ±64 (legal for the verifier, never produced by our
/// generator), partial CTA tiles (m % 128 = 64, n % 256 = 64), tiny chunks and an abort/resume.
#[test]
fn host_matrices_edges_chunks_and_abort() {
    if !enabled() {
        return;
    }
    let hdr = header(SATURATED_NBITS);
    let (m, n, k) = (192usize, 320usize, 2048usize);
    let mut a = fill_int7(91, DOMAIN_A, m * k);
    let mut bt = fill_int7(91, DOMAIN_BT, n * k);
    for (i, v) in a.iter_mut().enumerate() {
        if i % 7 == 0 {
            *v = if i % 2 == 0 { 64 } else { -64 };
        }
    }
    for (i, v) in bt.iter_mut().enumerate() {
        if i % 5 == 0 {
            *v = if i % 3 == 0 { 64 } else { -64 };
        }
    }
    let p = Problem::from_matrices(hdr, m, n, k, a, bt).expect("problem");
    let oracle = Oracle::new(&p).expect("oracle");
    let expected = oracle.transcripts().expect("tiles");
    let c = *oracle.commitment();

    // Nonce-style patch: a second problem that differs from p in chunk 0 of A only.
    let mut a2 = p.a.clone();
    for (i, v) in a2.iter_mut().take(1024).enumerate() {
        *v = ((i * 37 % 129) as i32 - 64) as i8;
    }
    let p2 = Problem::from_matrices(hdr, m, n, k, a2.clone(), p.bt.clone()).expect("problem 2");
    let oracle2 = Oracle::new(&p2).expect("oracle 2");
    let expected2 = oracle2.transcripts().expect("tiles 2");
    let c2 = *oracle2.commitment();
    assert_eq!(c.b_noise_seed, c2.b_noise_seed, "same Bᵀ, same B seed");

    let mut jp = params(&p, &c, Matrices::Host { a: &p.a, bt: &p.bt }, [0; 32], true);
    jp.chunk_ctas = Some(1); // one CTA tile per chunk: exercises every chunk boundary
    let mut job = Job::new(&jp).expect("job");
    let info = job.info().expect("info");
    assert_eq!(info.cta_tiles, 2 * 2);
    assert_eq!(info.chunks, info.cta_tiles);
    job.set_attempt(&c.a_noise_seed, None).expect("attempt");

    // Abort before anything runs: nothing happens, the cursor stays.
    let abort = AtomicU32::new(1);
    assert_eq!(job.run_attempt(&abort).expect("run"), ChunkStatus::Aborted);
    // One chunk by hand, then resume the rest.
    assert_eq!(job.run_chunk().expect("chunk"), ChunkStatus::More);
    abort.store(0, Ordering::Relaxed);
    assert_eq!(job.run_attempt(&abort).expect("run"), ChunkStatus::Done);
    let got: Vec<TileResult> = job
        .dump_records()
        .expect("dump")
        .iter()
        .map(tile_of)
        .collect();
    assert_eq!(
        first_mismatch(&expected, &got),
        None,
        "{}",
        diagnose(&p, &mut job, &expected, &got)
    );

    // Patch the nonce region and run the new attempt.
    job.patch_a(0, &a2[..1024]).expect("patch");
    assert_eq!(
        job.run_chunk().unwrap_err().kind,
        ErrorKind::State,
        "A' is stale after a patch"
    );
    job.set_attempt(&c2.a_noise_seed, None).expect("attempt 2");
    job.run_to_completion().expect("run 2");
    let got2: Vec<TileResult> = job
        .dump_records()
        .expect("dump")
        .iter()
        .map(tile_of)
        .collect();
    assert_eq!(
        first_mismatch(&expected2, &got2),
        None,
        "{}",
        diagnose(&p2, &mut job, &expected2, &got2)
    );
}

/// Launch chunking (automatic and adaptive, fixed whole waves, fixed odd sizes) never changes the
/// result: on a 16384 x 16384 x 2048 job (8192 CTA tiles, several chunks) the hit sets agree.
#[test]
fn chunking_does_not_change_the_hits() {
    if !enabled() {
        return;
    }
    let (m, n, k) = (16384u32, 16384u32, 2048u32);
    // About 1/64 of the 2^21 tiles hit (digest <= 2^250).
    let mut bound = [0u8; 32];
    bound[31] = 0x04;
    let run = |chunk: Option<u32>, attempts: usize| -> Vec<Vec<(u32, u32, [u8; 32])>> {
        let mut job = Job::new(&JobParams {
            m,
            n,
            k,
            config52: config52_for(k),
            matrices: Matrices::Generated { seed: 0xc4 },
            b_noise_seed: [0x42; 32],
            bound,
            dump: false,
            chunk_ctas: chunk,
            hit_capacity: Some(1 << 16),
        })
        .expect("job");
        (0..attempts)
            .map(|_| {
                job.set_attempt(&[0x17; 32], None).expect("attempt");
                job.run_to_completion().expect("run");
                let hits = job.hits().expect("hits");
                assert_eq!(hits.dropped(), 0);
                let mut v: Vec<_> = hits
                    .hits
                    .iter()
                    .map(|h| (h.t_rows, h.t_cols, h.digest))
                    .collect();
                v.sort_unstable();
                v
            })
            .collect()
    };
    let auto = run(None, 3); // the chunk size adapts after the first chunks
    assert!(
        auto[0].len() > 20_000,
        "expected ~32768 hits, got {}",
        auto[0].len()
    );
    assert_eq!(auto[0], auto[1]);
    assert_eq!(auto[0], auto[2]);
    assert_eq!(auto[0], run(Some(48 * 5), 1)[0], "fixed whole waves");
    assert_eq!(auto[0], run(Some(1000), 1)[0], "fixed odd size");
}
