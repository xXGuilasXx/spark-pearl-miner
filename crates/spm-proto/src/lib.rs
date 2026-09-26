//! spm-proto — wire dialects for Pearl pools.
//!
//! Dialects: `object` (HeroMiners — captured live 2026-09-26 — and LuckyPool), `kryptex` (v1 arrays)
//! and `kryptex-v2` (object authorize with `"type":"v2"`, gzip proofs; from open-source clients,
//! not yet confirmed live).
//!
//! Layout:
//! - this file: message builders and parsers (pure data, no I/O);
//! - [`codec`]: NDJSON framing (4 MiB read cap, 2 MiB write guard, CRLF tolerant);
//! - [`tls`]: `TlsMode` off/on/auto/pinned and the connector;
//! - [`encode`]: proof encoders (plain/zstd/gzip) and the per-pool proof-field learner;
//! - [`client`]: a dumb, observable async `PoolSession` (no reconnection, no failover).
#![forbid(unsafe_code)]

pub mod client;
pub mod codec;
pub mod encode;
pub mod tls;

use primitive_types::U256;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

pub use encode::{ProofEncoding, ProofField};

pub const AGENT: &str = concat!("spark-pearl-miner/", env!("CARGO_PKG_VERSION"));
/// Read cap for one NDJSON line (proof submits are 100–370 KB; pools may send big lines).
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// The value of the optional `jsonrpc` member.
pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dialect {
    /// authorize-first with an object; notify/submit params are objects (HeroMiners, LuckyPool).
    Object,
    /// stratum-v1 style: subscribe [agent], authorize ["wallet.worker", "x"]; notify/submit objects (Kryptex).
    Kryptex,
    /// Kryptex "v2" session: object authorize `{"wallet":"addr.worker","agent":..,"type":"v2"}`; when the
    /// ack echoes `type:"v2"` every submit carries base64(gzip(bincode)). Not yet confirmed live.
    KryptexV2,
}

impl Dialect {
    /// Whether requests carry `"jsonrpc":"2.0"` unless the pool preset says otherwise.
    /// HeroMiners did not need it; LuckyPool does (its preset overrides this); Kryptex accepted it.
    pub fn default_jsonrpc(self) -> bool {
        matches!(self, Dialect::Kryptex | Dialect::KryptexV2)
    }

    /// Encoding of the `plain_proof` field for this dialect.
    pub fn plain_field_encoding(self) -> ProofEncoding {
        match self {
            Dialect::KryptexV2 => ProofEncoding::Gzip,
            Dialect::Object | Dialect::Kryptex => ProofEncoding::Plain,
        }
    }
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
    /// Certificate version (3 = V3 salted seeds). `None` means the pool did not say: this build
    /// treats that as unknown and refuses the job (see `spm-work`). Values that do not fit in
    /// `u32` are clamped to `u32::MAX` so they can never alias a known version.
    pub cert_version: Option<u32>,
}

impl Job {
    /// Header nbits (last 4 bytes, little-endian).
    pub fn nbits(&self) -> u32 {
        u32::from_le_bytes([self.header[72], self.header[73], self.header[74], self.header[75]])
    }
    /// `true` when the job's certificate version is not one this build can mine (≥ 4, or missing).
    pub fn requires_update(&self) -> bool {
        !matches!(self.cert_version, Some(3))
    }
    /// Pool convention (Bitcoin pdiff): target = floor(0xFFFF * 2^208 / diff).
    pub fn target_for_diff(diff: u64) -> U256 {
        (U256::from(0xFFFFu64) << 208) / U256::from(diff.max(1))
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

/// Options for the handshake/submit frames that vary per pool rather than per dialect.
#[derive(Debug, Clone, Copy)]
pub struct FrameOpts<'a> {
    /// Add `"jsonrpc":"2.0"` as the first member of every request.
    pub jsonrpc: bool,
    /// Agent string sent in the handshake.
    pub agent: &'a str,
}

impl FrameOpts<'static> {
    pub fn for_dialect(dialect: Dialect) -> Self {
        FrameOpts { jsonrpc: dialect.default_jsonrpc(), agent: AGENT }
    }
}

