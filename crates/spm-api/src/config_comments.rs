//! The comments of the generated `config.toml`.
//!
//! [`Config::to_toml`](crate::config::Config::to_toml) serializes with `toml::to_string_pretty`
//! and then runs [`annotate`] over the text: a header, a "Basic" and an "Advanced" banner, a short
//! introduction before each section and one to four `# ` lines before every key, looked up by
//! `(section, key)`. Values are never touched, so `from_toml(to_toml(x)) == x`, and every save
//! from the GUI writes the same comments again. Comments written by hand are not kept (the header
//! says so): the file is regenerated from the parsed values.

use crate::config::SCHEMA_VERSION;

/// First line of the Basic block.
pub const BASIC_BANNER: &str = "# ── Basic (also in the GUI: gear icon) ──────────────────────────────────────";
/// First line of the Advanced block.
pub const ADVANCED_BANNER: &str = "# ── Advanced: preset for the NVIDIA DGX Spark ──────────────────────────────";

fn header() -> String {
    format!(
        "# spark-pearl-miner settings (schema_version {SCHEMA_VERSION}). This is the ONLY settings file.\n\
         # The GUI edits the Basic block. Edit the Advanced block by hand only if you know why:\n\
         # the miner re-reads this file within a few seconds; an invalid edit is ignored (an alert says why)\n\
         # and the previous settings stay in force; [api] changes need: systemctl --user restart spark-pearl-miner.\n\
         # Check the file with: spark-pearl-miner config check. Reference: docs/en/CONFIGURATION.md\n\
         # The developer fee is not configurable.\n\
         # Comments you add by hand are NOT kept: every save from the GUI rewrites this file with these comments.\n"
    )
}

const ADVANCED_INTRO: &[&str] = &[
    "# Tested on the DGX Spark (GB10): failover in about 1 s, failback after 60 s stable, 73.9 T-MAC/s",
    "# at about 63 W on the GPU with the 2000 MHz clock cap. Change these only if you know why.",
];

/// Sections of the Advanced block, in file order; the first one gets the banner.
const ADVANCED_SECTIONS: &[&str] = &["failover", "power", "coexistence", "worker", "api"];

/// Introduction printed above a section header.
fn section_intro(section: &str, pool_index: usize) -> &'static [&'static str] {
    match (section, pool_index) {
        ("miner", _) => &["# Payout identity."],
        ("gui", _) => &["# The web interface."],
        ("pools", 0) => &[
            "# Up to 3 pools. The order is the priority: pool 1 is the main pool, pools 2 and 3 are backups.",
            "# The GUI shows host:port only; the other keys of an entry are kept as written here.",
            "# Pool 1 (main):",
        ],
        ("pools", 1) => &["# Pool 2 (backup 1):"],
        ("pools", _) => &["# Pool 3 (backup 2):"],
        ("failover", _) => &[
            "# When the active pool fails, the miner moves to the next enabled pool; it returns to a",
            "# higher-priority pool once that pool has stayed healthy for failback_stable_s.",
        ],
        ("power", _) => &[
            "# The power governor. The GB10 has no software power limit and is known to power off around",
            "# 88–92 W. Profiles (GPU power target / hard stop / boot clock cap):",
            "#   eco       60 W / 70 W / 1800 MHz",
            "#   balanced  75 W / 85 W / 2000 MHz  (default; measured about 63 W, GPU 72 °C, 73.9 T-MAC/s)",
            "#   max       88 W / 92 W / 2200 MHz  (measured 83–87 W and board 97.5 °C, above the 95 °C trip:",
            "#             not recommended; needs max_acknowledged = true and the clock-cap unit reinstalled",
            "#             with install-clockcap.sh --mhz 2200)",
            "# Built in (not settings): 10 Hz NVML sampling (2 Hz nvidia-smi fallback); GPU derate from 78 °C",
            "# at 3 W/°C; trips at GPU 83 °C and board (acpitz) 95 °C; 3 samples above the hard stop pause",
            "# mining for 60 s; 3 s without power readings holds the worker.",
        ],
        ("coexistence", _) => &[
            "# Sharing the GPU with other programs (an LLM server such as vLLM). Modes:",
            "#   exclusive      (default) the GPU is the miner's while mining; press Stop to use it for AI",
            "#   yield          pause the worker (CUDA context kept) while vLLM has requests",
            "#   yield-release  like yield, but the worker exits and frees its GPU memory",
            "#   spark-modo     the spark-modo \"miner\" runtime starts and stops the worker",
            "# Memory guard (built in, every mode): the worker starts only with MemAvailable >= 22 GiB",
            "# (2 GiB budget + 20 GiB headroom) and memory pressure (PSI some avg10) <= 10 %; it is",
            "# released below 16 GiB available or above 10 % pressure.",
        ],
        ("worker", _) => &["# The GPU worker process."],
        ("api", _) => &[
            "# The local web GUI and API. Changes here need: systemctl --user restart spark-pearl-miner",
        ],
        _ => &[],
    }
}

