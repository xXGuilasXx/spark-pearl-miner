//! Developer fee — the ONLY place where the fee is defined. README.md, README.pt-BR.md and
//! docs/*/FEE.md must show exactly these values; CI fails otherwise.
//!
//! Mechanism (see docs/en/ARCHITECTURE.md and this crate's README.md): a time slice on a
//! separate, pre-connected pool session, never share-splitting (Pearl shares are bound to each
//! session's job header). The slices are planned by the deterministic [`FeeScheduler`].
//!
//! Every value below is a compile-time constant. There is deliberately no override path of any
//! kind (no CLI flag, environment variable, config key, API call or setter): the fee wallet, rate,
//! pools, worker and schedule can only change by editing this file and recompiling.
#![forbid(unsafe_code)]

pub mod scheduler;
pub use scheduler::{
    AbortReason, Activity, FeeAction, FeePhase, FeeScheduler, FeeStats, PersistedFeeState, WindowSegment,
};

/// Fee in basis points of *active mining time* (200 bps = 2.00 %).
pub const FEE_BPS: u32 = 200;
/// Self-custody Pearl mainnet P2TR address of the developer (bech32m, HRP `prl`, witness v1).
pub const DEV_WALLET: &str = "prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n";
/// Worker name used on the developer session — constant, so pool stats never encode user wallets.
pub const DEV_WORKER: &str = "devfee";
/// Developer pools in preference order (wallet-as-login pools only). Region for HeroMiners is
/// chosen by RTT at runtime among these; the last entries are cross-pool fallbacks.
pub const DEV_POOLS: &[(&str, u16, bool)] = &[
    ("br.pearl.herominers.com", 1200, true),
    ("us.pearl.herominers.com", 1200, true),
    ("de.pearl.herominers.com", 1200, true),
    ("pearl-br.luckypool.io", 3360, true),
    ("prl.kryptex.network", 8048, true),
];
/// Length of one developer slice, in seconds of active mining.
pub const SLICE_SECS: u64 = 120;
/// Debt accrues as `FEE_BPS / (10_000 - FEE_BPS)` seconds of dev time per second of user time,
/// only while the GPU is actually hashing for the user.
pub const DEBT_NUM: u64 = FEE_BPS as u64;
pub const DEBT_DEN: u64 = 10_000 - FEE_BPS as u64;
/// The first slice starts at a random point inside this window after mining starts.
pub const FIRST_SLICE_MIN_SECS: u64 = 30 * 60;
pub const FIRST_SLICE_MAX_SECS: u64 = 90 * 60;
/// Unpaid debt is persisted and capped so a dev-pool outage never turns into a burst later.
pub const DEBT_CAP_SECS: u64 = 3600;
/// Fee is suspended when more than this share of dev submits are rejected (fails in the user's favour).
pub const SUSPEND_REJECT_RATIO: f32 = 0.5;
/// SUSPEND_REJECT_RATIO is evaluated over this many most recent dev submits.
pub const SUSPEND_WINDOW_SHARES: usize = 20;
/// Length of a reject-ratio suspension: no slice runs and no debt accrues meanwhile.
pub const SUSPEND_SECS: u64 = 3600;
/// The dev session is opened and authorized this long before a slice starts (PreWarm).
pub const PREWARM_SECS: u64 = 10;
/// A dev session that has not authorized this long after PreWarm is dropped; the slice is aborted.
pub const DEV_AUTH_TIMEOUT_SECS: u64 = 60;
/// After an aborted slice (authorize failure or timeout, dev session lost) the next try waits this long.
pub const DEV_RETRY_SECS: u64 = 300;
/// Rolling window, in seconds of *active* mining time, over which dev time never exceeds FEE_BPS
/// while the outstanding debt is at most CATCHUP_DEBT_SECS.
pub const FEE_WINDOW_SECS: u64 = 24 * 3600;
/// A slice shortened by the rolling-window ceiling is not started when it would be shorter than this.
pub const MIN_SLICE_SECS: u64 = 30;
/// Debt above this is a backlog left by a dev-pool outage: it is repaid in full SLICE_SECS slices,
/// at most one per MIN_SLICE_GAP_SECS of user hashing, and never beyond DEBT_CAP_SECS in total.
pub const CATCHUP_DEBT_SECS: u64 = 2 * SLICE_SECS;
/// Minimum user hashing between two slices, so debt is never paid as a burst.
pub const MIN_SLICE_GAP_SECS: u64 = 30 * 60;

