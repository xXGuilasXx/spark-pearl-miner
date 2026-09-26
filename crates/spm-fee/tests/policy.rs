//! Policy checks on the crate itself: the fee constants have no override path, and the crate
//! README states exactly the values in `src/lib.rs`.

use spm_fee::*;

const LIB: &str = include_str!("../src/lib.rs");
const SCHEDULER: &str = include_str!("../src/scheduler.rs");
const README: &str = include_str!("../README.md");

/// `pub fn` signatures of a source file (from `pub fn`/`pub const fn` to the opening brace).
fn pub_fn_signatures(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("pub ") {
        let tail = &rest[i..];
        let is_fn = tail.starts_with("pub fn ") || tail.starts_with("pub const fn ");
        if is_fn {
            let end = tail.find('{').unwrap_or(tail.len());
            out.push(tail[..end].split_whitespace().collect::<Vec<_>>().join(" "));
        }
        rest = &rest[i + 4..];
    }
    out
}

fn fn_name(sig: &str) -> &str {
    let after = &sig[sig.find("fn ").map_or(0, |i| i + 3)..];
    let end = after.find(['(', '<']).unwrap_or(after.len());
    &after[..end]
}

#[test]
fn fee_constants_are_compile_time_only() {
    for name in [
        "FEE_BPS", "DEV_WALLET", "DEV_WORKER", "DEV_POOLS", "SLICE_SECS", "DEBT_NUM", "DEBT_DEN",
        "FIRST_SLICE_MIN_SECS", "FIRST_SLICE_MAX_SECS", "DEBT_CAP_SECS", "SUSPEND_REJECT_RATIO",
        "SUSPEND_WINDOW_SHARES", "SUSPEND_SECS", "PREWARM_SECS", "DEV_AUTH_TIMEOUT_SECS", "DEV_RETRY_SECS",
        "FEE_WINDOW_SECS", "MIN_SLICE_SECS", "CATCHUP_DEBT_SECS", "MIN_SLICE_GAP_SECS",
    ] {
        assert_eq!(LIB.matches(&format!("pub const {name}:")).count(), 1, "{name} must be one `pub const`");
        assert!(!LIB.contains(&format!("static {name}")) && !SCHEDULER.contains(&format!("static {name}")));
    }
    // Nothing in the crate reads the environment or holds mutable global state, and the scheduler
    // does no I/O and reads no clock of its own.
    for (file, src) in [("lib.rs", LIB), ("scheduler.rs", SCHEDULER)] {
        for needle in [
            "std::env", "env!(", "option_env!(", "static mut", "OnceLock", "OnceCell", "LazyLock",
            "lazy_static", "thread_local", "Atomic", "Mutex", "RwLock", "RefCell", "Cell<", "std::fs",
            "std::net", "std::process", "std::thread", "SystemTime", "Instant", "include_str!", "unsafe ",
        ] {
            assert!(!src.contains(needle), "{file} contains `{needle}`");
        }
    }
    // Usable in const context: these are values, not places.
    const _BPS: u32 = FEE_BPS;
    const _WALLET: &str = DEV_WALLET;
    const _ALL: FeeConstants = fee_constants();
    assert_eq!(_ALL.dev_wallet, DEV_WALLET);
    assert_eq!(_ALL.fee_bps, FEE_BPS);
    assert_eq!(_ALL.dev_pools, DEV_POOLS);
}

#[test]
fn public_api_has_no_setter_or_override() {
    // The only `&mut self` methods are event inputs from the daemon; none takes a fee constant.
    let inputs = [
        "on_mining_started", "on_mining_stopped", "on_activity", "on_user_hashing", "on_dev_hashing",
        "on_dev_authorized", "on_dev_authorize_failed", "on_dev_session_lost", "on_dev_share", "poll",
    ];
    let mut seen = 0;
    for src in [LIB, SCHEDULER] {
        for sig in pub_fn_signatures(src) {
            let name = fn_name(&sig);
            seen += 1;
            for bad in ["set", "with_", "override", "replace", "configure", "update"] {
                assert!(!name.starts_with(bad), "public fn `{name}` looks like a setter");
            }
            if sig.contains("&mut self") {
                assert!(inputs.contains(&name), "unexpected mutating public fn: {sig}");
            }
            for part in sig.split(['(', ',', ')']) {
                if part.contains(": &str") {
                    let param = part.trim();
                    assert!(
                        ["user_wallet: &str", "json: &str", "addr: &str"].contains(&param),
                        "`{name}` takes a string that is not the user's wallet: {param}"
                    );
                }
            }
            let lower = sig.to_lowercase();
            for field in ["dev_wallet", "fee_bps", "dev_pool", "dev_worker", "slice_secs"] {
                assert!(!lower.contains(field), "`{name}` takes a fee constant as a parameter");
            }
        }
    }
    assert!(seen >= 20, "signature scan found only {seen} public fns");
}

