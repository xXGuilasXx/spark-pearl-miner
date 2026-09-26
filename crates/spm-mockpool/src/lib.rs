//! spm-mockpool — a test-only Pearl pool on 127.0.0.1 (plain TCP).
//!
//! It speaks the dialects we captured live, detected per connection from the authorize:
//! object (HeroMiners/LuckyPool: `params` object), kryptex (silent `mining.subscribe`, then
//! `params: ["wallet.worker", "x"]`) and kryptex v2 (object with `"type":"v2"`, gzip proofs).
//! Jobs are built from a fixed header at a trivial difficulty, and every submitted proof is
//! checked with the official verifier exactly as pools do:
//! `check_cert_version_eligible` + `verify_plain_proof(.., Some(nbits_share), SeedDerivation::Salted)`.
//!
//! Fault knobs ([`Faults`]) can be changed at runtime with [`MockPool::set_faults`]: refuse
//! connections, blackhole, auth reject, no job, reject storm, mute submits, stall, EOF
//! mid-submit and an oversized (> 4 MiB) line.
#![forbid(unsafe_code)]

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use primitive_types::U256;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use spm_pow::{check_cert_version_eligible, compact_from_target, target_from_compact, IncompleteBlockHeader, PlainProof, SeedDerivation};
use spm_proto::codec::{write_line, NdjsonReader};
use spm_proto::encode::{decode_proof, sniff_encoding, ProofEncoding};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use zk_pow::api::verify::verify_plain_proof;

/// Header nbits and share nbits of the trivial preset: bound ≈ 2^231 · h·w·k, so a CPU finds a
/// share on the first attempt at m = n = 256.
pub const TRIVIAL_NBITS: u32 = 0x1d7f_ffff;
/// Jobs kept for late submits; older job ids are answered "Job not found".
const JOB_HISTORY: usize = 8;
/// Size of the oversized line (just over the client's 4 MiB cap).
pub const OVERSIZED_LINE_BYTES: usize = 4 * 1024 * 1024 + 64;

/// Runtime fault injection. All default to off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Faults {
    /// Close the listening socket: new connections get ECONNREFUSED.
    pub refuse_connections: bool,
    /// Accept and read, but never write anything.
    pub blackhole: bool,
    /// Answer every authorize with an error.
    pub auth_reject: bool,
    /// Authorize, but never send a job.
    pub no_job: bool,
    /// Reject every submit without verifying it.
    pub reject_storm: bool,
    /// Never answer submits (the client sees ack timeouts).
    pub mute_submits: bool,
    /// After the first job of a connection, send no job for this long.
    pub stall: Option<Duration>,
    /// Close the connection as soon as a submit arrives, without answering.
    pub eof_mid_submit: bool,
    /// Right after the authorize ack, send one line longer than 4 MiB.
    pub oversized_line: bool,
}

#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Header of the first job; later jobs bump the timestamp.
    pub header: [u8; 76],
    /// Share target sent in every notify (big-endian hex).
    pub share_target: U256,
    /// `cert_version` sent in every notify (`None` omits the field).
    pub cert_version: Option<u32>,
    /// Send a fresh job this often (jobs are also created on demand with [`MockPool::new_job`]).
    pub job_interval: Option<Duration>,
    /// `type` member of the authorize ack (LuckyPool sends `"plain"`).
    pub ack_type: Option<String>,
    /// Proof fields this pool accepts; the others get a format reject.
    pub accepted_fields: Vec<String>,
    pub faults: Faults,
}

impl MockConfig {
    /// Header nbits and share target both at [`TRIVIAL_NBITS`].
    pub fn trivial() -> Self {
        MockConfig {
            header: trivial_header(),
            share_target: target_from_compact(TRIVIAL_NBITS),
            cert_version: Some(3),
            job_interval: None,
            ack_type: None,
            accepted_fields: vec!["plain_proof".into(), "plain_proof_zst".into()],
            faults: Faults::default(),
        }
    }
}

