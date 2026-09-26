//! Cross-checks of the oracle against the official zk-pow reference.

mod common;

use common::*;
use rand::{rngs::StdRng, Rng, SeedableRng};
use spm_cpuref::*;
use spm_pow::{extract_difficulty_bound, mining_config};
use zk_pow::api::proof_utils::CompiledPublicParams;
use zk_pow::circuit::pearl_noise::compute_noise_for_indices;
use zk_pow::ffi::mine::try_mine_one;

/// (seed, m, n, k): three 256×256×2048 problems, one k = 4096 (every transcript word is
/// revisited, so the rotation matters) and one with r ∤ k (ragged tail never hashed).
const SHAPES: &[(u64, usize, usize, usize)] = &[
    (11, 256, 256, 2048),
    (12, 256, 256, 2048),
    (13, 256, 256, 2048),
    (14, 128, 192, 4096),
    (15, 64, 64, 2112),
];

fn problem(seed: u64, m: usize, n: usize, k: usize) -> Problem {
    Problem::generate(m, n, k, header(MEDIUM_NBITS), seed).expect("valid problem")
}

#[test]
fn commitment_matches_official_public_params() {
    for &(seed, m, n, k) in SHAPES {
        let p = problem(seed, m, n, k);
        let c = commit(&p).unwrap();
        let params = reference_params(&p, 0, 0);
        assert_eq!(c.job_key, params.job_key(), "job_key, seed {seed}");
        assert_eq!(c.root_a, params.hash_a, "root_a, seed {seed}");
        assert_eq!(c.root_b, params.hash_b, "root_b, seed {seed}");
        assert_eq!(
            (c.b_noise_seed, c.a_noise_seed),
            params.commitment_hash(params.job_key()),
            "seeds, seed {seed}"
        );
        let compiled = CompiledPublicParams::from(&params);
        assert_eq!(compiled.job_key, c.job_key);
        assert_eq!(compiled.commitment_hash, (c.b_noise_seed, c.a_noise_seed));
        // The job key is the unkeyed hash of the 128-byte header||config message.
        let mut msg = p.header.to_bytes().to_vec();
        msg.extend_from_slice(&p.config.to_bytes());
        assert_eq!(msg.len(), 128);
        assert_eq!(c.job_key, *blake3::hash(&msg).as_bytes());
        // Roots are plain keyed BLAKE3 of the (already chunk-aligned) matrices.
        assert_eq!((m * k) % 1024, 0);
        assert_eq!(
            c.root_a,
            *blake3::keyed_hash(&c.job_key, &matrix_bytes(&p.a)).as_bytes()
        );
    }
}

#[test]
fn noise_matches_compute_noise_for_indices() {
    for &(seed, m, n, k) in SHAPES {
        let p = problem(seed, m, n, k);
        let oracle = Oracle::new(&p).unwrap();
        let c = *oracle.commitment();
        let factors = oracle.noise_factors().unwrap();
        assert!(factors
            .a_l
            .iter()
            .chain(&factors.b_rt)
            .all(|v| (UNIFORM_MIN..=UNIFORM_MAX).contains(v)));
        for &[plus, minus] in factors.a_r.iter().chain(&factors.b_l) {
            assert!((plus as usize) < p.rank() && (minus as usize) < p.rank() && plus != minus);
        }
        let noise = factors.expand().unwrap();
        let rows: Vec<usize> = (0..m).collect();
        let cols: Vec<usize> = (0..n).collect();
        let reference =
            compute_noise_for_indices(k, p.rank(), (c.b_noise_seed, c.a_noise_seed), &rows, &cols);
        assert_eq!(noise.e_a, flatten(&reference.a), "E_A, seed {seed}");
        assert_eq!(noise.e_bt, flatten(&reference.b), "E_Bᵀ, seed {seed}");
        assert!(noise
            .e_a
            .iter()
            .chain(&noise.e_bt)
            .all(|v| v.abs() <= NOISE_ABS_MAX));
        // Noised operands stay within s8 without wrapping.
        let a_noised: Vec<i32> =
            p.a.iter()
                .zip(&noise.e_a)
                .map(|(&x, &e)| i32::from(x) + i32::from(e))
                .collect();
        assert_eq!(
            a_noised,
            oracle
                .noised_a()
                .iter()
                .map(|&v| i32::from(v))
                .collect::<Vec<_>>()
        );
        // The per-tile official path (compute_noise on the tile's own params) agrees too.
        let tiles = [
            (0u32, 0u32),
            (1, 2),
            (
                p.row_offsets()[p.row_offsets().len() - 1],
                p.col_offsets()[p.col_offsets().len() - 1],
            ),
        ];
        for (t_rows, t_cols) in tiles {
            let (na, nb) = reference_tile_noise(&p, t_rows, t_cols);
            for (u, d) in p.row_pattern().iter().enumerate() {
                let i = (t_rows + d) as usize;
                assert_eq!(na[u], noise.e_a[i * k..(i + 1) * k]);
            }
            for (v, d) in p.col_pattern().iter().enumerate() {
                let j = (t_cols + d) as usize;
                assert_eq!(nb[v], noise.e_bt[j * k..(j + 1) * k]);
            }
        }
    }
}

