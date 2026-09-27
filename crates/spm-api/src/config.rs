//! The user configuration: `~/.config/spark-pearl-miner/config.toml`, `schema_version = 1`.
//!
//! This module owns the schema, the defaults and the validation. File I/O (atomic write, `.bak`,
//! hot reload, audit) lives in the daemon; the API uses the same types for `GET`/`PUT
//! /api/v1/config`, so a config accepted by one is accepted by the other.
//!
//! There is deliberately no fee setting anywhere in this schema: every struct denies unknown
//! fields, and [`find_fee_keys`] rejects any key that even looks fee-related with a specific error.
//! The developer fee is compiled into `spm-fee` and is read-only everywhere.

use std::net::IpAddr;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use spm_proto::tls::{TlsMode, LUCKYPOOL_SPKI_SHA256_B64};
use spm_proto::{Dialect, ProofField};

/// Current `schema_version`.
pub const SCHEMA_VERSION: u32 = 1;
/// "até 3 endereços de pool".
pub const MAX_POOLS: usize = 3;
/// Default API port (loopback only).
pub const DEFAULT_API_PORT: u16 = 4078;
/// Default worker name.
pub const DEFAULT_WORKER: &str = "spark";

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub miner: MinerConfig,
    #[serde(default = "default_pools")]
    pub pools: Vec<PoolEntry>,
    #[serde(default)]
    pub failover: FailoverSettings,
    #[serde(default)]
    pub power: PowerConfig,
    #[serde(default)]
    pub coexistence: CoexistenceConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub gui: GuiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: SCHEMA_VERSION,
            miner: MinerConfig::default(),
            pools: default_pools(),
            failover: FailoverSettings::default(),
            power: PowerConfig::default(),
            coexistence: CoexistenceConfig::default(),
            worker: WorkerConfig::default(),
            api: ApiConfig::default(),
            gui: GuiConfig::default(),
        }
    }
}

/// Payout identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinerConfig {
    /// The user's Pearl address (bech32m, `prl1p…`). Empty until the setup wizard runs.
    #[serde(default)]
    pub wallet: String,
    /// Worker name shown by the pools: `[A-Za-z0-9_-]{1,32}`.
    #[serde(default = "default_worker")]
    pub worker: String,
    /// The user read the disclosure screen (developer fee, pools, power) and accepted it.
    /// Mining does not start before this is true.
    #[serde(default)]
    pub disclosure_accepted: bool,
}

impl Default for MinerConfig {
    fn default() -> Self {
        MinerConfig { wallet: String::new(), worker: default_worker(), disclosure_accepted: false }
    }
}

fn default_worker() -> String {
    DEFAULT_WORKER.to_string()
}

/// Transport security of one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsSetting {
    /// TLS first; plain TCP only when the server does not speak TLS (never after a certificate error).
    #[default]
    Auto,
    On,
    Off,
    /// TLS checked against `spki_pin` only (self-signed pools such as LuckyPool).
    Pinned,
}

/// Wire dialect of one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialectSetting {
    /// Chosen from the host name (HeroMiners/LuckyPool: object, Kryptex: kryptex; else object).
    #[default]
    Auto,
    Object,
    Kryptex,
    KryptexV2,
}

/// `"jsonrpc":"2.0"` member on requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JsonRpcSetting {
    #[default]
    Auto,
    On,
    Off,
}

/// Proof encoding on submit ("encoding" in the GUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProofSetting {
    /// Learned per pool (switches after repeated format rejects; persisted in state.json).
    #[default]
    Auto,
    /// `plain_proof` (base64 of bincode; gzip on Kryptex v2 sessions).
    Plain,
    /// `plain_proof_zst` (base64 of zstd).
    Zstd,
}

/// Hash-tile pattern (the official 2×64 fallback arrives with M15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PatternSetting {
    #[default]
    Auto,
    Official,
}

/// One user pool slot. The index in `pools` is the priority (pool 1 first).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolEntry {
    /// Free label shown in the GUI (a preset name or "custom").
    #[serde(default)]
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: TlsSetting,
    /// Base64 SHA-256 of the server's SubjectPublicKeyInfo, for `tls = "pinned"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spki_pin: String,
    #[serde(default)]
    pub dialect: DialectSetting,
    #[serde(default)]
    pub jsonrpc: JsonRpcSetting,
    #[serde(default)]
    pub proof: ProofSetting,
    /// Stratum password (`x`; Kryptex also takes `d=<N>`).
    #[serde(default = "default_password")]
    pub password: String,
    #[serde(default)]
    pub pattern: PatternSetting,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_password() -> String {
    "x".to_string()
}

