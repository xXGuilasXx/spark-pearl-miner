//! `state.json`: what the daemon learned and must remember across restarts. No fee constant is
//! stored here (the fee part is `spm_fee::PersistedFeeState`, which carries none either).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use spm_fee::PersistedFeeState;
use spm_proto::ProofField;

use crate::paths::write_atomic;

pub const STATE_VERSION: u32 = 1;
/// A learned "auto → plain TCP" result is re-probed after this long.
pub const TLS_AUTO_TTL_S: u64 = 7 * 24 * 3600;

/// What TLS `auto` found for an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsAutoResult {
    /// `tls` | `plain`
    pub transport: String,
    pub at_unix: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    pub version: u32,
    /// Developer-fee debt scheduler state.
    pub fee: Option<PersistedFeeState>,
    /// Working proof field per pool endpoint (`host:port` → `plain_proof` | `plain_proof_zst`).
    pub proof_fields: BTreeMap<String, String>,
    /// TLS auto results per endpoint.
    pub tls_auto: BTreeMap<String, TlsAutoResult>,
    /// The daemon was running when it last wrote this file (unclean-shutdown detection).
    pub running: bool,
    /// The user pressed Start (and not Stop): mining resumes when the daemon restarts.
    pub mining_wanted: bool,
}

impl PersistedState {
    pub fn proof_field(&self, endpoint: &str) -> Option<ProofField> {
        self.proof_fields.get(endpoint).and_then(|k| ProofField::from_key(k))
    }

    /// `Some(true)` = TLS worked, `Some(false)` = plain TCP; `None` when unknown or stale.
    pub fn tls_auto(&self, endpoint: &str, now_unix: u64) -> Option<bool> {
        let r = self.tls_auto.get(endpoint)?;
        if now_unix.saturating_sub(r.at_unix) > TLS_AUTO_TTL_S {
            return None;
        }
        Some(r.transport == "tls")
    }
}

#[derive(Debug)]
pub struct StateStore {
    file: PathBuf,
    pub state: PersistedState,
}

impl StateStore {
    /// Load `state.json`; a missing or unreadable file starts fresh (the old one is kept as
    /// `state.json.corrupt` for inspection).
    pub fn load(file: PathBuf) -> StateStore {
        let state = match fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str::<PersistedState>(&text) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "state.json is unreadable; starting with a fresh state");
                    let _ = fs::copy(&file, file.with_extension("json.corrupt"));
                    PersistedState::default()
                }
            },
            Err(_) => PersistedState::default(),
        };
        StateStore { file, state }
    }

    pub fn save(&mut self) -> io::Result<()> {
        self.state.version = STATE_VERSION;
        let text = serde_json::to_string_pretty(&self.state).map_err(io::Error::other)?;
        write_atomic(&self.file, text.as_bytes(), 0o600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_ttl() {
        let dir = std::env::temp_dir().join(format!("spm-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = StateStore::load(dir.join("state.json"));
        s.state.proof_fields.insert("br.pearl.herominers.com:1200".into(), "plain_proof_zst".into());
        s.state.tls_auto.insert("h:1".into(), TlsAutoResult { transport: "plain".into(), at_unix: 1000 });
        s.state.fee = Some(spm_fee::FeeScheduler::new(7, "prl1x").persisted());
        s.save().unwrap();
        let back = StateStore::load(dir.join("state.json"));
        assert_eq!(back.state, s.state);
        assert_eq!(back.state.proof_field("br.pearl.herominers.com:1200"), Some(ProofField::PlainProofZst));
        assert_eq!(back.state.tls_auto("h:1", 1000 + 60), Some(false));
        assert_eq!(back.state.tls_auto("h:1", 1000 + TLS_AUTO_TTL_S + 1), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