/// A fixed, obviously synthetic header: version 0x20000000, prev 0x11.., merkle 0x22.., nbits trivial.
pub fn trivial_header() -> [u8; 76] {
    let mut h = [0u8; 76];
    h[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    h[4..36].fill(0x11);
    h[36..68].fill(0x22);
    h[68..72].copy_from_slice(&0x6666_6666u32.to_le_bytes());
    h[72..76].copy_from_slice(&TRIVIAL_NBITS.to_le_bytes());
    h
}

/// What the mock saw. Watch it with [`MockPool::wait_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MockStats {
    pub connections: u64,
    pub subscribes: u64,
    pub authorizes: u64,
    pub jobs_sent: u64,
    pub submits: u64,
    pub verified_ok: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub last_reject: Option<String>,
    pub last_submit_field: Option<String>,
    pub last_submit_job: Option<String>,
    /// Raw authorize line of the last connection (to check handshake bytes).
    pub last_authorize: Option<String>,
}

#[derive(Debug, Clone)]
struct MockJob {
    job_id: String,
    header: [u8; 76],
}

struct Board {
    counter: u32,
    jobs: VecDeque<MockJob>,
}

struct Shared {
    cfg: MockConfig,
    nbits_share: u32,
    diff: u64,
    faults: watch::Sender<Faults>,
    stats: watch::Sender<MockStats>,
    board: Mutex<Board>,
    job_tx: broadcast::Sender<MockJob>,
    seen: Mutex<HashSet<[u8; 32]>>,
}

impl Shared {
    fn faults(&self) -> Faults {
        self.faults.borrow().clone()
    }

    fn stat(&self, f: impl FnOnce(&mut MockStats)) {
        self.stats.send_modify(f);
    }

    fn current_job(&self) -> Option<MockJob> {
        self.board.lock().ok().and_then(|b| b.jobs.back().cloned())
    }

    fn find_job(&self, job_id: &str) -> Option<MockJob> {
        self.board.lock().ok().and_then(|b| b.jobs.iter().find(|j| j.job_id == job_id).cloned())
    }

    fn make_job(&self) -> Option<MockJob> {
        let mut b = self.board.lock().ok()?;
        let n = b.counter;
        b.counter = b.counter.wrapping_add(1);
        let mut header = self.cfg.header;
        let ts = u32::from_le_bytes([header[68], header[69], header[70], header[71]]).wrapping_add(n);
        header[68..72].copy_from_slice(&ts.to_le_bytes());
        let job = MockJob { job_id: format!("{n:08x}_{}", self.diff), header };
        b.jobs.push_back(job.clone());
        while b.jobs.len() > JOB_HISTORY {
            b.jobs.pop_front();
        }
        Some(job)
    }

    fn notify_line(&self, j: &MockJob) -> String {
        let mut target = [0u8; 32];
        self.cfg.share_target.to_big_endian(&mut target);
        let mut params = json!({
            "job_id": j.job_id,
            "header": hex::encode(j.header),
            "target": hex::encode(target),
            "height": 119_365,
        });
        if let (Some(cv), Some(o)) = (self.cfg.cert_version, params.as_object_mut()) {
            o.insert("cert_version".into(), json!(cv));
        }
        json!({"id": null, "method": "mining.notify", "params": params}).to_string()
    }
}