fn yes() -> bool {
    true
}

impl PoolEntry {
    /// A plain entry with every advanced setting on auto.
    pub fn new(name: &str, host: &str, port: u16, tls: TlsSetting) -> Self {
        PoolEntry {
            name: name.to_string(),
            host: host.to_string(),
            port,
            tls,
            spki_pin: String::new(),
            dialect: DialectSetting::Auto,
            jsonrpc: JsonRpcSetting::Auto,
            proof: ProofSetting::Auto,
            password: default_password(),
            pattern: PatternSetting::Auto,
            enabled: true,
        }
    }

    fn host_lc(&self) -> String {
        self.host.trim().to_ascii_lowercase()
    }

    /// The dialect actually spoken (`auto` resolved from the host name).
    pub fn resolved_dialect(&self) -> Dialect {
        match self.dialect {
            DialectSetting::Object => Dialect::Object,
            DialectSetting::Kryptex => Dialect::Kryptex,
            DialectSetting::KryptexV2 => Dialect::KryptexV2,
            DialectSetting::Auto => dialect_for_host(&self.host_lc()),
        }
    }

    /// `Some(true/false)` to force the `jsonrpc` member, `None` for the dialect default.
    pub fn resolved_jsonrpc(&self) -> Option<bool> {
        match self.jsonrpc {
            JsonRpcSetting::On => Some(true),
            JsonRpcSetting::Off => Some(false),
            JsonRpcSetting::Auto => self.host_lc().ends_with("luckypool.io").then_some(true),
        }
    }

    /// Transport mode for a connection that the failover manager wants over TLS (`tls = true`)
    /// or plain TCP (`tls = false`). `auto` is resolved by the manager, so here it means "TLS".
    pub fn transport_mode(&self, tls: bool) -> TlsMode {
        if !tls {
            return TlsMode::Off;
        }
        match self.tls {
            TlsSetting::Pinned => TlsMode::Pinned { spki_sha256_b64: self.spki_pin.trim().to_string() },
            _ => TlsMode::On,
        }
    }

    /// The mode used by a one-shot connection test (`auto` keeps its fallback semantics).
    pub fn test_mode(&self) -> TlsMode {
        match self.tls {
            TlsSetting::Auto => TlsMode::Auto,
            TlsSetting::On => TlsMode::On,
            TlsSetting::Off => TlsMode::Off,
            TlsSetting::Pinned => TlsMode::Pinned { spki_sha256_b64: self.spki_pin.trim().to_string() },
        }
    }

    /// First proof field to use on a new session (`learned` comes from state.json).
    pub fn initial_proof_field(&self, learned: Option<ProofField>) -> ProofField {
        match self.proof {
            ProofSetting::Plain => ProofField::PlainProof,
            ProofSetting::Zstd => ProofField::PlainProofZst,
            ProofSetting::Auto => learned.unwrap_or(ProofField::PlainProof),
        }
    }

    /// `host:port`, the key of everything learned per pool.
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host_lc(), self.port)
    }
}

/// Dialect a pool speaks, by host name.
pub fn dialect_for_host(host: &str) -> Dialect {
    let h = host.to_ascii_lowercase();
    if h.ends_with("kryptex.network") {
        Dialect::Kryptex
    } else {
        Dialect::Object
    }
}

/// The three default slots: HeroMiners BR → LuckyPool BR → Kryptex.
pub fn default_pools() -> Vec<PoolEntry> {
    let hero = PoolEntry::new("HeroMiners BR", "br.pearl.herominers.com", 1200, TlsSetting::Auto);
    let mut lucky = PoolEntry::new("LuckyPool BR", "pearl-br.luckypool.io", 3360, TlsSetting::Pinned);
    lucky.spki_pin = LUCKYPOOL_SPKI_SHA256_B64.to_string();
    lucky.dialect = DialectSetting::Object;
    lucky.jsonrpc = JsonRpcSetting::On;
    let mut kryptex = PoolEntry::new("Kryptex", "prl-br.kryptex.network", 8048, TlsSetting::On);
    kryptex.dialect = DialectSetting::Kryptex;
    vec![hero, lucky, kryptex]
}