/// Comment printed above `key` in `section` (`""` is the top level). Pools 2 and 3 get the short
/// form.
fn key_comment(section: &str, key: &str, pool_index: usize) -> Option<&'static [&'static str]> {
    let lines: &'static [&'static str] = match (section, key) {
        ("", "schema_version") => &["# File format version: do not change."],
        ("miner", "wallet") => &[
            "# Your Pearl address (bech32m, prl1p…). Use a wallet you control (self-custody).",
            "# Empty until the setup wizard runs.",
        ],
        ("miner", "worker") => &["# Name shown on the pool's website: 1–32 letters, digits, _ or -. Default: spark."],
        ("miner", "disclosure_accepted") => &[
            "# Set by the setup wizard when you accept the 2 % developer fee; mining does not start while false.",
        ],
        ("gui", "language") => &["# Interface language: auto (from the browser), en or pt-BR."],
        ("pools", _) if pool_index > 0 => match key {
            "name" => &["# label"],
            "host" => &["# host name or IP address"],
            "port" => &["# TCP port (1–65535)"],
            "tls" => &["# on | off | auto | pinned"],
            "spki_pin" => &["# only with tls = \"pinned\""],
            "dialect" => &["# auto | object | kryptex | kryptex-v2"],
            "jsonrpc" => &["# auto | on | off"],
            "proof" => &["# auto | plain | zstd"],
            "password" => &["# stratum password"],
            "pattern" => &["# auto | official"],
            "enabled" => &["# true | false"],
            _ => return None,
        },
        ("pools", "name") => &["# Label shown in the GUI and the logs (up to 40 characters)."],
        ("pools", "host") => &["# Pool host name or IP address."],
        ("pools", "port") => &["# TCP port (1–65535)."],
        ("pools", "tls") => &[
            "# Transport: on (TLS) | off (plain TCP) | auto (TLS; plain only if the pool has no TLS, never",
            "# after a certificate error) | pinned (TLS checked against spki_pin only, for self-signed pools).",
        ],
        ("pools", "spki_pin") => &["# Only with tls = \"pinned\": base64 SHA-256 of the server's public key (SPKI)."],
        ("pools", "dialect") => &["# Wire dialect: auto (from the host name) | object (HeroMiners, LuckyPool) | kryptex | kryptex-v2."],
        ("pools", "jsonrpc") => &["# The \"jsonrpc\":\"2.0\" member on requests: auto | on | off."],
        ("pools", "proof") => &["# Proof encoding on submit: auto (learned per pool) | plain | zstd."],
        ("pools", "password") => &["# Stratum password: x (Kryptex also takes d=<difficulty>); up to 64 printable characters."],
        ("pools", "pattern") => &["# Hash-tile pattern: auto (fastest) | official."],
        ("pools", "enabled") => &["# false keeps the entry but never connects to it."],
        ("failover", "connect_timeout_s") => &["# seconds for DNS and for the TCP connect, each (1–120); tested: 10"],
        ("failover", "handshake_timeout_s") => &["# seconds for the TLS handshake and the login (1–120); tested: 15"],
        ("failover", "first_job_timeout_s") => &["# seconds to wait for the first job after the login (1–600); tested: 30"],
        ("failover", "stall_soft_reconnect_s") => &[
            "# seconds without a new job before one reconnect, then a failover (10–7200); tested: 900",
        ],
        ("failover", "max_consecutive_invalid") => &["# invalid shares in a row that count as a reject storm (1–1000); tested: 5"],
        ("failover", "reject_ratio_max") => &["# rejected-share ratio over reject_window that fails the pool over (0–1); tested: 0.5"],
        ("failover", "reject_window") => &["# shares in the reject window (1–1000); tested: 20"],
        ("failover", "stale_ratio_max") => &["# stale-share ratio over stale_window that fails the pool over (0–1); tested: 0.02"],
        ("failover", "stale_window") => &["# shares in the stale window (1–10000); tested: 100"],
        ("failover", "submit_ack_timeout_s") => &["# seconds to wait for the pool to answer a share (1–600); tested: 30"],
        ("failover", "max_ack_timeouts") => &["# unanswered shares in a row that fail the pool over (1–100); tested: 3"],
        ("failover", "backoff_s") => &[
            "# waits in seconds after consecutive failures of one pool; the last one repeats",
            "# (1–16 steps, each 1–3600); tested: 5, 10, 20, 40, 80, 120",
        ],
        ("failover", "backoff_jitter_pct") => &["# random +/- percent applied to each backoff wait (0–100); tested: 20"],
        ("failover", "failback_probe_every_s") => &[
            "# seconds between probes of a recovered higher-priority pool (1–86400); tested: 300",
        ],
        ("failover", "failback_stable_s") => &[
            "# seconds a recovered higher-priority pool must stay healthy before we switch back (1–86400); tested: 60",
        ],
        ("failover", "auth_retry_s") => &["# seconds before retrying a pool that refused the login (1–86400); tested: 600"],
        ("failover", "quarantine_s") => &["# seconds a pool that sent ban text is skipped (1–86400); tested: 600"],
        ("failover", "drain_s") => &[
            "# seconds the old pool still receives in-flight shares after a planned switch (0–120); tested: 5",
        ],
        ("failover", "reconnect_same_after_s") => &[
            "# a pool that was mining longer than this gets one reconnect before a failover (0–86400); tested: 60",
        ],
        ("power", "profile") => &["# eco | balanced | max (see the table above); default: balanced"],
        ("power", "max_acknowledged") => &[
            "# must be true for profile = \"max\": you accept the risk of the Spark's hard power-off",
        ],
        ("coexistence", "mode") => &["# exclusive | yield | yield-release | spark-modo (see the table above); default: exclusive"],
        ("coexistence", "metrics_url") => &[
            "# vLLM Prometheus endpoint, plain http:// only; used only by the yield modes",
        ],
        ("coexistence", "poll_ms") => &["# metrics poll period in milliseconds (100–250); default: 200"],
        ("coexistence", "idle_s") => &["# seconds the LLM server must stay idle before mining resumes (1–600); default: 5"],
        ("coexistence", "busy_sm_pct") => &[
            "# without metrics, another process at or above this SM utilization counts as busy (1–100 %); default: 10",
        ],
        ("worker", "launch") => &[
            "# spawn (the miner starts the worker) | external (something else starts it; forced by mode = \"spark-modo\")",
        ],
        ("worker", "simulate") => &[
            "# simulate: testing only, no real mining (CPU reference worker; never on a real miner)",
        ],
        ("worker", "sim_interval_ms") => &["# pause between two simulated attempts in milliseconds (50–60000)"],
        ("api", "bind") => &[
            "# loopback address only (127.0.0.1 or ::1); remote access: ssh -L 4078:127.0.0.1:4078 you@your-spark",
        ],
        ("api", "port") => &["# TCP port of the GUI and the API (1–65535); default: 4078"],
        ("api", "lan") => &["# lan cannot be enabled (LAN access needs TLS, which this build does not have)"],
        ("api", "trust_local_user") => &[
            "# true: your own user account on this machine needs no token; false forces the token even for you",
        ],
        _ => return None,
    };
    Some(lines)
}

