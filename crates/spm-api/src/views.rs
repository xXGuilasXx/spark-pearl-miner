//! What the API returns. The daemon fills these; the GUI and `spark-pearl-miner status` read them.
//! Pool-provided strings (errors, job ids) are data: the GUI renders them as text only.

use serde::{Deserialize, Serialize};
use spm_fee::{FeeConstants, FeeStats};

/// `GET /api/v1/status` (also the 1 Hz `stats` SSE event).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusView {
    pub version: String,
    /// `setup_required` | `stopped` | `starting` | `mining` | `failing_over` | `all_down` | `paused`
    pub state: String,
    /// The failover manager state in English, e.g. `mining on pool 2`.
    pub manager: String,
    /// `starting` | `mining` | `failing_over` | `reconnecting` | `all_down` | `paused`
    pub manager_code: String,
    /// 1-based slots of a failover (`from` → `to`) or the mining slot (`to`).
    pub manager_from: Option<u8>,
    pub manager_to: Option<u8>,
    /// 1-based slot the GPU mines for (user pools only).
    pub active_pool: Option<u8>,
    /// `user` | `dev` | `idle`: what the worker hashes right now.
    pub mining_target: String,
    /// The user asked for mining (Start pressed and not Stopped).
    pub running: bool,
    pub paused: bool,
    pub pause_reason: Option<String>,
    /// Credited MAC/s over the last 10 s, in tera (T-MAC/s ≈ TH/s in pool units).
    pub hashrate_tmacs: f64,
    /// Credited MACs since the daemon started (user + dev).
    pub credited_macs_total: f64,
    pub shares: ShareCounts,
    pub uptime_s: u64,
    pub mining_s: u64,
    pub worker: WorkerView,
    /// Fee phase (`waiting`, `pre_warm`, `slice`, `suspended`, `disabled`).
    pub fee_phase: String,
    pub alerts: Vec<AlertView>,
    pub wallet: String,
    pub worker_name: String,
    /// Set when the payout wallet changed; cleared by `POST /api/v1/wallet/ack`.
    pub wallet_changed: Option<WalletChange>,
    pub setup_required: bool,
    pub spark_modo_present: bool,
    /// Unix time (ms) of this snapshot.
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareCounts {
    pub accepted: u64,
    pub rejected: u64,
    /// Rejected by the pool as stale, or dropped locally because the job had changed.
    pub stale: u64,
    /// Hits dropped before submitting (stale job, dead session, local verify failure).
    pub discarded: u64,
    pub dev_accepted: u64,
    pub dev_rejected: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerView {
    /// `absent` | `starting` | `ready` | `hashing` | `paused` | `backoff` | `waiting_external` | `faulted`
    pub state: String,
    pub device: Option<String>,
    pub simulated: bool,
    /// `spawn` | `external`
    pub launch: String,
    pub pid: Option<u32>,
    pub failures_10min: u32,
    pub last_fault: Option<String>,
    pub next_retry_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertView {
    pub at_ms: u64,
    /// `warn` | `error`
    pub level: String,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletChange {
    pub at_ms: u64,
    /// `api` | `file` | `cli`
    pub source: String,
    /// The previous address, abbreviated.
    pub previous: String,
}

/// `GET /api/v1/pools`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolsView {
    pub manager: String,
    pub manager_code: String,
    pub manager_from: Option<u8>,
    pub manager_to: Option<u8>,
    /// 1-based pinned slot.
    pub pinned: Option<u8>,
    /// 1-based slot being probed for failback / manual switch.
    pub probing: Option<u8>,
    pub slots: Vec<SlotView>,
    /// Failover timeline, newest last.
    pub timeline: Vec<TimelineEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlotView {
    /// 1-based priority.
    pub index: u8,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// Configured TLS mode.
    pub tls: String,
    /// What `auto` found for this endpoint (`tls` | `plain`), if known.
    pub tls_learned: Option<String>,
    pub enabled: bool,
    /// Slot state code: `disabled` | `idle` | `resolving` | `connecting` | `tls_handshake` |
    /// `authorizing` | `awaiting_job` | `active` | `standby` | `draining` | `backoff` |
    /// `config_error` | `quarantined`
    pub state: String,
    /// Seconds until a backoff/quarantine/retry ends, when relevant.
    pub retry_in_s: Option<u64>,
    pub probe: bool,
    pub failures: u32,
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub current_job: Option<String>,
    pub last_error: Option<ErrorView>,
    /// Learned proof field (`plain_proof` | `plain_proof_zst`).
    pub proof_field: Option<String>,
}

/// A problem in plain language: the GUI translates `code`; `detail` is the raw text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorView {
    pub code: String,
    pub detail: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineEntry {
    pub at_ms: u64,
    /// `manager` | `slot` | `switch` | `alert` | `log`
    pub kind: String,
    pub slot: Option<u8>,
    pub msg: String,
}

/// `GET /api/v1/fee`: read-only constants plus what was measured.
#[derive(Debug, Clone, Serialize)]
pub struct FeeView {
    pub banner: String,
    pub constants: FeeConstants,
    pub constants_hash: String,
    /// `None` until the daemon has loaded its state.
    pub stats: Option<FeeStats>,
    /// The dev session, while one is open: `host:port` and its state.
    pub dev_session: Option<String>,
    /// The fee is off because the user's wallet is the fee wallet.
    pub disabled_for_wallet: bool,
}

impl FeeView {
    /// Constants only.
    pub fn constants_only() -> Self {
        FeeView {
            banner: spm_fee::banner(),
            constants: spm_fee::fee_constants(),
            constants_hash: spm_fee::constants_hash(),
            stats: None,
            dev_session: None,
            disabled_for_wallet: false,
        }
    }
}

/// `GET /api/v1/gpu`. The daemon never opens a CUDA context: this comes from the worker's frames
/// and from `nvidia-smi --query-gpu` (NVML).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuView {
    pub worker_device: Option<String>,
    pub simulated: bool,
    pub sm_clock_mhz: Option<u32>,
    pub power_w: Option<f32>,
    pub smi: Option<SmiView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmiView {
    pub name: String,
    pub temperature_c: Option<f32>,
    pub power_w: Option<f32>,
    pub sm_clock_mhz: Option<u32>,
    pub max_sm_clock_mhz: Option<u32>,
    pub utilization_pct: Option<f32>,
    pub at_ms: u64,
}

/// `GET /api/v1/about`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AboutView {
    pub version: String,
    pub commit: String,
    /// SHA-256 of the running binary (`/proc/self/exe`).
    pub binary_sha256: String,
    pub fee_constants_hash: String,
    pub license: String,
    pub repository: String,
}

/// One log line (`GET /api/v1/logs`, `log` SSE events).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub seq: u64,
    pub at_ms: u64,
    pub level: String,
    pub target: String,
    pub msg: String,
}

/// Pushed on `GET /api/v1/events` (the SSE event name is `type`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    Share {
        at_ms: u64,
        /// `user` | `dev`
        target: String,
        pool: Option<u8>,
        accepted: bool,
        reason: Option<String>,
        job_id: String,
    },
    Fsm {
        at_ms: u64,
        /// `manager` | `slot`
        scope: String,
        slot: Option<u8>,
        from: String,
        to: String,
    },
    Fee {
        at_ms: u64,
        action: String,
        detail: String,
    },
    Alert(AlertView),
    Log(LogEntry),
    Timeline(TimelineEntry),
    Config {
        at_ms: u64,
        source: String,
        wallet_changed: bool,
    },
}

