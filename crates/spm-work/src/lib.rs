//! spm-work — turns a pool `Job` plus our fixed problem shape into a `WorkUnit`: everything the
//! GPU worker needs for one job, computed once on the host.
//!
//! - `nbits_share = compact(target)`; `share_bound = spm_pow::share_bound(target, config)` —
//!   the smaller of `target` and `expand(compact(target))`, rank-penalized, exactly what pools
//!   verify with `nbits_override`. `None` (overflow) ⇒ the job is refused.
//! - `block_bound = extract_difficulty_bound(header.nbits, config)` — the consensus bound; a
//!   share whose hash is also ≤ this bound is a block.
//! - `job_key = blake3(header76 ‖ config52)` (plain, unkeyed; same as zk-pow `compute_job_key`).
//! - `fill_seed = blake3(job_key ‖ "spm/fill/v1")` seeds the structured A/B fills.
//! - `cert_version` gate: only V3 is mined; ≥ 4, any other value, or a missing field ⇒
//!   `UpdateRequired` (the pool is paused, never an invalid share).
#![forbid(unsafe_code)]

use primitive_types::U256;
use serde::{Deserialize, Serialize};
use spm_pow::{compact_from_target, extract_difficulty_bound, mining_config, share_bound, IncompleteBlockHeader, MiningConfiguration, NOISE_RANK};
use spm_proto::Job;

/// Domain separator for the per-job fill seed.
pub const FILL_DOMAIN: &[u8] = b"spm/fill/v1";
/// The only certificate version this build mines (V3, salted seeds).
pub const SUPPORTED_CERT_VERSION: u32 = 3;
/// Row/column pattern period of our 8x16 hash tile: m and n must be multiples of it.
pub const PATTERN_PERIOD: u32 = 64;

/// Problem shape of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Shape {
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub r: u16,
}

impl Shape {
    /// The production shape: m = n = 131072, k = 4096, r = 128.
    pub const MINING: Shape = Shape { m: 131_072, n: 131_072, k: 4096, r: NOISE_RANK };