#[test]
fn all_tile_transcripts_and_digests_match_reference() {
    for &(seed, m, n, k) in SHAPES {
        let p = problem(seed, m, n, k);
        let ours = transcripts(&p).unwrap();
        let reference = reference_tiles(&p);
        assert_eq!(ours.len(), p.tile_count());
        assert_eq!(ours.len(), m * n / 128, "8×16 tiles partition the output");
        assert_eq!(
            first_mismatch(&reference, &ours),
            None,
            "seed {seed} ({m}×{n}×{k})"
        );
        assert_eq!(tiles_digest(&ours), tiles_digest(&reference));
    }
}

#[test]
fn extreme_entries_match_reference() {
    // Largest magnitudes the verifier admits (64 and -64): A' reaches ±127 and the i32
    // accumulators their largest values; nothing may wrap or saturate.
    let (m, n, k) = (64usize, 64usize, 4096usize);
    let a = vec![64i8; m * k];
    let bt: Vec<i8> = (0..n * k)
        .map(|i| if (i / k) % 2 == 0 { -64 } else { 64 })
        .collect();
    let p = Problem::from_matrices(header(SATURATED_NBITS), m, n, k, a, bt).unwrap();
    let oracle = Oracle::new(&p).unwrap();
    assert!(oracle
        .noised_a()
        .iter()
        .chain(oracle.noised_bt())
        .all(|v| (-127..=127).contains(v)));
    assert_eq!(
        first_mismatch(&reference_tiles(&p), &oracle.transcripts().unwrap()),
        None
    );
    let proof = oracle
        .build_plain_proof(&oracle.tile(5, 6).unwrap())
        .unwrap();
    verify_v3(&p.header, &proof, None).unwrap();
}

#[test]
fn final_accumulators_equal_the_noised_gemm() {
    for &(seed, m, n, k) in &SHAPES[3..] {
        let p = problem(seed, m, n, k);
        let oracle = Oracle::new(&p).unwrap();
        let c = oracle.gemm();
        let (rows, cols) = (p.row_pattern(), p.col_pattern());
        let covered = k - k % p.rank();
        let last = (
            *p.row_offsets().last().unwrap(),
            *p.col_offsets().last().unwrap(),
        );
        for (t_rows, t_cols) in [(0u32, 0u32), (3, 6), last] {
            let trace = oracle.trace(t_rows, t_cols).unwrap();
            assert_eq!(trace.folds.len(), p.slices());
            for (u, dr) in rows.iter().enumerate() {
                for (v, dc) in cols.iter().enumerate() {
                    let (i, j) = ((t_rows + dr) as usize, (t_cols + dc) as usize);
                    let partial: i32 = (0..covered)
                        .map(|l| {
                            i32::from(oracle.noised_a()[i * k + l])
                                * i32::from(oracle.noised_bt()[j * k + l])
                        })
                        .sum();
                    assert_eq!(trace.acc[u * cols.len() + v], partial);
                    if covered == k {
                        assert_eq!(trace.acc[u * cols.len() + v], c[i * n + j]);
                    }
                }
            }
            // Transcript = the per-slice folds replayed through the rotl-13 schedule.
            let mut t = [0u32; 16];
            for (s, f) in trace.folds.iter().enumerate() {
                t[s % 16] = t[s % 16].rotate_left(13) ^ f;
            }
            assert_eq!(t, trace.tile.transcript);
        }
    }
}

