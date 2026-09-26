//! `spark-pearl-miner fee-test`: one developer-fee cycle through the real `FeeScheduler` with
//! compressed time (a virtual clock), so the whole PreWarm → StartSlice → EndSlice sequence and
//! the measured fee can be checked in a second instead of 100 minutes.
//!
//! By default nothing leaves the machine. With `--connect`, the PreWarm step really opens the dev
//! session (the first reachable `DEV_POOLS` entry, worker `devfee`) and waits for the login; it
//! never submits a share.

use std::time::Duration;

use spm_fee::{FeeAction, FeeScheduler, DEV_POOLS, DEV_WALLET, DEV_WORKER};

/// One thing that happened, on the virtual clock.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeTestEvent {
    /// Virtual seconds since mining started.
    pub t: u64,
    pub action: FeeAction,
}

#[derive(Debug, Clone)]
pub struct FeeTestReport {
    pub events: Vec<FeeTestEvent>,
    pub user_secs: u64,
    pub dev_secs: u64,
    pub measured_pct: f64,
    pub enabled: bool,
}

/// Where the dev login of PreWarm comes from.
pub trait DevLogin {
    /// `Ok(description)` when the dev session authorized.
    fn login(&mut self) -> Result<String, String>;
}

/// No network: the login always succeeds.
pub struct OfflineLogin;

impl DevLogin for OfflineLogin {
    fn login(&mut self) -> Result<String, String> {
        Ok(format!("simulated login to {}:{} as {DEV_WORKER} (offline)", DEV_POOLS[0].0, DEV_POOLS[0].1))
    }
}

/// Run one full cycle. `user_wallet` only matters for the auto-off rule. `pace` slows each
/// virtual second down to that much real time (zero = as fast as possible). Stops after the first
/// `EndSlice`/`Abort`, or after `max_virtual_s`.
pub fn run_cycle(user_wallet: &str, seed: u64, login: &mut dyn DevLogin, pace: Duration, max_virtual_s: u64, mut on_event: impl FnMut(&FeeTestEvent, &str)) -> FeeTestReport {
    let mut s = FeeScheduler::new(seed, user_wallet);
    let t0 = 1_000_000u64;
    s.on_mining_started(t0);
    let mut events = Vec::new();
    let mut t = 0u64;
    while t < max_virtual_s {
        if s.in_slice() {
            s.on_dev_hashing(1);
        } else {
            s.on_user_hashing(1);
        }
        t += 1;
        let mut done = false;
        while let Some(a) = s.poll(t0 + t) {
            let ev = FeeTestEvent { t, action: a };
            let note = match a {
                FeeAction::PreWarm => match login.login() {
                    Ok(desc) => {
                        s.on_dev_authorized(t0 + t);
                        desc
                    }
                    Err(e) => {
                        s.on_dev_authorize_failed(t0 + t);
                        format!("dev login failed: {e}")
                    }
                },
                FeeAction::StartSlice => "GPU moves to the dev job".to_string(),
                FeeAction::EndSlice => {
                    done = true;
                    "slice paid; GPU back on the user pool".to_string()
                }
                FeeAction::Abort(r) => {
                    done = true;
                    format!("slice aborted: {r:?}")
                }
            };
            on_event(&ev, &note);
            events.push(ev);
        }
        if done {
            break;
        }
        if !pace.is_zero() {
            std::thread::sleep(pace);
        }
    }
    let st = s.stats();
    FeeTestReport {
        events,
        user_secs: st.user_hash_secs,
        dev_secs: st.dev_hash_secs,
        measured_pct: st.measured_fee_pct,
        enabled: s.is_enabled(),
    }
}

/// `--connect`: a real dev login on the first reachable dev pool (authorize only).
pub struct LiveLogin {
    pub rt: tokio::runtime::Handle,
}

impl DevLogin for LiveLogin {
    fn login(&mut self) -> Result<String, String> {
        self.rt.block_on(async {
            let mut errors = Vec::new();
            for (host, port, _) in DEV_POOLS {
                let cfg = crate::daemon::dev_session_config(host, *port, None);
                let (session, mut ev) = spm_proto::client::PoolSession::spawn(cfg, crate::daemon::connector(10, 15));
                let r = tokio::time::timeout(Duration::from_secs(20), async {
                    while let Some(e) = ev.recv().await {
                        match e {
                            spm_proto::client::SessionEvent::Authorized { .. } => return Ok(()),
                            spm_proto::client::SessionEvent::AuthRejected { reason } => return Err(reason),
                            spm_proto::client::SessionEvent::Disconnected { reason } => return Err(format!("{reason:?}")),
                            _ => {}
                        }
                    }
                    Err("session ended".to_string())
                })
                .await;
                session.shutdown().await;
                match r {
                    Ok(Ok(())) => {
                        return Ok(format!("dev session authorized on {host}:{port} as {DEV_WALLET}.{DEV_WORKER} (no share submitted)"));
                    }
                    Ok(Err(e)) => errors.push(format!("{host}:{port}: {e}")),
                    Err(_) => errors.push(format!("{host}:{port}: timed out")),
                }
            }
            Err(errors.join("; "))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_cycle_is_prewarm_start_end() {
        let r = run_cycle("prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh", 42, &mut OfflineLogin, Duration::ZERO, 20_000, |_, _| {});
        let actions: Vec<FeeAction> = r.events.iter().map(|e| e.action).collect();
        assert_eq!(actions, vec![FeeAction::PreWarm, FeeAction::StartSlice, FeeAction::EndSlice]);
        let (pre, start, end) = (r.events[0].t, r.events[1].t, r.events[2].t);
        assert_eq!(start - pre, spm_fee::PREWARM_SECS);
        assert_eq!(end - start, spm_fee::SLICE_SECS);
        // A fresh install owes one slice after 98 min of hashing: 120 / 6000 = 2.00 %.
        assert_eq!(r.dev_secs, spm_fee::SLICE_SECS);
        assert!((r.measured_pct - 2.0).abs() < 0.01, "{}", r.measured_pct);
    }

    #[test]
    fn the_fee_wallet_itself_pays_no_fee() {
        let r = run_cycle(DEV_WALLET, 1, &mut OfflineLogin, Duration::ZERO, 20_000, |_, _| {});
        assert!(r.events.is_empty());
        assert!(!r.enabled);
    }

    #[test]
    fn a_failed_dev_login_aborts_without_fee_time() {
        struct Refused;
        impl DevLogin for Refused {
            fn login(&mut self) -> Result<String, String> {
                Err("refused".into())
            }
        }
        let r = run_cycle("prl1user", 3, &mut Refused, Duration::ZERO, 20_000, |_, _| {});
        let actions: Vec<FeeAction> = r.events.iter().map(|e| e.action).collect();
        assert_eq!(actions, vec![FeeAction::PreWarm, FeeAction::Abort(spm_fee::AbortReason::AuthorizeFailed)]);
        assert_eq!(r.dev_secs, 0);
    }
}