/// Failover thresholds (defaults = `docs/en/ARCHITECTURE.md`, "Failover").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverSettings {
    pub connect_timeout_s: u64,
    pub handshake_timeout_s: u64,
    pub first_job_timeout_s: u64,
    pub stall_soft_reconnect_s: u64,
    pub max_consecutive_invalid: u32,
    pub reject_ratio_max: f64,
    pub reject_window: usize,
    pub stale_ratio_max: f64,
    pub stale_window: usize,
    pub submit_ack_timeout_s: u64,
    pub max_ack_timeouts: u32,
    pub backoff_s: Vec<u64>,
    pub backoff_jitter_pct: u64,
    pub failback_probe_every_s: u64,
    pub failback_stable_s: u64,
    pub auth_retry_s: u64,
    pub quarantine_s: u64,
    pub drain_s: u64,
    pub reconnect_same_after_s: u64,
}

impl Default for FailoverSettings {
    fn default() -> Self {
        FailoverSettings {
            connect_timeout_s: 10,
            handshake_timeout_s: 15,
            first_job_timeout_s: 30,
            stall_soft_reconnect_s: 900,
            max_consecutive_invalid: 5,
            reject_ratio_max: 0.5,
            reject_window: 20,
            stale_ratio_max: 0.02,
            stale_window: 100,
            submit_ack_timeout_s: 30,
            max_ack_timeouts: 3,
            backoff_s: vec![5, 10, 20, 40, 80, 120],
            backoff_jitter_pct: 20,
            failback_probe_every_s: 300,
            failback_stable_s: 60,
            auth_retry_s: 600,
            quarantine_s: 600,
            drain_s: 5,
            reconnect_same_after_s: 60,
        }
    }
}

/// Power profile (enforced by the power governor, M11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerProfile {
    /// 60 W target, 70 W stop.
    Eco,
    /// 75 W target, 85 W stop.
    #[default]
    Balanced,
    /// 88 W target, 92 W stop: inside the band where a DGX Spark can power off.
    Max,
}

impl PowerProfile {
    /// Config/API spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PowerProfile::Eco => "eco",
            PowerProfile::Balanced => "balanced",
            PowerProfile::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PowerConfig {
    pub profile: PowerProfile,
    /// Max runs the GPU at its limits; the GUI asks the user to type an acknowledgement first.
    /// Without it `max` is refused, by the validation and again by the daemon.
    pub max_acknowledged: bool,
}

/// How the miner shares the GPU with other workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoexistenceMode {
    /// DGX Spark with spark-modo: the worker runs only as the `miner` runtime; the daemon never
    /// spawns it and only reports.
    SparkModo,
    /// Pause the worker (CUDA context kept) while the LLM server is busy.
    Yield,
    /// Release the worker (the process exits, its context is freed) while the LLM server is busy.
    YieldRelease,
    /// The GPU is the miner's.
    #[default]
    Exclusive,
}

impl CoexistenceMode {
    /// Config/API spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CoexistenceMode::SparkModo => "spark-modo",
            CoexistenceMode::Yield => "yield",
            CoexistenceMode::YieldRelease => "yield-release",
            CoexistenceMode::Exclusive => "exclusive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CoexistenceConfig {
    pub mode: CoexistenceMode,
    /// vLLM Prometheus endpoint polled in `yield` and `yield-release` (plain `http://` only).
    pub metrics_url: String,
    /// Metrics poll period, 100–250 ms.
    pub poll_ms: u64,
    /// Continuous idle time of the LLM server before mining starts or resumes, 1–600 s.
    pub idle_s: u64,
    /// Fallback when the metrics are unavailable: another compute process at or above this SM
    /// utilization counts as busy, 1–100 %.
    pub busy_sm_pct: u32,
}

impl Default for CoexistenceConfig {
    fn default() -> Self {
        CoexistenceConfig {
            mode: CoexistenceMode::default(),
            metrics_url: spm_coexist::DEFAULT_METRICS_URL.to_string(),
            poll_ms: spm_coexist::DEFAULT_POLL.as_millis() as u64,
            idle_s: spm_coexist::DEFAULT_IDLE.as_secs(),
            busy_sm_pct: spm_coexist::DEFAULT_BUSY_SM_PCT,
        }
    }
}