/// `key` of a `key = value` line at the start of a line.
fn key_of(line: &str) -> Option<&str> {
    let (k, _) = line.split_once(" = ")?;
    (!k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).then_some(k)
}

/// Bracket balance of a line's array value (strings in our arrays never contain brackets).
fn bracket_delta(s: &str) -> i32 {
    s.bytes().fold(0, |d, b| match b {
        b'[' => d + 1,
        b']' => d - 1,
        _ => d,
    })
}

/// Add the header, the banners and the per-key comments to `toml::to_string_pretty` output.
pub fn annotate(body: &str) -> String {
    let mut out = header();
    out.push('\n');
    out.push_str(BASIC_BANNER);
    out.push_str("\n\n");
    let mut section = String::new();
    let mut pool_index = 0usize;
    let mut pools_seen = 0usize;
    let mut advanced = false;
    let mut depth = 0i32;
    for line in body.lines() {
        if depth > 0 {
            depth += bracket_delta(line);
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("[[").and_then(|r| r.strip_suffix("]]")) {
            section = name.to_string();
            pool_index = pools_seen;
            pools_seen += 1;
        } else if let Some(name) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.to_string();
            pool_index = 0;
            if !advanced && ADVANCED_SECTIONS.contains(&name) {
                advanced = true;
                out.push_str(ADVANCED_BANNER);
                out.push('\n');
                for l in ADVANCED_INTRO {
                    out.push_str(l);
                    out.push('\n');
                }
                out.push('\n');
            }
        } else if let Some(key) = key_of(line) {
            if let Some(lines) = key_comment(&section, key, pool_index) {
                for l in lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            if let Some((_, value)) = line.split_once(" = ") {
                if value.trim_start().starts_with('[') {
                    depth = bracket_delta(value);
                }
            }
            out.push_str(line);
            out.push('\n');
            continue;
        } else {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        for l in section_intro(&section, pool_index) {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_line_arrays_are_not_mistaken_for_keys() {
        let text = annotate("[failover]\nbackoff_s = [\n    5,\n    10,\n]\ndrain_s = 5\n");
        let lines: Vec<&str> = text.lines().collect();
        let i = lines.iter().position(|l| *l == "drain_s = 5").unwrap();
        assert!(lines[i - 1].starts_with("# seconds the old pool"));
        let j = lines.iter().position(|l| *l == "    5,").unwrap();
        assert!(!lines[j - 1].starts_with('#'));
    }
}