    /// Check the shape against the verifier's public-parameter rules and our tile pattern.
    pub fn validate(&self) -> Result<(), WorkError> {
        let Shape { m, n, k, r } = *self;
        let (k, r32) = (k as u64, r as u64);
        let bad = |why: &str| Err(WorkError::BadShape(format!("{self:?}: {why}")));
        if r != NOISE_RANK {
            return bad("the noise rank must be 128");
        }
        if k % 64 != 0 || k < 1024 || k < 16 * r32 || k > 4 * r32 * r32 || k > 1 << 16 {
            return bad("k must be a multiple of 64 with max(1024, 16r) <= k <= min(4r^2, 2^16)");
        }
        for (name, d) in [("m", m), ("n", n)] {
            if d == 0 || d % PATTERN_PERIOD != 0 || d > 1 << 24 {
                return bad(&format!("{name} must be a positive multiple of 64 and at most 2^24"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkError {
    #[error("network upgrade – update required (pool sent cert_version {0:?}; this build mines only 3)")]
    UpdateRequired(Option<u32>),
    #[error("share target {0:#x} is unusable (zero, or the penalized bound overflows 256 bits)")]
    UnusableTarget(U256),
    #[error("bad shape {0}")]
    BadShape(String),
    #[error("mining configuration: {0}")]
    Config(String),
}

/// Where a jackpot hash lands relative to a work unit's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashClass {
    Miss,
    Share,
    /// A share that also meets the header's nbits.
    Block,
}

/// Everything the GPU worker needs for one job. Bounds are stored big-endian; the jackpot hash
/// is compared as a little-endian 256-bit integer (as consensus does), see [`WorkUnit::classify`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnit {
    pub wu_id: u64,
    pub session_id: u64,
    pub job_id: String,
    #[serde(with = "fixed_bytes")]
    pub header: [u8; 76],
    #[serde(with = "fixed_bytes")]
    pub config: [u8; 52],
    pub shape: Shape,
    pub cert_version: u32,
    pub height: Option<u64>,
    pub diff: Option<u64>,
    /// The pool's share target, as sent.
    pub share_target: [u8; 32],
    pub nbits_share: u32,
    pub share_bound: [u8; 32],
    pub block_nbits: u32,
    pub block_bound: [u8; 32],
    pub job_key: [u8; 32],
    pub fill_seed: [u8; 32],
}

fn be(x: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    x.to_big_endian(&mut b);
    b
}

/// Little-endian bytes of a 256-bit integer (the jackpot hash byte order).
pub fn le_bytes(x: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    x.to_little_endian(&mut b);
    b
}

impl WorkUnit {
    /// Build a work unit for the production shape.
    pub fn from_job(job: &Job, session_id: u64, wu_id: u64) -> Result<WorkUnit, WorkError> {
        Self::build(job, Shape::MINING, session_id, wu_id)
    }

    /// Build a work unit for any valid shape (tests and CPU benches use small ones).
    pub fn build(job: &Job, shape: Shape, session_id: u64, wu_id: u64) -> Result<WorkUnit, WorkError> {
        let cert_version = match job.cert_version {
            Some(SUPPORTED_CERT_VERSION) => SUPPORTED_CERT_VERSION,
            other => return Err(WorkError::UpdateRequired(other)),
        };
        shape.validate()?;
        let cfg = mining_config(shape.k).map_err(|e| WorkError::Config(e.to_string()))?;
        let config: [u8; 52] = cfg.to_bytes();
        if job.target.is_zero() {
            return Err(WorkError::UnusableTarget(job.target));
        }
        let bound = share_bound(job.target, &cfg).filter(|b| !b.is_zero()).ok_or(WorkError::UnusableTarget(job.target))?;
        let nbits_share = compact_from_target(job.target);
        let block_nbits = job.nbits();
        let block_bound = extract_difficulty_bound(block_nbits, &cfg);
        let job_key = job_key(&job.header, &config);
        Ok(WorkUnit {
            wu_id,
            session_id,
            job_id: job.job_id.clone(),
            header: job.header,
            config,
            shape,
            cert_version,
            height: job.height,
            diff: job.diff,
            share_target: be(job.target),
            nbits_share,
            share_bound: be(bound),
            block_nbits,
            block_bound: be(block_bound),
            job_key,
            fill_seed: fill_seed(&job_key),
        })
    }

    pub fn share_target(&self) -> U256 {
        U256::from_big_endian(&self.share_target)
    }

    pub fn share_bound(&self) -> U256 {
        U256::from_big_endian(&self.share_bound)
    }

    pub fn block_bound(&self) -> U256 {
        U256::from_big_endian(&self.block_bound)
    }

    /// The share bound as the GPU compares it: little-endian bytes of the 256-bit integer.
    pub fn share_bound_le(&self) -> [u8; 32] {
        le_bytes(self.share_bound())
    }

    /// Classify a jackpot hash (interpreted little-endian). A hash is a block only if it is also
    /// a share: we submit through the pool, which verifies against `nbits_share`.
    pub fn classify(&self, jackpot_hash: &[u8; 32]) -> HashClass {
        let h = U256::from_little_endian(jackpot_hash);
        if h > self.share_bound() {
            HashClass::Miss
        } else if h <= self.block_bound() {
            HashClass::Block
        } else {
            HashClass::Share
        }
    }

    pub fn block_header(&self) -> Result<IncompleteBlockHeader, WorkError> {
        IncompleteBlockHeader::from_bytes(&self.header).map_err(|e| WorkError::Config(e.to_string()))
    }

    pub fn mining_config(&self) -> Result<MiningConfiguration, WorkError> {
        MiningConfiguration::from_bytes(&self.config).map_err(|e| WorkError::Config(e.to_string()))
    }
}

/// `blake3(header76 ‖ config52)`, unkeyed.
pub fn job_key(header: &[u8; 76], config: &[u8; 52]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(header);
    h.update(config);
    *h.finalize().as_bytes()
}

/// `blake3(job_key ‖ "spm/fill/v1")`.
pub fn fill_seed(job_key: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(job_key);
    h.update(FILL_DOMAIN);
    *h.finalize().as_bytes()
}

/// Serde for byte arrays longer than 32 (serde's built-in array support stops at 32).
pub mod fixed_bytes {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        struct V<const N: usize>;
        impl<'de, const N: usize> Visitor<'de> for V<N> {
            type Value = [u8; N];
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "{N} bytes")
            }
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<[u8; N], E> {
                v.try_into().map_err(|_| E::invalid_length(v.len(), &self))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
                let mut out = [0u8; N];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = seq.next_element()?.ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                if seq.next_element::<u8>()?.is_some() {
                    return Err(A::Error::invalid_length(N + 1, &self));
                }
                Ok(out)
            }
        }
        d.deserialize_bytes(V::<N>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spm_pow::{target_from_compact, IncompleteBlockHeader};
    use spm_proto::parse_notify;

    const HEROMINERS: &str = include_str!("../../../tests/fixtures/capture-herominers-br-authorize.jsonl");
    const LUCKYPOOL: &str = include_str!("../../../tests/fixtures/capture-luckypool-br-authorize.jsonl");
    const KRYPTEX: &str = include_str!("../../../tests/fixtures/capture-kryptex-8048-authorize.jsonl");

    fn fixture_jobs(text: &str) -> Vec<Job> {
        text.lines()
            .filter(|l| l.contains("\"ev\": \"job\""))
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                parse_notify(&v["msg"].to_string()).unwrap().unwrap()
            })
            .collect()
    }

    #[test]
    fn herominers_jobs_become_work_units() {
        let jobs = fixture_jobs(HEROMINERS);
        assert!(!jobs.is_empty());
        for (i, job) in jobs.iter().enumerate() {
            let wu = WorkUnit::from_job(job, 7, i as u64).unwrap();
            assert_eq!(wu.shape, Shape::MINING);
            assert_eq!((wu.session_id, wu.wu_id), (7, i as u64));
            assert_eq!(wu.cert_version, 3);
            // diff 2^21: target 0x7fff8 << 184, nbits 0x1a07fff8, bound = target * 2^19 (h*w*k = 128*4096).
            assert_eq!(wu.share_target(), U256::from(0x7fff8u64) << 184);
            assert_eq!(wu.nbits_share, 0x1a07_fff8);
            assert_eq!(wu.share_bound(), wu.share_target() << 19);
            assert_eq!(wu.block_nbits, job.nbits());
            assert_eq!(wu.block_bound(), target_from_compact(job.nbits()) << 19);
            assert!(wu.block_bound() < wu.share_bound(), "the network is harder than the pool");
            assert_eq!(wu.header, job.header);
            assert_eq!(wu.mining_config().unwrap().to_bytes(), wu.config);
            assert_eq!(wu.block_header().unwrap().to_bytes(), job.header);
        }
    }

    #[test]
    fn luckypool_vardiff_target_rounds_down_through_compact() {
        for job in fixture_jobs(LUCKYPOOL) {
            let wu = WorkUnit::from_job(&job, 1, 1).unwrap();
            let rounded = target_from_compact(wu.nbits_share);
            // 888,888 is not a power of two: compact loses bits, and the bound uses the smaller value.
            assert!(rounded < job.target);
            assert_eq!(wu.share_bound(), rounded << 19);
        }
    }

    #[test]
    fn kryptex_target_uses_the_value_as_sent() {
        for job in fixture_jobs(KRYPTEX) {
            let wu = WorkUnit::from_job(&job, 1, 1).unwrap();
            assert_eq!(wu.share_target(), job.target);
            let rounded = target_from_compact(wu.nbits_share);
            assert!(rounded <= job.target);
            assert_eq!(wu.share_bound(), rounded.min(job.target) << 19);
        }
    }

    #[test]
    fn job_key_and_fill_seed_follow_the_reference_derivation() {
        let job = &fixture_jobs(HEROMINERS)[0];
        let wu = WorkUnit::from_job(job, 1, 1).unwrap();
        let mut data = job.header.to_vec();
        data.extend_from_slice(&mining_config(4096).unwrap().to_bytes());
        assert_eq!(data.len(), 128);
        assert_eq!(wu.job_key, *blake3::hash(&data).as_bytes());
        let mut seed_in = wu.job_key.to_vec();
        seed_in.extend_from_slice(b"spm/fill/v1");
        assert_eq!(wu.fill_seed, *blake3::hash(&seed_in).as_bytes());
        // Distinct headers ⇒ distinct keys; the key depends on k through the config.
        let wu2 = WorkUnit::from_job(&fixture_jobs(HEROMINERS)[1], 1, 2).unwrap();
        assert_ne!(wu.job_key, wu2.job_key);
        let small = WorkUnit::build(job, Shape { m: 256, n: 256, k: 2048, r: 128 }, 1, 3).unwrap();
        assert_ne!(small.job_key, wu.job_key);
        // Header round trip through the official type keeps the 76 bytes (hash fields reversed inside).
        let hdr = IncompleteBlockHeader::from_bytes(&job.header).unwrap();
        assert_eq!(hdr.to_bytes(), job.header);
    }

    #[test]
    fn cert_version_gate() {
        let base = fixture_jobs(HEROMINERS)[0].clone();
        for (cv, ok) in [(Some(3), true), (Some(4), false), (Some(7), false), (Some(2), false), (None, false), (Some(u32::MAX), false)] {
            let mut j = base.clone();
            j.cert_version = cv;
            match WorkUnit::from_job(&j, 1, 1) {
                Ok(_) => assert!(ok, "{cv:?} must be refused"),
                Err(e) => {
                    assert!(!ok, "{cv:?} must be accepted");
                    assert_eq!(e, WorkError::UpdateRequired(cv));
                    assert!(e.to_string().contains("update required"));
                }
            }
        }
    }

    #[test]
    fn unusable_targets_are_refused() {
        let mut j = fixture_jobs(HEROMINERS)[0].clone();
        j.target = U256::zero();
        assert!(matches!(WorkUnit::from_job(&j, 1, 1), Err(WorkError::UnusableTarget(_))));
        j.target = U256::MAX >> 4; // the penalized bound would overflow
        assert!(matches!(WorkUnit::from_job(&j, 1, 1), Err(WorkError::UnusableTarget(_))));
    }

    #[test]
    fn shapes_are_validated() {
        assert!(Shape::MINING.validate().is_ok());
        assert!(Shape { m: 256, n: 256, k: 2048, r: 128 }.validate().is_ok());
        for bad in [
            Shape { m: 256, n: 256, k: 2048, r: 64 },
            Shape { m: 256, n: 256, k: 1024, r: 128 },  // k < 16r
            Shape { m: 256, n: 256, k: 2112, r: 128 },  // fine for zk-pow (64 | k) ...
            Shape { m: 100, n: 256, k: 2048, r: 128 },
            Shape { m: 256, n: 0, k: 2048, r: 128 },
            Shape { m: 256, n: 256, k: 70_000, r: 128 },
        ] {
            let r = bad.validate();
            if bad.k == 2112 {
                assert!(r.is_ok(), "k = 2112 is valid");
            } else {
                assert!(r.is_err(), "{bad:?}");
            }
        }
        let job = &fixture_jobs(HEROMINERS)[0];
        assert!(matches!(WorkUnit::build(job, Shape { m: 100, n: 256, k: 2048, r: 128 }, 1, 1), Err(WorkError::BadShape(_))));
    }

    #[test]
    fn classify_against_both_bounds() {
        let job = &fixture_jobs(HEROMINERS)[0];
        let wu = WorkUnit::from_job(job, 1, 1).unwrap();
        let le = le_bytes;
        assert_eq!(wu.classify(&le(wu.share_bound())), HashClass::Share);
        assert_eq!(wu.classify(&le(wu.share_bound() + 1)), HashClass::Miss);
        assert_eq!(wu.classify(&le(wu.block_bound())), HashClass::Block);
        assert_eq!(wu.classify(&le(U256::zero())), HashClass::Block);
        assert_eq!(wu.classify(&le(U256::MAX)), HashClass::Miss);
        assert_eq!(wu.share_bound_le(), le(wu.share_bound()));
    }

    #[test]
    fn serde_round_trips_bincode_and_json() {
        let wu = WorkUnit::from_job(&fixture_jobs(KRYPTEX)[0], 3, 9).unwrap();
        let b = bincode::serialize(&wu).unwrap();
        assert_eq!(bincode::deserialize::<WorkUnit>(&b).unwrap(), wu);
        let j = serde_json::to_string(&wu).unwrap();
        assert_eq!(serde_json::from_str::<WorkUnit>(&j).unwrap(), wu);
        // A truncated header is refused, not padded.
        let mut v: serde_json::Value = serde_json::from_str(&j).unwrap();
        v["header"].as_array_mut().unwrap().pop();
        assert!(serde_json::from_value::<WorkUnit>(v).is_err());
    }
}
