//! spark-pearl-miner: the daemon, the simulated GPU worker and the command line.
//!
//! One binary (`spark-pearl-miner`, alias `spm`) in three roles; see `docs/en/ARCHITECTURE.md`.
//! Unsafe code is forbidden in this crate: the peer-credential check on the Unix sockets uses
//! tokio's safe `UnixStream::peer_cred` (`SO_PEERCRED`).
#![forbid(unsafe_code)]

pub mod arbiter;
pub mod configsvc;
pub mod control;
pub mod daemon;
pub mod feetest;
pub mod logring;
pub mod paths;
pub mod smi;
pub mod state;
pub mod supervisor;
pub mod worker_sim;

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Git commit recorded at build time (`unknown` outside a checkout).
pub const COMMIT: &str = env!("SPM_GIT_COMMIT");

/// The `version` text: version, commit, fee constants hash and the fee line.
pub fn version_text() -> String {
    format!(
        "spark-pearl-miner {VERSION} (commit {COMMIT})\nfee constants hash (BLAKE3): {}\n{}\n",
        spm_fee::constants_hash(),
        spm_fee::banner()
    )
}