/// Replays the RNG calls of `try_mine_one` (A: m×k then B: k×n, each entry
/// `random_range(-64..=64)`) to rebuild the exact matrices the reference miner uses.
fn replay_try_mine_one_matrices(seed: u64, m: usize, n: usize, k: usize) -> (Vec<i8>, Vec<i8>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let a: Vec<Vec<i8>> = (0..m)
        .map(|_| (0..k).map(|_| rng.random_range(-64i8..=64)).collect())
        .collect();
    let b: Vec<Vec<i8>> = (0..k)
        .map(|_| (0..n).map(|_| rng.random_range(-64i8..=64)).collect())
        .collect();
    (flatten(&a), transpose(&b))
}

#[test]
fn try_mine_one_first_hit_and_proof_are_byte_identical() {
    let (m, n, k) = (256usize, 256usize, 2048usize);
    let cfg = mining_config(k as u32).unwrap();
    let hdr = header(MEDIUM_NBITS);
    let bound = extract_difficulty_bound(MEDIUM_NBITS, &cfg);
    let mut compared = 0;
    for seed in 0..12u64 {
        let reference = try_mine_one(
            &mut StdRng::seed_from_u64(seed),
            m,
            n,
            k,
            hdr,
            cfg,
            None,
            false,
            spm_pow::SeedDerivation::Salted,
        )
        .expect("reference miner runs");
        let (a, bt) = replay_try_mine_one_matrices(seed, m, n, k);
        let p = Problem::from_matrices(hdr, m, n, k, a, bt).unwrap();
        let hits = find_hits(&p, bound).unwrap();
        match reference {
            Some(ref_proof) => {
                let first = hits.first().expect("the reference found a hit, so must we");
                assert_eq!(
                    first.t_rows as usize, ref_proof.a.row_indices[0],
                    "seed {seed}"
                );
                assert_eq!(
                    first.t_cols as usize, ref_proof.bt.row_indices[0],
                    "seed {seed}"
                );
                let ours = build_plain_proof(&p, first).unwrap();
                assert_eq!(
                    bincode::serialize(&ours).unwrap(),
                    bincode::serialize(&ref_proof).unwrap(),
                    "seed {seed}"
                );
                verify_v3(&hdr, &ours, None).unwrap();
                compared += 1;
            }
            None => assert!(
                hits.is_empty(),
                "seed {seed}: reference found nothing but we found {hits:?}"
            ),
        }
        if compared == 4 {
            break;
        }
    }
    assert_eq!(
        compared, 4,
        "expected four reference proofs among the seeds"
    );
}

#[test]
fn try_mine_one_miss_agrees() {
    let (m, n, k) = (256usize, 256usize, 2048usize);
    let cfg = mining_config(k as u32).unwrap();
    let hdr = header(HARD_NBITS);
    let bound = extract_difficulty_bound(HARD_NBITS, &cfg);
    let seed = 1_000;
    let reference = try_mine_one(
        &mut StdRng::seed_from_u64(seed),
        m,
        n,
        k,
        hdr,
        cfg,
        None,
        false,
        spm_pow::SeedDerivation::Salted,
    )
    .unwrap();
    let (a, bt) = replay_try_mine_one_matrices(seed, m, n, k);
    let p = Problem::from_matrices(hdr, m, n, k, a, bt).unwrap();
    let hits = find_hits(&p, bound).unwrap();
    assert_eq!(reference.is_none(), hits.is_empty());
    // And the "wrong jackpot" mode (accepts the first tile that does NOT meet the bound) points
    // at our first non-hit.
    let wrong = try_mine_one(
        &mut StdRng::seed_from_u64(seed),
        m,
        n,
        k,
        hdr,
        cfg,
        None,
        true,
        spm_pow::SeedDerivation::Salted,
    )
    .unwrap()
    .expect("almost every tile misses");
    let first_miss = transcripts(&p)
        .unwrap()
        .into_iter()
        .find(|t| !t.meets(bound))
        .unwrap();
    let ours = build_plain_proof(&p, &first_miss).unwrap();
    assert_eq!(
        bincode::serialize(&ours).unwrap(),
        bincode::serialize(&wrong).unwrap()
    );
    assert!(verify_v3(&hdr, &ours, None).is_err());
}
