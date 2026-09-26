//! Known-answer test, run before `Ready` (and required to pass before any mining).
//!
//! One fixed 256 × 256 × 2048 job goes through the whole production path: the host job
//! (fills, layer caches, seed chain, nonce patch) and the engine in dump mode. The engine's 512
//! tile records must equal `spm_cpuref::transcripts` of the same problem built independently from
//! whole matrices (`first_mismatch == None`), the host's roots and seeds must equal the oracle's
//! commitment, the engine's hits must equal the oracle's, and the host proof of a hit must be
//! byte-identical to `spm_cpuref::build_plain_proof` and pass the official verifier.

use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use spm_cpuref::{
    build_plain_proof, first_mismatch, tiles_digest, verify_v3, Oracle, Problem, U256,
};
use spm_proto::Job;
use spm_work::{Shape, WorkUnit};

use crate::engine::{ChunkStatus, Engine, JobSpec};
use crate::host::JobHost;

/// The KAT shape.
pub const KAT_SHAPE: Shape = Shape {
    m: 256,
    n: 256,
    k: 2048,
    r: 128,
};
/// The nonce of the KAT attempt.
pub const KAT_NONCE: u64 = 0x4b41_545f_6e6f_6e63;

/// Summary of a passed test.
#[derive(Debug, Clone)]
pub struct KatReport {
    pub tiles: usize,
    pub hits: usize,
    pub tiles_digest: [u8; 32],
    pub elapsed: Duration,
}

/// The fixed work unit of the test: share bound 2^251 (≈ 1 hit in 32 tiles), a block bound far
/// out of reach.
pub fn kat_work_unit() -> WorkUnit {
    let mut header = [0u8; 76];
    header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    header[4..36].fill(0x4b);
    header[36..68].fill(0x41);
    header[68..72].copy_from_slice(&0x6666_6666u32.to_le_bytes());
    header[72..76].copy_from_slice(&0x1c7f_ffffu32.to_le_bytes());
    let job = Job {
        job_id: "kat".into(),
        header,
        target: U256::one() << 233,
        height: None,
        diff: None,
        cert_version: Some(3),
    };
    WorkUnit::build(&job, KAT_SHAPE, 0, 0).expect("the KAT work unit is valid")
}

/// Runs the test on `engine` (its job is destroyed afterwards).
pub fn known_answer_test<E: Engine + ?Sized>(engine: &mut E) -> Result<KatReport> {
    let start = Instant::now();
    let result = run(engine);
    engine.destroy_job();
    result.map(|(tiles, hits, digest)| KatReport {
        tiles,
        hits,
        tiles_digest: digest,
        elapsed: start.elapsed(),
    })
}

fn run<E: Engine + ?Sized>(engine: &mut E) -> Result<(usize, usize, [u8; 32])> {
    let wu = kat_work_unit();
    let host = JobHost::new(&wu).context("KAT host job")?;
    let att = host.attempt(KAT_NONCE);

    // The reference, from whole matrices.
    let (a, bt) = host.matrices(&att);
    let (m, n, k) = (
        KAT_SHAPE.m as usize,
        KAT_SHAPE.n as usize,
        KAT_SHAPE.k as usize,
    );
    let problem = Problem::from_matrices(*host.header(), m, n, k, a, bt)?;
    let oracle = Oracle::new(&problem)?;
    let c = oracle.commitment();
    ensure!(
        c.root_a == att.root_a && c.root_b == host.root_b(),
        "host Merkle roots differ from the oracle's"
    );
    ensure!(
        c.a_noise_seed == att.a_noise_seed && c.b_noise_seed == host.b_noise_seed(),
        "host seed chain differs from the oracle's"
    );
    let expected = oracle.transcripts()?;
    let bound = wu.share_bound();
    let expected_hits: Vec<_> = expected.iter().filter(|t| t.meets(bound)).collect();
    ensure!(!expected_hits.is_empty(), "the KAT problem has no hit");

    // The engine, in dump mode.
    let spec = JobSpec {
        dump: true,
        ..host.spec()
    };
    // The test is short and runs before `Ready`: a pause or exit request that arrives meanwhile
    // (a signal) is served right after it, so an abort here only re-runs the chunk.
    let abort = engine.abort_flag();
    abort.clear();
    engine.create_job(&spec).map_err(|e| anyhow!("{e}"))?;
    engine
        .set_attempt(&att.a_noise_seed, &wu.share_bound_le(), &att.prefix())
        .map_err(|e| anyhow!("{e}"))?;
    let mut aborted = 0u32;
    loop {
        abort.clear();
        match engine.run_chunk().map_err(|e| anyhow!("{e}"))?.status {
            ChunkStatus::More => continue,
            ChunkStatus::Done => break,
            ChunkStatus::Aborted => {
                aborted += 1;
                ensure!(aborted < 1000, "the KAT attempt keeps being aborted");
            }
        }
    }
    let got = engine.dump().map_err(|e| anyhow!("{e}"))?;
    if let Some(i) = first_mismatch(&expected, &got) {
        anyhow::bail!(
            "tile record {i} differs from the oracle (expected {:?}, got {:?})",
            expected.get(i).map(|t| (t.t_rows, t.t_cols, t.digest)),
            got.get(i).map(|t| (t.t_rows, t.t_cols, t.digest))
        );
    }
    let (mut hits, total) = engine.hits().map_err(|e| anyhow!("{e}"))?;
    hits.sort_by_key(|h| (h.t_rows, h.t_cols));
    ensure!(
        total as usize == expected_hits.len()
            && hits.len() == expected_hits.len()
            && hits
                .iter()
                .zip(&expected_hits)
                .all(|(h, e)| (h.t_rows, h.t_cols, h.digest) == (e.t_rows, e.t_cols, e.digest)),
        "engine hits differ from the oracle's ({} vs {})",
        total,
        expected_hits.len()
    );

    // Host canary and proof path on the first hit.
    let first = expected_hits[0];
    ensure!(
        host.tile(&att, first.t_rows, first.t_cols)? == *first,
        "host canary tile differs from the oracle"
    );
    let proof = host.proof(&att, first.t_rows, first.t_cols)?;
    ensure!(
        bincode::serialize(&proof)? == bincode::serialize(&build_plain_proof(&problem, first)?)?,
        "host proof differs from spm_cpuref::build_plain_proof"
    );
    verify_v3(host.header(), &proof, Some(wu.nbits_share))
        .context("KAT proof rejected by the verifier")?;
    Ok((got.len(), hits.len(), tiles_digest(&got)))
}
