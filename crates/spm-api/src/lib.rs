//! spm-api — the local HTTP API and embedded web GUI of spark-pearl-miner.
//!
//! * [`config`]: the TOML schema (`schema_version = 1`), defaults and validation, shared with the
//!   daemon. It has no fee setting at all; fee-looking keys are refused explicitly.
//! * [`security`]: token file (0600) → HttpOnly SameSite=Strict cookie + CSRF value; Host/Origin
//!   allowlist; the same user account on this machine may skip the token (`api.trust_local_user`).
//! * [`peer`]: the UID behind a loopback connection, from `/proc/net/tcp` and `/proc/net/tcp6`.
//! * [`server`]: axum router on `127.0.0.1:4078` — REST, SSE and the static GUI from `webui/`.
//! * [`pooltest`]: the "Test connection" button (DNS + TCP + TLS; authorize only when confirmed).
//!
//! The daemon plugs in through [`Backend`]; tests use a fake one.
#![forbid(unsafe_code)]

pub mod config;
pub mod peer;
pub mod pooltest;
pub mod security;
pub mod server;
pub mod views;

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use config::Config;
pub use views::*;

/// Where a configuration change came from (audited).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    /// `PUT /api/v1/config` (the GUI).
    Api,
    /// The file was edited on disk (hot reload).
    File,
    /// The command line.
    Cli,
}

impl ChangeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeSource::Api => "api",
            ChangeSource::File => "file",
            ChangeSource::Cli => "cli",
        }
    }
}

/// What applying a configuration did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub applied: bool,
    pub wallet_changed: bool,
    /// Settings that only take effect after a daemon restart (e.g. the API port).
    pub restart_required: Vec<String>,
}

/// Mining and pool controls (`POST /api/v1/mining/*`, `/api/v1/pools/{i}/*`, control socket).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    Start,
    Stop,
    Pause,
    Resume,
    /// Switch to slot (0-based) now; also pins it.
    Switch { slot: u8 },
    /// Pin a slot (0-based) or unpin with `None`.
    Pin { slot: Option<u8> },
    /// Dismiss the "wallet changed" notice.
    AckWallet,
}

/// The daemon side of the API. Reads are snapshots; writes are forwarded to the daemon's loop.
pub trait Backend: Send + Sync + 'static {
    fn status(&self) -> StatusView;
    fn pools(&self) -> PoolsView;
    fn fee(&self) -> FeeView;
    fn gpu(&self) -> GpuView;
    fn about(&self) -> AboutView;
    fn config(&self) -> Config;
    /// Log lines with `seq > since`, at most `limit`, oldest first.
    fn logs(&self, since: u64, limit: usize) -> Vec<LogEntry>;
    fn subscribe(&self) -> broadcast::Receiver<ApiEvent>;
    /// Apply an already validated configuration.
    fn apply_config(&self, cfg: Config, source: ChangeSource) -> impl Future<Output = Result<ApplyOutcome, String>> + Send;
    fn control(&self, op: ControlOp) -> impl Future<Output = Result<String, String>> + Send;
}
