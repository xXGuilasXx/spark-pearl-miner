//! "Test connection": DNS, TCP and TLS only, unless the user explicitly confirms an authorize test
//! (`confirm = true`), which logs in once with the user's wallet and waits for the first job.
//! Nothing is ever submitted.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use spm_proto::client::{DisconnectReason, PoolSession, SessionConfig, SessionEvent};
use spm_proto::tls::{ConnectError, Connector, TlsMode, Transport};

use crate::config::{is_valid_host, is_valid_worker, PoolEntry};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolTestRequest {
    pub pool: PoolEntry,
    /// Also log in (authorize) and wait for a job. Off unless the user confirms.
    #[serde(default)]
    pub confirm: bool,
    /// Defaults to the configured wallet/worker.
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TestStep {
    /// `dns` | `tcp` | `tls` | `authorize` | `job`
    pub step: String,
    pub ok: bool,
    pub ms: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolTestResult {
    pub ok: bool,
    pub steps: Vec<TestStep>,
    /// Stable code of the first failure (translated by the GUI).
    pub error_code: Option<String>,
    /// `tls` | `plain`
    pub transport: Option<String>,
    pub spki_sha256_b64: Option<String>,
    pub authorize_tested: bool,
}

impl PoolTestResult {
    fn fail(mut self, code: &str) -> Self {
        self.ok = false;
        self.error_code = Some(code.to_string());
        self
    }
}

fn step(name: &str, ok: bool, t0: Instant, detail: impl Into<String>) -> TestStep {
    TestStep { step: name.to_string(), ok, ms: t0.elapsed().as_millis() as u64, detail: detail.into() }
}

/// Error code for a connect failure, as the GUI translates it.
pub fn connect_error_code(e: &ConnectError) -> &'static str {
    match e {
        ConnectError::Tcp { source, .. } if source.kind() == std::io::ErrorKind::ConnectionRefused => "connect_refused",
        ConnectError::Tcp { .. } => "connect_failed",
        ConnectError::Timeout { .. } => "connect_timeout",
        ConnectError::Certificate { .. } => "tls_certificate",
        ConnectError::PinMismatch { .. } => "tls_pin_mismatch",
        ConnectError::TlsProtocol { .. } => "tls_protocol",
        ConnectError::ServerName(_) => "host_invalid",
        ConnectError::Config(_) => "tls_config",
    }
}

/// Run the test. `wallet`/`worker` are only used when `confirm` is set.
pub async fn run(req: PoolTestRequest, default_wallet: &str, default_worker: &str) -> PoolTestResult {
    let p = &req.pool;
    let mut r = PoolTestResult {
        ok: true,
        steps: Vec::new(),
        error_code: None,
        transport: None,
        spki_sha256_b64: None,
        authorize_tested: false,
    };
    let host = p.host.trim().to_string();
    if !is_valid_host(&host) || p.port == 0 {
        r.steps.push(TestStep { step: "dns".into(), ok: false, ms: 0, detail: "invalid host or port".into() });
        return r.fail("host_invalid");
    }

    // DNS
    let t0 = Instant::now();
    match tokio::time::timeout(STEP_TIMEOUT, tokio::net::lookup_host((host.as_str(), p.port))).await {
        Ok(Ok(addrs)) => {
            let list: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            if list.is_empty() {
                r.steps.push(step("dns", false, t0, "no address"));
                return r.fail("dns_failed");
            }
            r.steps.push(step("dns", true, t0, list.join(", ")));
        }
        Ok(Err(e)) => {
            r.steps.push(step("dns", false, t0, e.to_string()));
            return r.fail("dns_failed");
        }
        Err(_) => {
            r.steps.push(step("dns", false, t0, "timed out"));
            return r.fail("dns_timeout");
        }
    }

    // TCP
    let t0 = Instant::now();
    match tokio::time::timeout(STEP_TIMEOUT, tokio::net::TcpStream::connect((host.as_str(), p.port))).await {
        Ok(Ok(s)) => {
            drop(s);
            r.steps.push(step("tcp", true, t0, "connected"));
        }
        Ok(Err(e)) => {
            let code = if e.kind() == std::io::ErrorKind::ConnectionRefused { "connect_refused" } else { "connect_failed" };
            r.steps.push(step("tcp", false, t0, e.to_string()));
            return r.fail(code);
        }
        Err(_) => {
            r.steps.push(step("tcp", false, t0, "timed out"));
            return r.fail("connect_timeout");
        }
    }

    // TLS (or the auto fallback)
    let mode = p.test_mode();
    let connector = Connector::default().with_timeouts(STEP_TIMEOUT, STEP_TIMEOUT);
    let t0 = Instant::now();
    let session_mode = match &mode {
        TlsMode::Off => {
            r.transport = Some("plain".into());
            TlsMode::Off
        }
        _ => match connector.connect(&host, p.port, &mode).await {
            Ok(c) => {
                let transport = if c.transport == Transport::Tls { "tls" } else { "plain" };
                r.transport = Some(transport.into());
                r.spki_sha256_b64 = c.peer_spki_sha256_b64.clone();
                let detail = match (&c.transport, c.plain_fallback) {
                    (Transport::Tls, _) => format!("TLS ok, key SHA-256 {}", c.peer_spki_sha256_b64.clone().unwrap_or_default()),
                    (Transport::Plain, true) => "the server does not speak TLS; auto uses plain TCP".to_string(),
                    (Transport::Plain, false) => "plain TCP".to_string(),
                };
                r.steps.push(step("tls", true, t0, detail));
                if c.transport == Transport::Tls { mode.clone() } else { TlsMode::Off }
            }
            Err(e) => {
                r.steps.push(step("tls", false, t0, e.to_string()));
                return r.fail(connect_error_code(&e));
            }
        },
    };

    if !req.confirm {
        return r;
    }

    // Authorize (explicitly confirmed by the user)
    r.authorize_tested = true;
    let wallet = req.wallet.clone().unwrap_or_else(|| default_wallet.to_string());
    let worker = req.worker.clone().unwrap_or_else(|| default_worker.to_string());
    if !spm_fee::is_valid_prl_p2tr(&wallet) || !is_valid_worker(&worker) {
        r.steps.push(TestStep { step: "authorize".into(), ok: false, ms: 0, detail: "set a valid wallet and worker first".into() });
        return r.fail("wallet_invalid");
    }
    let mut cfg = SessionConfig::new(host.clone(), p.port, p.resolved_dialect(), wallet, worker);
    cfg.tls = session_mode;
    cfg.jsonrpc = p.resolved_jsonrpc();
    cfg.password = p.password.clone();
    let (session, mut ev) = PoolSession::spawn(cfg, connector);
    let t0 = Instant::now();
    let deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
    let mut authorized = false;
    let mut outcome: Option<&'static str> = None;
    loop {
        let e = match tokio::time::timeout_at(deadline, ev.recv()).await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => {
                let (name, code) = if authorized { ("job", "no_job") } else { ("authorize", "auth_timeout") };
                r.steps.push(step(name, false, t0, "timed out"));
                outcome = Some(code);
                break;
            }
        };
        match e {
            SessionEvent::Authorized { proof_type } => {
                authorized = true;
                r.steps.push(step("authorize", true, t0, proof_type.map(|t| format!("accepted (type {t})")).unwrap_or_else(|| "accepted".into())));
            }
            SessionEvent::AuthRejected { reason } => {
                r.steps.push(step("authorize", false, t0, reason));
                outcome = Some("auth_rejected");
                break;
            }
            SessionEvent::JobReceived(job) => {
                if !authorized {
                    r.steps.push(step("authorize", true, t0, "implicit (job before the ack)"));
                }
                let cv = job.cert_version.map_or_else(|| "missing".to_string(), |v| v.to_string());
                let ok = !job.requires_update();
                r.steps.push(step("job", ok, t0, format!("job {} (cert_version {cv})", job.job_id)));
                if !ok {
                    outcome = Some("update_required");
                }
                break;
            }
            SessionEvent::Disconnected { reason } => {
                let code = match reason {
                    DisconnectReason::AuthRejected(_) => "auth_rejected",
                    DisconnectReason::Eof => "eof",
                    _ => "connect_failed",
                };
                r.steps.push(step(if authorized { "job" } else { "authorize" }, false, t0, format!("{reason:?}")));
                outcome = Some(code);
                break;
            }
            _ => {}
        }
    }
    session.shutdown().await;
    match outcome {
        Some(code) => r.fail(code),
        None => r,
    }
}