/// One request, serialized with a fixed member order: `jsonrpc` (optional), `id`, `method`, `params`.
/// This is the order every pool accepted in the captures.
#[derive(Serialize)]
struct Request<'a, P: Serialize> {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<&'static str>,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Serialize)]
struct ObjectLogin<'a> {
    wallet: &'a str,
    worker: &'a str,
    agent: &'a str,
}

#[derive(Serialize)]
struct KryptexV2Login<'a> {
    wallet: String,
    agent: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Submit params with a dynamic proof member name, in the order wallet?, worker, job_id, proof.
struct SubmitParams<'a> {
    wallet: Option<&'a str>,
    worker: &'a str,
    job_id: &'a str,
    field: &'a str,
    proof_b64: &'a str,
}

impl Serialize for SubmitParams<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(if self.wallet.is_some() { 4 } else { 3 }))?;
        if let Some(w) = self.wallet {
            m.serialize_entry("wallet", w)?;
        }
        m.serialize_entry("worker", self.worker)?;
        m.serialize_entry("job_id", self.job_id)?;
        m.serialize_entry(self.field, self.proof_b64)?;
        m.end()
    }
}

fn request_line<P: Serialize>(opts: &FrameOpts<'_>, id: u64, method: &str, params: P) -> Result<String, ProtoError> {
    let req = Request { jsonrpc: opts.jsonrpc.then_some(JSONRPC_VERSION), id, method, params };
    Ok(serde_json::to_string(&req)?)
}

/// The handshake as serialized lines (without the trailing newline), in sending order.
/// Ids are `id`, `id + 1`, ...; the authorize is always the last line.
pub fn authorize_lines(
    dialect: Dialect,
    id: u64,
    wallet: &str,
    worker: &str,
    password: &str,
    opts: &FrameOpts<'_>,
) -> Result<Vec<String>, ProtoError> {
    Ok(match dialect {
        Dialect::Object => vec![request_line(
            opts,
            id,
            "mining.authorize",
            ObjectLogin { wallet, worker, agent: opts.agent },
        )?],
        Dialect::Kryptex => vec![
            request_line(opts, id, "mining.subscribe", [opts.agent])?,
            request_line(opts, id + 1, "mining.authorize", [format!("{wallet}.{worker}"), password.to_string()])?,
        ],
        Dialect::KryptexV2 => vec![request_line(
            opts,
            id,
            "mining.authorize",
            KryptexV2Login { wallet: format!("{wallet}.{worker}"), agent: opts.agent, kind: "v2" },
        )?],
    })
}

/// `mining.submit` as one serialized line (no trailing newline).
/// `proof_field` is learned per pool ("plain_proof" or "plain_proof_zst").
#[allow(clippy::too_many_arguments)]
pub fn submit_line(
    dialect: Dialect,
    id: u64,
    wallet: &str,
    worker: &str,
    job_id: &str,
    proof_field: &str,
    proof_b64: &str,
    opts: &FrameOpts<'_>,
) -> Result<String, ProtoError> {
    let login;
    let params = match dialect {
        Dialect::Object => SubmitParams { wallet: Some(wallet), worker, job_id, field: proof_field, proof_b64 },
        Dialect::Kryptex | Dialect::KryptexV2 => {
            login = format!("{wallet}.{worker}");
            SubmitParams { wallet: None, worker: &login, job_id, field: proof_field, proof_b64 }
        }
    };
    request_line(opts, id, "mining.submit", params)
}

