//! Developer fee — the ONLY place where the fee is defined. README.md, README.pt-BR.md and
//! docs/*/FEE.md must show exactly these values; CI fails otherwise.
//!
//! Mechanism (see docs/en/ARCHITECTURE.md): a time slice on a separate, pre-connected pool
//! session, never share-splitting (Pearl shares are bound to each session's job header).

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
