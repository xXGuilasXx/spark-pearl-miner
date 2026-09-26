//! spm-proto — wire dialects for Pearl pools. Pure data + codec; no sockets here.
//! Dialects: `object` (HeroMiners — captured live 2026-09-26 — and LuckyPool), `kryptex` (v1 arrays).
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const AGENT: &str = concat!("spark-pearl-miner/", env!("CARGO_PKG_VERSION"));
/// Read cap for one NDJSON line (proof submits are 100–370 KB; pools may send big lines).
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialect {
    /// authorize-first with an object; notify/submit params are objects (HeroMiners, LuckyPool).
    Object,
    /// stratum-v1 style: subscribe [agent], authorize ["wallet.worker", "x"]; notify/submit objects (Kryptex).
    Kryptex,
}

/// A parsed `mining.notify`. Fields are identical across the object and kryptex dialects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub job_id: String,
    /// 76-byte IncompleteBlockHeader: version u32 LE | prev_block 32 | merkle_root 32 | timestamp u32 LE | nbits u32 LE
    pub header: [u8; 76],
    /// Big-endian 256-bit share target as sent by the pool (use as-is; do not recompute from diff).
    pub target: U256,
    pub height: Option<u64>,
    /// Share difficulty when the pool sends it (LuckyPool `diff`); otherwise parsed from `job_id` suffix.
    pub diff: Option<u64>,
    /// Certificate version (3 = V3 salted seeds). `None` ⇒ assume 3. `>= 4` ⇒ "update required".
    pub cert_version: Option<u32>,
}

impl Job {
    pub fn nbits(&self) -> u32 {
        u32::from_le_bytes(self.header[72..76].try_into().unwrap())
    }
    pub fn requires_update(&self) -> bool {
        matches!(self.cert_version, Some(v) if v >= 4)
    }
    /// Pool convention (Bitcoin pdiff): target = floor(0xFFFF * 2^208 / diff).
    pub fn target_for_diff(diff: u64) -> U256 {
        (U256::from(0xFFFFu64) << 208) / U256::from(diff)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("not a mining.notify")]
    NotNotify,
    #[error("bad field {0}")]
    BadField(&'static str),
    #[error("line too long ({0} bytes)")]
    LineTooLong(usize),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Messages the client sends. Each is one NDJSON line.
pub fn authorize_msg(dialect: Dialect, id: u64, wallet: &str, worker: &str, password: &str) -> Vec<Value> {
    match dialect {
        Dialect::Object => vec![json!({"id": id, "method": "mining.authorize",
            "params": {"wallet": wallet, "worker": worker, "agent": AGENT}})],
        Dialect::Kryptex => vec![
            json!({"id": id, "method": "mining.subscribe", "params": [AGENT]}),
            json!({"id": id + 1, "method": "mining.authorize", "params": [format!("{wallet}.{worker}"), password]}),
        ],
    }
}

/// `mining.submit`. `proof_field` is learned per pool ("plain_proof" or "plain_proof_zst").
pub fn submit_msg(dialect: Dialect, id: u64, wallet: &str, worker: &str, job_id: &str, proof_field: &str, proof_b64: &str) -> Value {
    match dialect {
        Dialect::Object => json!({"id": id, "method": "mining.submit",
            "params": {"wallet": wallet, "worker": worker, "job_id": job_id, proof_field: proof_b64}}),
        Dialect::Kryptex => json!({"id": id, "method": "mining.submit",
            "params": {"worker": format!("{wallet}.{worker}"), "job_id": job_id, proof_field: proof_b64}}),
    }
}

/// Parse one server line. Returns `Ok(Some(job))` for a notify, `Ok(None)` for anything else.
pub fn parse_notify(line: &str) -> Result<Option<Job>, ProtoError> {
    if line.len() > MAX_LINE_BYTES { return Err(ProtoError::LineTooLong(line.len())); }
    let v: Value = serde_json::from_str(line)?;
    if v.get("method").and_then(Value::as_str) != Some("mining.notify") { return Ok(None); }
    let p = v.get("params").ok_or(ProtoError::BadField("params"))?;
    let job_id = p.get("job_id").and_then(Value::as_str).ok_or(ProtoError::BadField("job_id"))?.to_string();
    let hh = p.get("header").and_then(Value::as_str).ok_or(ProtoError::BadField("header"))?;
    let hb = hex::decode(hh).map_err(|_| ProtoError::BadField("header"))?;
    let header: [u8; 76] = hb.try_into().map_err(|_| ProtoError::BadField("header"))?;
    let th = p.get("target").and_then(Value::as_str).ok_or(ProtoError::BadField("target"))?;
    let tb = hex::decode(format!("{:0>64}", th)).map_err(|_| ProtoError::BadField("target"))?;
    let target = U256::from_big_endian(&tb);
    let height = p.get("height").and_then(Value::as_u64);
    let diff = p.get("diff").and_then(Value::as_u64)
        .or_else(|| job_id.rsplit('_').next().and_then(|s| s.parse().ok()));
    let cert_version = p.get("cert_version").and_then(Value::as_u64).map(|x| x as u32);
    Ok(Some(Job { job_id, header, target, height, diff, cert_version }))
}

/// Result of an id-matched reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply { Accepted, Rejected(String), Unrelated }

pub fn parse_reply(line: &str, expect_id: u64) -> Result<Reply, ProtoError> {
    let v: Value = serde_json::from_str(line)?;
    if v.get("id").and_then(Value::as_u64) != Some(expect_id) { return Ok(Reply::Unrelated); }
    let err = v.get("error");
    let err_is_null = matches!(err, None | Some(Value::Null));
    let ok = err_is_null && !matches!(v.get("result"), None | Some(Value::Null) | Some(Value::Bool(false)));
    if ok { Ok(Reply::Accepted) } else { Ok(Reply::Rejected(err.map(|e| e.to_string()).unwrap_or_else(|| "result false".into()))) }
}