/// Read-only view of every fee constant (for `--version`, logs and `/api/v1/fee`).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct FeeConstants {
    pub fee_bps: u32,
    pub dev_wallet: &'static str,
    pub dev_worker: &'static str,
    pub dev_pools: &'static [(&'static str, u16, bool)],
    pub slice_secs: u64,
    pub debt_num: u64,
    pub debt_den: u64,
    pub first_slice_min_secs: u64,
    pub first_slice_max_secs: u64,
    pub debt_cap_secs: u64,
    pub suspend_reject_ratio: f32,
    pub suspend_window_shares: usize,
    pub suspend_secs: u64,
    pub prewarm_secs: u64,
    pub dev_auth_timeout_secs: u64,
    pub dev_retry_secs: u64,
    pub fee_window_secs: u64,
    pub min_slice_secs: u64,
    pub catchup_debt_secs: u64,
    pub min_slice_gap_secs: u64,
}

/// All fee constants, by value. There is no counterpart that sets them.
pub const fn fee_constants() -> FeeConstants {
    FeeConstants {
        fee_bps: FEE_BPS,
        dev_wallet: DEV_WALLET,
        dev_worker: DEV_WORKER,
        dev_pools: DEV_POOLS,
        slice_secs: SLICE_SECS,
        debt_num: DEBT_NUM,
        debt_den: DEBT_DEN,
        first_slice_min_secs: FIRST_SLICE_MIN_SECS,
        first_slice_max_secs: FIRST_SLICE_MAX_SECS,
        debt_cap_secs: DEBT_CAP_SECS,
        suspend_reject_ratio: SUSPEND_REJECT_RATIO,
        suspend_window_shares: SUSPEND_WINDOW_SHARES,
        suspend_secs: SUSPEND_SECS,
        prewarm_secs: PREWARM_SECS,
        dev_auth_timeout_secs: DEV_AUTH_TIMEOUT_SECS,
        dev_retry_secs: DEV_RETRY_SECS,
        fee_window_secs: FEE_WINDOW_SECS,
        min_slice_secs: MIN_SLICE_SECS,
        catchup_debt_secs: CATCHUP_DEBT_SECS,
        min_slice_gap_secs: MIN_SLICE_GAP_SECS,
    }
}

/// Canonical text form of every fee constant, one `NAME=value` per line (input of [`constants_hash`]).
pub fn canonical_constants() -> String {
    let c = fee_constants();
    let mut s = String::from("spm-fee constants v1\n");
    let mut line = |k: &str, v: String| {
        s.push_str(k);
        s.push('=');
        s.push_str(&v);
        s.push('\n');
    };
    line("FEE_BPS", c.fee_bps.to_string());
    line("DEV_WALLET", c.dev_wallet.to_string());
    line("DEV_WORKER", c.dev_worker.to_string());
    for (host, port, flag) in c.dev_pools {
        line("DEV_POOL", format!("{host}:{port}:{flag}"));
    }
    line("SLICE_SECS", c.slice_secs.to_string());
    line("DEBT_NUM", c.debt_num.to_string());
    line("DEBT_DEN", c.debt_den.to_string());
    line("FIRST_SLICE_MIN_SECS", c.first_slice_min_secs.to_string());
    line("FIRST_SLICE_MAX_SECS", c.first_slice_max_secs.to_string());
    line("DEBT_CAP_SECS", c.debt_cap_secs.to_string());
    line("SUSPEND_REJECT_RATIO", c.suspend_reject_ratio.to_string());
    line("SUSPEND_WINDOW_SHARES", c.suspend_window_shares.to_string());
    line("SUSPEND_SECS", c.suspend_secs.to_string());
    line("PREWARM_SECS", c.prewarm_secs.to_string());
    line("DEV_AUTH_TIMEOUT_SECS", c.dev_auth_timeout_secs.to_string());
    line("DEV_RETRY_SECS", c.dev_retry_secs.to_string());
    line("FEE_WINDOW_SECS", c.fee_window_secs.to_string());
    line("MIN_SLICE_SECS", c.min_slice_secs.to_string());
    line("CATCHUP_DEBT_SECS", c.catchup_debt_secs.to_string());
    line("MIN_SLICE_GAP_SECS", c.min_slice_gap_secs.to_string());
    s
}