/// Messages the client sends, as JSON values (each is one NDJSON line). Uses the dialect's
/// default frame options; see [`authorize_lines`] for exact bytes.
pub fn authorize_msg(dialect: Dialect, id: u64, wallet: &str, worker: &str, password: &str) -> Vec<Value> {
    let opts = FrameOpts::for_dialect(dialect);
    authorize_lines(dialect, id, wallet, worker, password, &opts)
        .map(|lines| lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// `mining.submit` as a JSON value. `proof_field` is learned per pool ("plain_proof" or "plain_proof_zst").
pub fn submit_msg(dialect: Dialect, id: u64, wallet: &str, worker: &str, job_id: &str, proof_field: &str, proof_b64: &str) -> Value {
    let opts = FrameOpts { jsonrpc: false, agent: AGENT };
    submit_line(dialect, id, wallet, worker, job_id, proof_field, proof_b64, &opts)
        .ok()
        .and_then(|l| serde_json::from_str(&l).ok())
        .unwrap_or(Value::Null)
}

/// Parse one server line. Returns `Ok(Some(job))` for a notify, `Ok(None)` for anything else.
pub fn parse_notify(line: &str) -> Result<Option<Job>, ProtoError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ProtoError::LineTooLong(line.len()));
    }
    let v: Value = serde_json::from_str(line)?;
    parse_notify_value(&v)
}

/// [`parse_notify`] on an already parsed message.
pub fn parse_notify_value(v: &Value) -> Result<Option<Job>, ProtoError> {
    if v.get("method").and_then(Value::as_str) != Some("mining.notify") {
        return Ok(None);
    }
    let p = v.get("params").ok_or(ProtoError::BadField("params"))?;
    let job_id = p.get("job_id").and_then(Value::as_str).ok_or(ProtoError::BadField("job_id"))?;
    if job_id.is_empty() || job_id.len() > 128 {
        return Err(ProtoError::BadField("job_id"));
    }
    let job_id = job_id.to_string();
    let hh = p.get("header").and_then(Value::as_str).ok_or(ProtoError::BadField("header"))?;
    let hb = hex::decode(hh).map_err(|_| ProtoError::BadField("header"))?;
    let header: [u8; 76] = hb.try_into().map_err(|_| ProtoError::BadField("header"))?;
    let th = p.get("target").and_then(Value::as_str).ok_or(ProtoError::BadField("target"))?;
    let th = th.strip_prefix("0x").unwrap_or(th);
    if th.is_empty() || th.len() > 64 {
        return Err(ProtoError::BadField("target"));
    }
    let tb = hex::decode(format!("{th:0>64}")).map_err(|_| ProtoError::BadField("target"))?;
    let target = U256::from_big_endian(&tb);
    let height = p.get("height").and_then(Value::as_u64);
    let diff = p
        .get("diff")
        .and_then(Value::as_u64)
        .or_else(|| job_id.rsplit('_').next().and_then(|s| s.parse().ok()));
    let cert_version = p
        .get("cert_version")
        .and_then(Value::as_u64)
        .map(|x| u32::try_from(x).unwrap_or(u32::MAX));
    Ok(Some(Job { job_id, header, target, height, diff, cert_version }))
}

/// Result of an id-matched reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Accepted,
    Rejected(String),
    Unrelated,
}

pub fn parse_reply(line: &str, expect_id: u64) -> Result<Reply, ProtoError> {
    let v: Value = serde_json::from_str(line)?;
    Ok(match reply_of(&v) {
        Some((id, outcome)) if id == expect_id => outcome,
        _ => Reply::Unrelated,
    })
}

/// If `v` is a reply (numeric `id`, no `method`), its id and outcome.
pub fn reply_of(v: &Value) -> Option<(u64, Reply)> {
    if v.get("method").is_some() {
        return None;
    }
    let id = v.get("id").and_then(Value::as_u64)?;
    let err = v.get("error");
    let err_is_null = matches!(err, None | Some(Value::Null));
    let ok = err_is_null && !matches!(v.get("result"), None | Some(Value::Null) | Some(Value::Bool(false)));
    if ok {
        Some((id, Reply::Accepted))
    } else {
        let reason = match err {
            Some(e) if !e.is_null() => error_text(e),
            _ => "result false".to_string(),
        };
        Some((id, Reply::Rejected(reason)))
    }
}