#[test]
fn persisted_state_carries_no_fee_constant() {
    let mut s = FeeScheduler::new(1, "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh");
    s.on_mining_started(100);
    s.on_user_hashing(3_600);
    let json = s.persisted().to_json().expect("json");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let keys: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
    for key in &keys {
        for bad in ["wallet", "bps", "pool", "worker", "rate", "cap", "ratio"] {
            assert!(!key.contains(bad), "persisted field `{key}` looks like a fee constant");
        }
    }
    assert!(!json.contains("prl1"), "no address may be persisted");
    // Extra keys in an edited state file are ignored; they cannot redirect the fee.
    let tampered = json.replacen('{', r#"{"dev_wallet":"prl1attacker","fee_bps":0,"#, 1);
    let st = PersistedFeeState::from_json(&tampered).expect("parse");
    assert_eq!(st, s.persisted());
    let restored = FeeScheduler::restore(st, "prl1user");
    assert!(restored.is_enabled());
    assert_eq!(fee_constants().dev_wallet, DEV_WALLET);
}

#[test]
fn crate_readme_matches_the_constants() {
    let line = format!(
        "dev fee {}.{:02}% → {} @ {}:{} (HeroMiners), worker \"{}\", {} s slices, only while mining",
        FEE_BPS / 100,
        FEE_BPS % 100,
        DEV_WALLET,
        DEV_POOLS[0].0,
        DEV_POOLS[0].1,
        DEV_WORKER,
        SLICE_SECS
    );
    assert!(README.contains(&line), "README fee line differs from lib.rs:\n{line}");
    assert_eq!(banner().replace("->", "→"), line);
    let rows = [
        ("FEE_BPS", FEE_BPS.to_string()),
        ("DEV_WALLET", format!("`{DEV_WALLET}`")),
        ("DEV_WORKER", DEV_WORKER.to_string()),
        ("SLICE_SECS", SLICE_SECS.to_string()),
        ("FIRST_SLICE_MIN_SECS", FIRST_SLICE_MIN_SECS.to_string()),
        ("FIRST_SLICE_MAX_SECS", FIRST_SLICE_MAX_SECS.to_string()),
        ("DEBT_CAP_SECS", DEBT_CAP_SECS.to_string()),
        ("SUSPEND_REJECT_RATIO", SUSPEND_REJECT_RATIO.to_string()),
        ("SUSPEND_WINDOW_SHARES", SUSPEND_WINDOW_SHARES.to_string()),
        ("SUSPEND_SECS", SUSPEND_SECS.to_string()),
        ("PREWARM_SECS", PREWARM_SECS.to_string()),
        ("DEV_AUTH_TIMEOUT_SECS", DEV_AUTH_TIMEOUT_SECS.to_string()),
        ("DEV_RETRY_SECS", DEV_RETRY_SECS.to_string()),
        ("FEE_WINDOW_SECS", FEE_WINDOW_SECS.to_string()),
        ("MIN_SLICE_SECS", MIN_SLICE_SECS.to_string()),
        ("CATCHUP_DEBT_SECS", CATCHUP_DEBT_SECS.to_string()),
        ("MIN_SLICE_GAP_SECS", MIN_SLICE_GAP_SECS.to_string()),
    ];
    for (name, value) in rows {
        let row = format!("| `{name}` | {value} |");
        assert!(README.contains(&row), "README row missing or stale: {row}");
    }
    assert!(README.contains(&format!("| `DEBT_NUM` / `DEBT_DEN` | {DEBT_NUM} / {DEBT_DEN} |")));
    let pools: Vec<_> = DEV_POOLS.iter().map(|(host, port, _)| format!("{host}:{port}")).collect();
    assert!(README.contains(&format!("| `DEV_POOLS` | {} |", pools.join(", "))), "README pool list is stale");
    // Derived numbers quoted in the prose.
    let owed_after = SLICE_SECS * DEBT_DEN / DEBT_NUM;
    for s in [
        format!("{} s of debt", SLICE_SECS),
        format!("{owed_after} s ({} min)", owed_after / 60),
        format!("{}/{} s of debt", DEBT_NUM, DEBT_DEN),
        format!("[{}, {}] min", FIRST_SLICE_MIN_SECS / 60, FIRST_SLICE_MAX_SECS / 60),
        format!("{} s of user plus dev hashing", FEE_WINDOW_SECS),
        format!("({} s)", FEE_WINDOW_SECS * u64::from(FEE_BPS) / 10_000),
        format!("capped at {DEBT_CAP_SECS} s"),
        format!("Debt above {CATCHUP_DEBT_SECS} s"),
        format!("at least {} min of user hashing", MIN_SLICE_GAP_SECS / 60),
        format!("at most {SLICE_SECS} s per {} min", (MIN_SLICE_GAP_SECS + SLICE_SECS) / 60),
        format!("last {SUSPEND_WINDOW_SHARES} dev submits"),
        format!("the next try waits {DEV_RETRY_SECS} s"),
        format!("within {DEV_AUTH_TIMEOUT_SECS} s"),
        format!("comes {PREWARM_SECS} s before a slice"),
    ] {
        assert!(README.contains(&s), "README prose does not say `{s}`");
    }
    let local = SLICE_SECS as f64 * 100.0 / (MIN_SLICE_GAP_SECS + SLICE_SECS) as f64;
    assert!(README.contains(&format!("{local:.2} % locally")));
}