/// Who starts the GPU worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchMode {
    /// The daemon spawns `spark-pearl-miner gpu-worker --attach <worker.sock>` itself.
    #[default]
    Spawn,
    /// Something else starts the worker (the `spark-miner.service` system unit under spark-modo).
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkerConfig {
    pub launch: LaunchMode,
    /// Spawn the CPU simulation worker (`gpu-worker --sim`) instead of the CUDA worker. It finds
    /// shares only at trivial difficulty (the mock pool); meant for testing the pipeline.
    pub simulate: bool,
    /// Pause between two simulated attempts.
    pub sim_interval_ms: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig { launch: LaunchMode::Spawn, simulate: false, sim_interval_ms: 1000 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApiConfig {
    /// Loopback address to bind (127.0.0.1 or ::1).
    pub bind: String,
    pub port: u16,
    /// LAN access (opt-in, TLS only). Not implemented in this build: `true` is rejected.
    pub lan: bool,
    /// Connections from this machine by the same user account need no token (the peer's UID is
    /// checked); everything else keeps the token. Set false to require the token everywhere.
    pub trust_local_user: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig { bind: "127.0.0.1".to_string(), port: DEFAULT_API_PORT, lan: false, trust_local_user: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GuiConfig {
    /// `auto`, `en` or `pt-BR`.
    pub language: String,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { language: "auto".to_string() }
    }
}

/// One validation problem, addressed by a dotted path (`pools[1].host`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub path: String,
    /// Stable machine code, translated by the GUI.
    pub code: String,
    pub message: String,
}

impl FieldError {
    fn new(path: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        FieldError { path: path.into(), code: code.to_string(), message: message.into() }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("fee settings are not configurable (found {0:?}); the developer fee is compiled into the binary and shown read-only")]
    FeeKey(Vec<String>),
    #[error("could not parse the configuration: {0}")]
    Parse(String),
    #[error("invalid configuration: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Invalid(Vec<FieldError>),
}

impl ConfigError {
    /// The field errors, for the GUI.
    pub fn fields(&self) -> Vec<FieldError> {
        match self {
            ConfigError::FeeKey(keys) => keys
                .iter()
                .map(|k| FieldError::new(k.clone(), "fee_not_configurable", "the developer fee cannot be configured"))
                .collect(),
            ConfigError::Parse(m) => vec![FieldError::new("", "parse", m.clone())],
            ConfigError::Invalid(v) => v.clone(),
        }
    }
}

/// Does a key name look like a fee setting? (`fee`, `dev_fee`, `devfee`, `dev_wallet`, `donation`…)
pub fn is_fee_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("fee") || k.starts_with("dev") || k.contains("donat")
}

/// Every fee-looking key anywhere in a JSON document, as dotted paths.
pub fn find_fee_keys(v: &serde_json::Value) -> Vec<String> {
    fn walk(v: &serde_json::Value, path: &str, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    if is_fee_key(k) {
                        out.push(p.clone());
                    }
                    walk(child, &p, out);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "", &mut out);
    out
}

/// A worker name the pools accept: `[A-Za-z0-9_-]{1,32}`.
pub fn is_valid_worker(w: &str) -> bool {
    (1..=32).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Host name or IP literal.
pub fn is_valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    if h.parse::<IpAddr>().is_ok() {
        return true;
    }
    h.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// A base64 SHA-256 (32 bytes).
pub fn is_valid_pin(p: &str) -> bool {
    STANDARD.decode(p.trim()).map(|v| v.len() == 32).unwrap_or(false)
}

fn is_valid_password(p: &str) -> bool {
    p.len() <= 64 && p.bytes().all(|b| b.is_ascii_graphic())
}

/// How strict [`Config::validate`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// A file on disk: an empty wallet (setup not done yet) is allowed.
    File,
    /// A change submitted through the API or CLI: the wallet is required.
    Submit,
}

fn range<T: PartialOrd + Copy + std::fmt::Display>(errs: &mut Vec<FieldError>, path: &str, v: T, lo: T, hi: T) {
    if v < lo || v > hi {
        errs.push(FieldError::new(path, "out_of_range", format!("must be between {lo} and {hi} (got {v})")));
    }
}

impl Config {
    /// Parse a TOML document (fee keys and unknown keys are errors).
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let value: toml::Value = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let json = serde_json::to_value(&value).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let fee = find_fee_keys(&json);
        if !fee.is_empty() {
            return Err(ConfigError::FeeKey(fee));
        }
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Parse a JSON document from the API (fee keys and unknown keys are errors).
    pub fn from_json_value(v: serde_json::Value) -> Result<Config, ConfigError> {
        let fee = find_fee_keys(&v);
        if !fee.is_empty() {
            return Err(ConfigError::FeeKey(fee));
        }
        serde_json::from_value(v).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Serialize to TOML with a short header.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!(
            "# spark-pearl-miner configuration (schema_version {SCHEMA_VERSION}).\n\
             # Edited by the GUI (http://127.0.0.1:4078) or by hand; changes are picked up while the\n\
             # daemon runs. The developer fee is not configurable: see FEE.md.\n\n{body}"
        )
    }

    /// Check every field. Returns all problems at once.
    pub fn validate(&self, strictness: Strictness) -> Result<(), ConfigError> {
        let mut e = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            e.push(FieldError::new(
                "schema_version",
                "schema_version",
                format!("this build reads schema_version {SCHEMA_VERSION} (got {})", self.schema_version),
            ));
        }
        let w = self.miner.wallet.trim();
        if w.is_empty() {
            if strictness == Strictness::Submit {
                e.push(FieldError::new("miner.wallet", "wallet_required", "a Pearl wallet address is required"));
            }
        } else if w != self.miner.wallet || w != w.to_ascii_lowercase() || !spm_fee::is_valid_prl_p2tr(w) {
            e.push(FieldError::new(
                "miner.wallet",
                "wallet_invalid",
                "not a Pearl mainnet address (bech32m, prl1p…, witness v1, 32 bytes)",
            ));
        }
        if !is_valid_worker(&self.miner.worker) {
            e.push(FieldError::new("miner.worker", "worker_invalid", "1–32 characters: letters, digits, _ or -"));
        }
        if self.pools.is_empty() {
            e.push(FieldError::new("pools", "pools_empty", "configure at least one pool"));
        }
        if self.pools.len() > MAX_POOLS {
            e.push(FieldError::new(
                "pools",
                "too_many_pools",
                format!("at most {MAX_POOLS} pools (got {})", self.pools.len()),
            ));
        }
        for (i, p) in self.pools.iter().enumerate() {
            let at = |f: &str| format!("pools[{i}].{f}");
            if !is_valid_host(p.host.trim()) || p.host.trim() != p.host {
                e.push(FieldError::new(at("host"), "host_invalid", "not a valid host name or IP address"));
            }
            if p.port == 0 {
                e.push(FieldError::new(at("port"), "port_invalid", "port must be 1–65535"));
            }
            if p.tls == TlsSetting::Pinned && !is_valid_pin(&p.spki_pin) {
                e.push(FieldError::new(at("spki_pin"), "pin_invalid", "a pinned pool needs the base64 SHA-256 of its public key"));
            }
            if !p.spki_pin.is_empty() && p.tls != TlsSetting::Pinned {
                e.push(FieldError::new(at("spki_pin"), "pin_unused", "spki_pin is only used with tls = \"pinned\""));
            }
            if !is_valid_password(&p.password) {
                e.push(FieldError::new(at("password"), "password_invalid", "up to 64 printable characters without spaces"));
            }
            if p.name.chars().count() > 40 || p.name.chars().any(char::is_control) {
                e.push(FieldError::new(at("name"), "name_invalid", "up to 40 characters"));
            }
        }
        let f = &self.failover;
        range(&mut e, "failover.connect_timeout_s", f.connect_timeout_s, 1, 120);
        range(&mut e, "failover.handshake_timeout_s", f.handshake_timeout_s, 1, 120);
        range(&mut e, "failover.first_job_timeout_s", f.first_job_timeout_s, 1, 600);
        range(&mut e, "failover.stall_soft_reconnect_s", f.stall_soft_reconnect_s, 10, 7200);
        range(&mut e, "failover.max_consecutive_invalid", f.max_consecutive_invalid, 1, 1000);
        range(&mut e, "failover.reject_ratio_max", f.reject_ratio_max, 0.0, 1.0);
        range(&mut e, "failover.reject_window", f.reject_window, 1, 1000);
        range(&mut e, "failover.stale_ratio_max", f.stale_ratio_max, 0.0, 1.0);
        range(&mut e, "failover.stale_window", f.stale_window, 1, 10_000);
        range(&mut e, "failover.submit_ack_timeout_s", f.submit_ack_timeout_s, 1, 600);
        range(&mut e, "failover.max_ack_timeouts", f.max_ack_timeouts, 1, 100);
        range(&mut e, "failover.backoff_jitter_pct", f.backoff_jitter_pct, 0, 100);
        range(&mut e, "failover.failback_probe_every_s", f.failback_probe_every_s, 1, 86_400);
        range(&mut e, "failover.failback_stable_s", f.failback_stable_s, 1, 86_400);
        range(&mut e, "failover.auth_retry_s", f.auth_retry_s, 1, 86_400);
        range(&mut e, "failover.quarantine_s", f.quarantine_s, 1, 86_400);
        range(&mut e, "failover.drain_s", f.drain_s, 0, 120);
        range(&mut e, "failover.reconnect_same_after_s", f.reconnect_same_after_s, 0, 86_400);
        if f.backoff_s.is_empty() || f.backoff_s.len() > 16 || f.backoff_s.iter().any(|&s| s == 0 || s > 3600) {
            e.push(FieldError::new("failover.backoff_s", "out_of_range", "1–16 steps, each 1–3600 s"));
        }
        if self.power.profile == PowerProfile::Max && !self.power.max_acknowledged {
            e.push(FieldError::new(
                "power.max_acknowledged",
                "max_not_acknowledged",
                "the Max profile needs the typed acknowledgement",
            ));
        }
        let c = &self.coexistence;
        if spm_coexist::http::HttpUrl::parse(&c.metrics_url).is_err() {
            e.push(FieldError::new(
                "coexistence.metrics_url",
                "metrics_url_invalid",
                "a plain http:// URL, e.g. http://127.0.0.1:8001/metrics",
            ));
        }
        range(&mut e, "coexistence.poll_ms", c.poll_ms, 100, 250);
        range(&mut e, "coexistence.idle_s", c.idle_s, 1, 600);
        range(&mut e, "coexistence.busy_sm_pct", c.busy_sm_pct, 1, 100);
        range(&mut e, "worker.sim_interval_ms", self.worker.sim_interval_ms, 50, 60_000);
        match self.api.bind.parse::<IpAddr>() {
            Ok(ip) if ip.is_loopback() => {}
            Ok(_) => e.push(FieldError::new(
                "api.bind",
                "bind_not_loopback",
                "the API only listens on a loopback address (use ssh -L 4078:127.0.0.1:4078 for remote access)",
            )),
            Err(_) => e.push(FieldError::new("api.bind", "bind_invalid", "not an IP address")),
        }
        if self.api.port == 0 {
            e.push(FieldError::new("api.port", "port_invalid", "port must be 1–65535"));
        }
        if self.api.lan {
            e.push(FieldError::new(
                "api.lan",
                "lan_requires_tls",
                "LAN access requires TLS, which this build does not implement yet; use ssh -L 4078:127.0.0.1:4078",
            ));
        }
        if !["auto", "en", "pt-BR"].contains(&self.gui.language.as_str()) {
            e.push(FieldError::new("gui.language", "language_invalid", "auto, en or pt-BR"));
        }
        if e.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(e))
        }
    }

    /// True when mining can start: wallet set and the disclosure accepted.
    pub fn setup_complete(&self) -> bool {
        !self.miner.wallet.is_empty() && self.miner.disclosure_accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALLET: &str = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh";

    fn valid() -> Config {
        let mut c = Config::default();
        c.miner.wallet = WALLET.to_string();
        c.miner.disclosure_accepted = true;
        c
    }

    #[test]
    fn defaults_are_valid_and_round_trip_through_toml() {
        Config::default().validate(Strictness::File).unwrap();
        let c = valid();
        c.validate(Strictness::Submit).unwrap();
        let text = c.to_toml();
        assert!(text.starts_with("# spark-pearl-miner configuration"));
        assert!(text.contains("schema_version = 1"));
        assert_eq!(Config::from_toml(&text).unwrap(), c);
    }

    #[test]
    fn default_slots_match_the_architecture() {
        let p = default_pools();
        assert_eq!((p[0].host.as_str(), p[0].port, p[0].tls, p[0].dialect), ("br.pearl.herominers.com", 1200, TlsSetting::Auto, DialectSetting::Auto));
        assert_eq!((p[1].host.as_str(), p[1].port, p[1].tls, p[1].dialect, p[1].jsonrpc), ("pearl-br.luckypool.io", 3360, TlsSetting::Pinned, DialectSetting::Object, JsonRpcSetting::On));
        assert_eq!(p[1].spki_pin, "d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=");
        assert_eq!((p[2].host.as_str(), p[2].port, p[2].tls, p[2].dialect), ("prl-br.kryptex.network", 8048, TlsSetting::On, DialectSetting::Kryptex));
        assert_eq!(p[0].resolved_dialect(), Dialect::Object);
        assert_eq!(p[1].resolved_jsonrpc(), Some(true));
        assert_eq!(p[2].resolved_dialect(), Dialect::Kryptex);
        assert_eq!(p[1].transport_mode(true), TlsMode::luckypool());
        assert_eq!(p[0].transport_mode(false), TlsMode::Off);
    }

    #[test]
    fn the_documented_default_file_is_the_real_one() {
        let toml = Config::default().to_toml();
        for doc in [include_str!("../../../docs/en/CONFIGURATION.md"), include_str!("../../../docs/pt-BR/CONFIGURACAO.md")] {
            for line in toml.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
                assert!(doc.contains(line), "the configuration docs do not show `{line}`; default file:\n{toml}");
            }
        }
    }

    #[test]
    fn power_and_coexistence_defaults_and_ranges() {
        let c = Config::default();
        assert_eq!((c.power.profile, c.power.max_acknowledged), (PowerProfile::Balanced, false));
        assert_eq!(c.coexistence.mode, CoexistenceMode::Exclusive);
        assert_eq!(c.coexistence.metrics_url, "http://127.0.0.1:8001/metrics");
        assert_eq!((c.coexistence.poll_ms, c.coexistence.idle_s, c.coexistence.busy_sm_pct), (200, 5, 10));
        let mut bad = valid();
        bad.coexistence.metrics_url = "https://127.0.0.1:8001/metrics".into();
        bad.coexistence.poll_ms = 50;
        bad.coexistence.idle_s = 0;
        bad.coexistence.busy_sm_pct = 101;
        bad.power.profile = PowerProfile::Max;
        let fields: Vec<String> = bad.validate(Strictness::File).unwrap_err().fields().into_iter().map(|f| f.path).collect();
        for p in ["coexistence.metrics_url", "coexistence.poll_ms", "coexistence.idle_s", "coexistence.busy_sm_pct", "power.max_acknowledged"] {
            assert!(fields.iter().any(|f| f == p), "{p} not refused: {fields:?}");
        }
        // Old files with only `mode` still load.
        let c = Config::from_toml("schema_version = 1\n[coexistence]\nmode = \"yield\"\n").unwrap();
        assert_eq!((c.coexistence.mode, c.coexistence.idle_s), (CoexistenceMode::Yield, 5));
    }

    #[test]
    fn a_minimal_file_gets_defaults() {
        let c = Config::from_toml("schema_version = 1\n[miner]\nwallet = \"\"\n").unwrap();
        assert_eq!(c.pools.len(), 3);
        assert_eq!(c.api.port, DEFAULT_API_PORT);
        assert_eq!(c.miner.worker, DEFAULT_WORKER);
    }

    #[test]
    fn trust_local_user_defaults_on_and_can_be_turned_off() {
        assert!(Config::default().api.trust_local_user);
        // Files written before the key existed keep working and get the default.
        let c = Config::from_toml("schema_version = 1\n[api]\nbind = \"127.0.0.1\"\nport = 4078\nlan = false\n").unwrap();
        assert!(c.api.trust_local_user);
        let c = Config::from_toml("schema_version = 1\n[api]\ntrust_local_user = false\n").unwrap();
        assert!(!c.api.trust_local_user);
        c.validate(Strictness::File).unwrap();
    }
}