/// Human-readable text of a stratum error: `{"code":..,"message":..}`, `[code, "msg", ..]` or a string.
pub fn error_text(e: &Value) -> String {
    match e {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string()),
        Value::Array(a) => a
            .iter()
            .find_map(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string()),
        other => other.to_string(),
    }
}

/// The `type` member of an authorize ack (LuckyPool `"plain"`, Kryptex v2 `"v2"`).
pub fn reply_type(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

/// Why a share was rejected, as far as the pool's free-form text tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RejectKind {
    /// Job unknown/expired on the pool side.
    Stale,
    /// Hash above the share target.
    LowDifficulty,
    Duplicate,
    /// The pool could not decode the proof member (wrong field, wrong compression, bad base64).
    Format,
    /// Decoded but failed verification.
    InvalidProof,
    Unauthorized,
    Banned,
    Other,
}

impl RejectKind {
    /// Rejects that say the proof itself (not timing) is wrong.
    pub fn is_invalid(self) -> bool {
        matches!(self, RejectKind::Format | RejectKind::InvalidProof | RejectKind::LowDifficulty)
    }
}

/// Classify a pool's reject text. Order matters: "invalid proof format" is a format reject.
pub fn classify_reject(reason: &str) -> RejectKind {
    let r = reason.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| r.contains(w));
    if has(&["banned", "blacklist", "blocked"]) {
        RejectKind::Banned
    } else if has(&["stale", "job not found", "unknown job", "old job", "expired", "outdated"]) {
        RejectKind::Stale
    } else if has(&["duplicate", "already submitted"]) {
        RejectKind::Duplicate
    } else if has(&["unauthori", "not authori", "unauthenticated", "not subscribed", "invalid worker", "login"]) {
        RejectKind::Unauthorized
    } else if has(&["low diff", "low difficulty", "above target", "high hash", "does not meet", "difficulty"]) {
        RejectKind::LowDifficulty
    } else if has(&[
        "format", "decode", "base64", "deserializ", "malformed", "missing", "parse", "zstd", "gzip", "bincode",
        "field",
    ]) {
        RejectKind::Format
    } else if has(&["invalid", "verif", "bad proof", "bad share", "incorrect"]) {
        RejectKind::InvalidProof
    } else {
        RejectKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_fixed_member_order() {
        let o = FrameOpts { jsonrpc: true, agent: "a/1" };
        let l = authorize_lines(Dialect::Object, 1, "W", "w", "x", &o).unwrap();
        assert_eq!(
            l,
            vec![r#"{"jsonrpc":"2.0","id":1,"method":"mining.authorize","params":{"wallet":"W","worker":"w","agent":"a/1"}}"#]
        );
        let o = FrameOpts { jsonrpc: false, agent: "a/1" };
        let l = authorize_lines(Dialect::Kryptex, 7, "W", "w", "x", &o).unwrap();
        assert_eq!(l[0], r#"{"id":7,"method":"mining.subscribe","params":["a/1"]}"#);
        assert_eq!(l[1], r#"{"id":8,"method":"mining.authorize","params":["W.w","x"]}"#);
        let l = authorize_lines(Dialect::KryptexV2, 1, "W", "w", "x", &o).unwrap();
        assert_eq!(l, vec![r#"{"id":1,"method":"mining.authorize","params":{"wallet":"W.w","agent":"a/1","type":"v2"}}"#]);
        let s = submit_line(Dialect::Object, 3, "W", "w", "j_1", "plain_proof_zst", "QUJD", &o).unwrap();
        assert_eq!(s, r#"{"id":3,"method":"mining.submit","params":{"wallet":"W","worker":"w","job_id":"j_1","plain_proof_zst":"QUJD"}}"#);
        let s = submit_line(Dialect::Kryptex, 3, "W", "w", "j_1", "plain_proof", "QUJD", &o).unwrap();
        assert_eq!(s, r#"{"id":3,"method":"mining.submit","params":{"worker":"W.w","job_id":"j_1","plain_proof":"QUJD"}}"#);
    }

    #[test]
    fn legacy_value_builders_still_work() {
        let v = submit_msg(Dialect::Object, 2, "W", "w", "j", "plain_proof", "AA==");
        assert_eq!(v["params"]["plain_proof"], "AA==");
        assert!(v.get("jsonrpc").is_none());
        let a = authorize_msg(Dialect::Kryptex, 1, "W", "w", "x");
        assert_eq!(a.len(), 2);
        assert_eq!(a[1]["params"][0], "W.w");
        assert_eq!(a[1]["jsonrpc"], "2.0");
    }

    #[test]
    fn hostile_notify_fields_are_rejected_not_panicking() {
        let h = "00".repeat(76);
        let long_target = format!(r#"{{"method":"mining.notify","params":{{"job_id":"a_1","header":"{h}","target":"{}"}}}}"#, "f".repeat(66));
        assert!(matches!(parse_notify(&long_target), Err(ProtoError::BadField("target"))));
        let short_header = r#"{"method":"mining.notify","params":{"job_id":"a_1","header":"00","target":"ff"}}"#;
        assert!(matches!(parse_notify(short_header), Err(ProtoError::BadField("header"))));
        let huge_cert = format!(r#"{{"method":"mining.notify","params":{{"job_id":"a_1","header":"{h}","target":"ff","cert_version":4294967299}}}}"#);
        let j = parse_notify(&huge_cert).unwrap().unwrap();
        assert_eq!(j.cert_version, Some(u32::MAX));
        assert!(j.requires_update());
        let no_cert = format!(r#"{{"method":"mining.notify","params":{{"job_id":"a_1","header":"{h}","target":"0x00ff"}}}}"#);
        let j = parse_notify(&no_cert).unwrap().unwrap();
        assert_eq!(j.target, U256::from(0xffu64));
        assert!(j.requires_update(), "a missing cert_version is unknown, never assumed");
        assert_eq!(j.diff, Some(1));
    }

    #[test]
    fn replies_and_error_shapes() {
        let v: Value = serde_json::from_str(r#"{"id":4,"result":null,"error":{"code":-1,"message":"Low difficulty share"}}"#).unwrap();
        assert_eq!(reply_of(&v), Some((4, Reply::Rejected("Low difficulty share".into()))));
        let v: Value = serde_json::from_str(r#"{"id":5,"result":null,"error":[21,"Job not found",null]}"#).unwrap();
        assert_eq!(reply_of(&v), Some((5, Reply::Rejected("Job not found".into()))));
        let v: Value = serde_json::from_str(r#"{"id":6,"result":{"status":"OK"},"error":null}"#).unwrap();
        assert_eq!(reply_of(&v), Some((6, Reply::Accepted)));
        let v: Value = serde_json::from_str(r#"{"id":null,"method":"mining.notify","params":{}}"#).unwrap();
        assert_eq!(reply_of(&v), None);
        let v: Value = serde_json::from_str(r#"{"error":null,"id":1,"result":true,"type":"plain"}"#).unwrap();
        assert_eq!(reply_type(&v), Some("plain"));
    }

    #[test]
    fn reject_texts_classify() {
        assert_eq!(classify_reject("Job not found"), RejectKind::Stale);
        assert_eq!(classify_reject("Stale share"), RejectKind::Stale);
        assert_eq!(classify_reject("Low difficulty share"), RejectKind::LowDifficulty);
        assert_eq!(classify_reject("Duplicate share"), RejectKind::Duplicate);
        assert_eq!(classify_reject("invalid proof format"), RejectKind::Format);
        assert_eq!(classify_reject("missing plain_proof"), RejectKind::Format);
        assert_eq!(classify_reject("failed to decode base64"), RejectKind::Format);
        assert_eq!(classify_reject("Invalid share"), RejectKind::InvalidProof);
        assert_eq!(classify_reject("proof verification failed"), RejectKind::InvalidProof);
        assert_eq!(classify_reject("Unauthorized worker"), RejectKind::Unauthorized);
        assert_eq!(classify_reject("IP banned for 3600 s"), RejectKind::Banned);
        assert_eq!(classify_reject("something else"), RejectKind::Other);
    }
}