impl ApiEvent {
    /// SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            ApiEvent::Share { .. } => "share",
            ApiEvent::Fsm { .. } => "fsm",
            ApiEvent::Fee { .. } => "fee",
            ApiEvent::Alert(_) => "alert",
            ApiEvent::Log(_) => "log",
            ApiEvent::Timeline(_) => "timeline",
            ApiEvent::Config { .. } => "config",
        }
    }
}

/// Replace every Pearl address in `text` by `prl1…<last 4>` (diagnostics export, shared logs).
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("prl1") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let len = tail.bytes().take_while(|b| b.is_ascii_alphanumeric()).count();
        if len >= 14 {
            out.push_str("prl1…");
            out.push_str(&tail[len - 4..len]);
        } else {
            out.push_str(&tail[..len]);
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// `prl1pxtu…eydh` style abbreviation for the GUI.
pub fn abbreviate_wallet(w: &str) -> String {
    if w.len() <= 16 || !w.is_ascii() {
        return w.to_string();
    }
    format!("{}…{}", &w[..8], &w[w.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_addresses_only() {
        let w = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh";
        let r = redact(&format!("login {w}.rig0 at pool; prl1 short"));
        assert_eq!(r, "login prl1…eydh.rig0 at pool; prl1 short");
        assert_eq!(abbreviate_wallet(w), "prl1pxtu…eydh");
    }
}