/// A running mock pool. Dropping it stops the listener and every connection.
pub struct MockPool {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for MockPool {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockPool {
    pub async fn start(cfg: MockConfig) -> std::io::Result<MockPool> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let nbits_share = compact_from_target(cfg.share_target);
        let pdiff = (U256::from(0xFFFFu64) << 208) / cfg.share_target.max(U256::one());
        let diff = if pdiff > U256::from(u64::MAX) { u64::MAX } else { pdiff.low_u64().max(1) };
        // Refusing from the start: close before anyone can land in the backlog.
        let listener = (!cfg.faults.refuse_connections).then_some(listener);
        let (faults, faults_rx) = watch::channel(cfg.faults.clone());
        let (job_tx, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            cfg,
            nbits_share,
            diff,
            faults,
            stats: watch::channel(MockStats::default()).0,
            board: Mutex::new(Board { counter: 0, jobs: VecDeque::new() }),
            job_tx,
            seen: Mutex::new(HashSet::new()),
        });
        shared.make_job();
        let task = tokio::spawn(accept_loop(addr, listener, shared.clone(), faults_rx));
        Ok(MockPool { addr, shared, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Compact nbits the mock verifies shares against.
    pub fn nbits_share(&self) -> u32 {
        self.shared.nbits_share
    }

    pub fn set_faults(&self, f: Faults) {
        self.shared.faults.send_replace(f);
    }

    pub fn stats(&self) -> MockStats {
        self.shared.stats.borrow().clone()
    }

    /// Wait until `pred` holds for the stats, or time out.
    pub async fn wait_stats(&self, pred: impl FnMut(&MockStats) -> bool, timeout: Duration) -> Option<MockStats> {
        let mut rx = self.shared.stats.subscribe();
        let r = tokio::time::timeout(timeout, rx.wait_for(pred)).await.ok()?.ok()?;
        Some(r.clone())
    }

    pub fn current_job_id(&self) -> Option<String> {
        self.shared.current_job().map(|j| j.job_id)
    }

    /// Create a new job and push it to every authorized connection. Returns its id.
    pub fn new_job(&self) -> Option<String> {
        let j = self.shared.make_job()?;
        let _ = self.shared.job_tx.send(j.clone());
        Some(j.job_id)
    }
}

async fn accept_on(l: Option<&TcpListener>) -> std::io::Result<(TcpStream, SocketAddr)> {
    match l {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

async fn accept_loop(addr: SocketAddr, mut listener: Option<TcpListener>, sh: Arc<Shared>, mut faults: watch::Receiver<Faults>) {
    let mut conns = JoinSet::new();
    if let Some(every) = sh.cfg.job_interval {
        let sh = sh.clone();
        conns.spawn(async move {
            let mut t = tokio::time::interval(every);
            t.tick().await;
            loop {
                t.tick().await;
                if let Some(j) = sh.make_job() {
                    let _ = sh.job_tx.send(j);
                }
            }
        });
    }
    loop {
        let refuse = faults.borrow_and_update().refuse_connections;
        if refuse {
            listener = None;
        } else if listener.is_none() {
            match TcpListener::bind(addr).await {
                Ok(l) => listener = Some(l),
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "mock pool could not re-bind; retrying");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        }
        tokio::select! {
            r = accept_on(listener.as_ref()) => {
                if let Ok((s, _)) = r {
                    let sh = sh.clone();
                    conns.spawn(async move { connection(s, sh).await });
                }
            }
            r = faults.changed() => if r.is_err() { return },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

struct Conn {
    sh: Arc<Shared>,
    w: OwnedWriteHalf,
    authorized: bool,
    gzip: bool,
    stall_until: Option<Instant>,
}

enum Flow {
    Continue,
    Close,
}

impl Conn {
    async fn send(&mut self, line: &str) -> bool {
        write_line(&mut self.w, line).await.is_ok()
    }

    async fn reply_ok(&mut self, id: &Value, extra: Option<(&str, &str)>) -> bool {
        let mut v = json!({"id": id, "error": null, "result": true});
        if let (Some((k, val)), Some(o)) = (extra, v.as_object_mut()) {
            o.insert(k.to_string(), json!(val));
        }
        self.send(&v.to_string()).await
    }

    async fn reply_err(&mut self, id: &Value, code: i64, msg: &str) -> bool {
        let v = json!({"id": id, "result": null, "error": {"code": code, "message": msg}});
        self.send(&v.to_string()).await
    }

    async fn send_job(&mut self, j: &MockJob) -> bool {
        let line = self.sh.notify_line(j);
        let ok = self.send(&line).await;
        if ok {
            self.sh.stat(|s| s.jobs_sent += 1);
        }
        ok
    }

    async fn reject(&mut self, id: &Value, code: i64, msg: &str) -> bool {
        let m = msg.to_string();
        self.sh.stat(|s| {
            s.rejected += 1;
            s.last_reject = Some(m);
        });
        self.reply_err(id, code, msg).await
    }

    async fn on_line(&mut self, line: &str) -> Flow {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            let ok = self.send(r#"{"id":null,"result":null,"error":{"code":-32700,"message":"Malformed JSON"}}"#).await;
            return if ok { Flow::Continue } else { Flow::Close };
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let ok = match v.get("method").and_then(Value::as_str) {
            Some("mining.subscribe") => {
                // Kryptex: silent.
                self.sh.stat(|s| s.subscribes += 1);
                true
            }
            Some("mining.authorize") => return self.on_authorize(&v, &id, line).await,
            Some("mining.submit") => return self.on_submit(&v, &id).await,
            _ => self.reply_err(&id, -32601, "Method not found").await,
        };
        if ok {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    async fn on_authorize(&mut self, v: &Value, id: &Value, raw: &str) -> Flow {
        let raw = raw.to_string();
        self.sh.stat(|s| {
            s.authorizes += 1;
            s.last_authorize = Some(raw);
        });
        let p = v.get("params");
        let (login_ok, wants_v2) = match p {
            Some(Value::Array(a)) => (a.first().and_then(Value::as_str).is_some_and(|l| l.contains('.') && l.len() > 2), false),
            Some(Value::Object(o)) => (
                o.get("wallet").and_then(Value::as_str).is_some_and(|w| !w.is_empty()),
                o.get("type").and_then(Value::as_str) == Some("v2"),
            ),
            _ => (false, false),
        };
        let faults = self.sh.faults();
        if faults.auth_reject || !login_ok {
            return if self.reply_err(id, 24, "Unauthorized worker").await { Flow::Continue } else { Flow::Close };
        }
        self.authorized = true;
        self.gzip = wants_v2;
        let ack_type = if wants_v2 { Some("v2".to_string()) } else { self.sh.cfg.ack_type.clone() };
        if !self.reply_ok(id, ack_type.as_deref().map(|t| ("type", t))).await {
            return Flow::Close;
        }
        if faults.oversized_line {
            let mut big = vec![b' '; OVERSIZED_LINE_BYTES];
            big[0] = b'{';
            big.extend_from_slice(b"}\n");
            if self.w.write_all(&big).await.is_err() {
                return Flow::Close;
            }
        }
        if !faults.no_job {
            if let Some(j) = self.sh.current_job() {
                if !self.send_job(&j).await {
                    return Flow::Close;
                }
                self.stall_until = faults.stall.map(|d| Instant::now() + d);
            }
        }
        Flow::Continue
    }

    async fn on_submit(&mut self, v: &Value, id: &Value) -> Flow {
        let p = v.get("params").cloned().unwrap_or(Value::Null);
        let job_id = p.get("job_id").and_then(Value::as_str).map(str::to_string);
        let field = ["plain_proof", "plain_proof_zst"].into_iter().find(|f| p.get(*f).is_some());
        let (jid, fld) = (job_id.clone(), field.map(str::to_string));
        self.sh.stat(|s| {
            s.submits += 1;
            s.last_submit_job = jid;
            s.last_submit_field = fld;
        });
        let faults = self.sh.faults();
        if faults.eof_mid_submit {
            return Flow::Close;
        }
        if faults.mute_submits {
            return Flow::Continue;
        }
        let ok = if !self.authorized {
            self.reject(id, 24, "Unauthorized worker").await
        } else if faults.reject_storm {
            self.reject(id, 20, "Invalid share").await
        } else {
            match self.check_submit(&p, job_id.as_deref(), field).await {
                Ok(()) => {
                    self.sh.stat(|s| {
                        s.verified_ok += 1;
                        s.accepted += 1;
                    });
                    self.reply_ok(id, None).await
                }
                Err((code, msg)) => self.reject(id, code, &msg).await,
            }
        };
        if ok {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    async fn check_submit(&self, p: &Value, job_id: Option<&str>, field: Option<&str>) -> Result<(), (i64, String)> {
        let job_id = job_id.ok_or((20, "missing job_id".to_string()))?;
        let job = self.sh.find_job(job_id).ok_or((21, "Job not found".to_string()))?;
        let field = field.ok_or((20, "missing plain_proof".to_string()))?;
        if !self.sh.cfg.accepted_fields.iter().any(|f| f == field) {
            return Err((20, format!("bad proof format: field {field} not supported")));
        }
        let b64 = p.get(field).and_then(Value::as_str).ok_or((20, "bad proof format: not a string".to_string()))?;
        let enc = if field == "plain_proof_zst" {
            ProofEncoding::Zstd
        } else if self.gzip {
            ProofEncoding::Gzip
        } else {
            ProofEncoding::Plain
        };
        let bytes = decode_proof(b64, enc).map_err(|e| (20, format!("failed to decode proof: {e}")))?;
        if enc == ProofEncoding::Plain && sniff_encoding(&bytes) != ProofEncoding::Plain {
            return Err((20, "bad proof format: compressed data in plain_proof".to_string()));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let proof = PlainProof::deserialize_compat(&bytes).map_err(|e| (20, format!("failed to deserialize proof: {e}")))?;
        let header = IncompleteBlockHeader::from_bytes(&job.header).map_err(|e| (20, format!("bad job header: {e}")))?;
        let nbits = self.sh.nbits_share;
        let cert_version = self.sh.cfg.cert_version.unwrap_or(3);
        let verdict = tokio::task::spawn_blocking(move || -> Result<(), String> {
            check_cert_version_eligible(cert_version, &proof).map_err(|e| e.to_string())?;
            verify_plain_proof(&header, &proof, Some(nbits), SeedDerivation::Salted).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| (20, format!("verifier panicked: {e}")))?;
        verdict.map_err(|e| (20, format!("Invalid proof: {e}")))?;
        let fresh = self.sh.seen.lock().map(|mut s| s.insert(digest)).unwrap_or(false);
        if !fresh {
            return Err((22, "Duplicate share".to_string()));
        }
        Ok(())
    }
}

async fn connection(s: TcpStream, sh: Arc<Shared>) {
    sh.stat(|st| st.connections += 1);
    let _ = s.set_nodelay(true);
    if sh.faults().blackhole {
        let mut s = s;
        let mut buf = vec![0u8; 64 * 1024];
        while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
        return;
    }
    let (r, w) = s.into_split();
    let mut reader = NdjsonReader::new(r);
    let mut jobs = sh.job_tx.subscribe();
    let mut c = Conn { sh: sh.clone(), w, authorized: false, gzip: false, stall_until: None };
    loop {
        tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(l)) => if let Flow::Close = c.on_line(&l).await { break },
                _ => break,
            },
            j = jobs.recv(), if c.authorized => match j {
                Ok(j) => {
                    let f = sh.faults();
                    let stalled = c.stall_until.is_some_and(|t| Instant::now() < t);
                    if !f.no_job && !f.blackhole && !stalled && !c.send_job(&j).await {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    let _ = c.w.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_preset_is_consistent() {
        let cfg = MockConfig::trivial();
        assert_eq!(compact_from_target(cfg.share_target), TRIVIAL_NBITS);
        let h = IncompleteBlockHeader::from_bytes(&cfg.header).unwrap();
        assert_eq!(h.nbits, TRIVIAL_NBITS);
        assert_eq!(h.version, 0x2000_0000);
    }

    #[tokio::test]
    async fn jobs_rotate_and_expire() {
        let m = MockPool::start(MockConfig::trivial()).await.unwrap();
        let first = m.current_job_id().unwrap();
        assert!(first.starts_with("00000000_"));
        let mut last = first.clone();
        for _ in 0..JOB_HISTORY {
            last = m.new_job().unwrap();
        }
        assert_ne!(first, last);
        assert!(m.shared.find_job(&first).is_none(), "oldest job expired");
        assert!(m.shared.find_job(&last).is_some());
        let a = m.shared.find_job(&last).unwrap().header;
        assert_ne!(a, m.shared.cfg.header, "later jobs change the timestamp");
    }
}