/// BLAKE3 (hex) of [`canonical_constants`]; printed by `--version` so a build's fee terms can be
/// compared with the published release at a glance.
pub fn constants_hash() -> String {
    blake3::hash(canonical_constants().as_bytes()).to_hex().to_string()
}

/// The single line every UI surface prints.
pub fn banner() -> String {
    format!(
        "dev fee {}.{:02}% -> {} @ {}:{} ({}), worker \"{}\", {} s slices, only while mining",
        FEE_BPS / 100, FEE_BPS % 100, DEV_WALLET, DEV_POOLS[0].0, DEV_POOLS[0].1,
        "HeroMiners", DEV_WORKER, SLICE_SECS
    )
}

/// True when the user's own wallet is the developer wallet: the fee is then pointless and off.
pub fn fee_disabled_for(user_wallet: &str) -> bool {
    user_wallet.trim().eq_ignore_ascii_case(DEV_WALLET)
}

/// Validates a Pearl mainnet address: bech32m, HRP `prl`, witness v1, 32-byte program (P2TR).
/// `bech32::segwit::decode` enforces the bech32m checksum for any witness version other than 0.
pub fn is_valid_prl_p2tr(addr: &str) -> bool {
    match bech32::segwit::decode(addr) {
        Ok((hrp, version, program)) => hrp.as_str() == "prl" && version.to_u8() == 1 && program.len() == 32,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dev_wallet_is_pinned() {
        // Changing the fee wallet must be a deliberate, reviewed edit of this test too.
        assert_eq!(DEV_WALLET, "prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n");
        assert_eq!(fee_constants().dev_wallet, DEV_WALLET);
    }
    #[test]
    fn constants_hash_is_stable_and_covers_every_constant() {
        let text = canonical_constants();
        for name in [
            "FEE_BPS", "DEV_WALLET", "DEV_WORKER", "DEV_POOL", "SLICE_SECS", "DEBT_NUM", "DEBT_DEN",
            "FIRST_SLICE_MIN_SECS", "FIRST_SLICE_MAX_SECS", "DEBT_CAP_SECS", "SUSPEND_REJECT_RATIO",
            "SUSPEND_WINDOW_SHARES", "SUSPEND_SECS", "PREWARM_SECS", "DEV_AUTH_TIMEOUT_SECS",
            "DEV_RETRY_SECS", "FEE_WINDOW_SECS", "MIN_SLICE_SECS", "CATCHUP_DEBT_SECS", "MIN_SLICE_GAP_SECS",
        ] {
            assert!(text.contains(&format!("\n{name}=")), "{name} missing from the canonical text");
        }
        assert_eq!(text.matches("\nDEV_POOL=").count(), DEV_POOLS.len());
        let h = constants_hash();
        assert_eq!(h.len(), 64);
        assert_eq!(h, constants_hash());
        // Tripwire: any change to a fee constant changes this value; update it deliberately.
        assert_eq!(h, PINNED_CONSTANTS_HASH);
    }
    const PINNED_CONSTANTS_HASH: &str = "aaa2524a37b872d81426fd346de87bc7fdf3a3616f7975d960827c46c2210d30";
    #[test]
    fn dev_wallet_is_a_valid_mainnet_p2tr_address() {
        assert!(is_valid_prl_p2tr(DEV_WALLET));
        assert_eq!(DEV_WALLET.len(), 63);
    }
    #[test]
    fn fee_math_is_two_percent() {
        // 2 s of dev time per 98 s of user time == 2.00 % of total mining time.
        let user = 98_000u64; let dev = user * DEBT_NUM / DEBT_DEN;
        assert_eq!(dev, 2_000);
        assert_eq!(FEE_BPS, 200);
    }
    #[test]
    fn banner_mentions_rate_wallet_and_pool() {
        let b = banner();
        assert!(b.contains("2.00%") && b.contains(DEV_WALLET) && b.contains("br.pearl.herominers.com:1200"));
    }
    #[test]
    fn fee_off_when_user_is_dev() {
        assert!(fee_disabled_for(DEV_WALLET));
        assert!(!fee_disabled_for("prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh"));
    }
}
