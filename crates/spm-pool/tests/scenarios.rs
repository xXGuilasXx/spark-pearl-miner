//! Deterministic, time-warped failover scenarios. Fake pools answer after 50 ms, so a healthy
//! TLS session goes Resolve (t) → Connect (t+50) → TLS (t+150) → Authorize → first job (t+250).
mod common;

use common::*;
use spm_pool::*;
use std::time::Duration;

fn is_resolve(s: SlotId) -> impl Fn(&Action) -> bool {
    move |a| *a == Action::Resolve { slot: s }
}

fn is_probe(s: SlotId) -> impl Fn(&Action) -> bool {
    move |a| *a == Action::Probe { slot: s }
}

fn mining(s: SlotId) -> ManagerState {
    ManagerState::Mining { active: s }
}

/// Run until `pred` holds (checked every 50 ms), at most `limit` seconds.
fn run_until_cond(sim: &mut Sim, limit_s: u64, pred: impl Fn(&Sim) -> bool) {
    let end = sim.now.plus_secs(limit_s);
    while !pred(sim) && sim.now < end {
        sim.run_for(Duration::from_millis(50));
    }
    assert!(pred(sim), "condition not reached by {}", sim.now);
}

// ---------------------------------------------------------------------------------------------
// Start-up and basic failover
// ---------------------------------------------------------------------------------------------

#[test]
fn starts_on_the_highest_priority_slot() {
    let mut sim = Sim::new(pools(3), vec![]);
    sim.start();
    assert_eq!(sim.manager(), ManagerState::Starting);
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.activated(P1, Instant::ZERO), Some(ms(250)));
    assert!(sim.gpu);
    assert_eq!(sim.count(|a| matches!(a, Action::Resolve { slot } if *slot != P1)), 0);
    assert_eq!(sim.state.tls_cached("pool-a.example", 1200), Some(true));
}

#[test]
fn pool1_refuses_pool2_mining_within_2s() {
    let mut sim = Sim::new(pools(3), vec![FakePool::refusing()]);
    sim.start();
    sim.run_until(ms(100));
    assert_eq!(sim.manager(), ManagerState::FailingOver { from: P1, to: P2 });
    sim.run_until(secs(2));
    assert_eq!(sim.manager(), mining(P2));
    let t = sim.activated(P2, Instant::ZERO).expect("pool 2 active");
    assert!(t <= secs(2), "pool 2 active at {t}");
    assert!(sim.gpu);
    match sim.slot(P1) {
        SlotState::Backoff { until, n } => {
            assert_eq!(n, 1);
            assert!(until >= ms(100 + 4000) && until <= ms(100 + 6000), "until {until}");
        }
        s => panic!("pool 1 is {s:?}"),
    }
    assert_eq!(sim.count(is_resolve(P3)), 0);
}

#[test]
fn dns_failure_fails_over_immediately() {
    let mut sim = Sim::new(pools(2), vec![FakePool { dns_ok: false, ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(1);
    assert_eq!(sim.first(is_resolve(P2)), Some(ms(50)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn connect_blackhole_times_out_after_10s() {
    let mut sim = Sim::new(pools(2), vec![FakePool { net: Net::Blackhole, ..FakePool::default() }]);
    sim.start();
    sim.run_until(secs(10));
    assert_eq!(sim.manager(), ManagerState::Starting);
    sim.run_for_secs(1);
    assert_eq!(sim.first(is_resolve(P2)), Some(ms(10_050)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn no_job_within_30s_of_authorize_fails_over() {
    let mut sim = Sim::new(pools(2), vec![FakePool { first_job: false, ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(29);
    assert_eq!(sim.slot(P1), SlotState::AwaitingJob);
    sim.run_for_secs(2);
    // Authorized at 200 ms, deadline 30 s later.
    assert_eq!(sim.first(is_resolve(P2)), Some(ms(30_200)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn disabled_slots_are_never_contacted() {
    let mut cfg = pools(3);
    cfg[1].enabled = false;
    let mut sim = Sim::new(cfg, vec![FakePool::refusing()]);
    sim.start();
    sim.run_for_secs(2);
    assert_eq!(sim.manager(), mining(P3));
    assert_eq!(sim.slot(P2), SlotState::Disabled);
    assert!(sim.actions().all(|(_, a)| !matches!(a,
        Action::Resolve { slot } | Action::Connect { slot, .. } | Action::Probe { slot } if *slot == P2)));
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

#[test]
fn tls_protocol_error_retries_plain_and_caches_per_host() {
    let mut sim = Sim::new(pools(2), vec![FakePool { tls: Tls::PlainOnly, ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.first(|a| *a == Action::Connect { slot: P1, tls: true }), Some(ms(50)));
    assert_eq!(sim.first(|a| *a == Action::Connect { slot: P1, tls: false }), Some(ms(150)));
    assert_eq!(sim.state.tls_cached("pool-a.example", 1200), Some(false));
    assert_eq!(sim.count(is_resolve(P2)), 0);
    // The next session to the same host goes straight to plain TCP.
    sim.run_until(secs(120));
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 119 });
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.count(|a| *a == Action::Connect { slot: P1, tls: true }), 1);
    assert_eq!(sim.count(|a| *a == Action::Connect { slot: P1, tls: false }), 2);
}

#[test]
fn tls_other_error_fails_over_without_plain_fallback() {
    let mut sim = Sim::new(pools(2), vec![FakePool { tls: Tls::BadCert, ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P2));
    assert_eq!(sim.count(|a| *a == Action::Connect { slot: P1, tls: false }), 0);
    assert!(matches!(sim.slot(P1), SlotState::Backoff { n: 1, .. }));
    assert_eq!(sim.state.tls_cached("pool-a.example", 1200), None);
}

#[test]
fn tls_forced_on_never_downgrades() {
    let mut cfg = pools(2);
    cfg[0].tls = TlsMode::On;
    let mut sim = Sim::new(cfg, vec![FakePool { tls: Tls::PlainOnly, ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P2));
    assert_eq!(sim.count(|a| *a == Action::Connect { slot: P1, tls: false }), 0);
}

// ---------------------------------------------------------------------------------------------
// Authorization and bans
// ---------------------------------------------------------------------------------------------

#[test]
fn auth_rejected_on_pools_1_and_2_goes_to_pool_3() {
    let bad = FakePool { auth: Auth::Reject("invalid wallet address"), ..FakePool::default() };
    let mut sim = Sim::new(pools(3), vec![bad.clone(), bad]);
    sim.start();
    sim.run_for_secs(2);
    assert_eq!(sim.manager(), mining(P3));
    for s in [P1, P2] {
        match sim.slot(s) {
            SlotState::ConfigError { msg, retry_at } => {
                assert_eq!(msg, "invalid wallet address");
                assert!(retry_at > secs(600) && retry_at < secs(601), "retry_at {retry_at}");
            }
            other => panic!("{s} is {other:?}"),
        }
    }
    let alerts = sim.alerts();
    assert_eq!(alerts.iter().filter(|m| m.contains("authorization rejected")).count(), 2);
}

#[test]
fn auth_error_is_retried_every_10_minutes() {
    let mut sim = Sim::new(pools(1), vec![FakePool { auth: Auth::Reject("unknown worker"), ..FakePool::default() }]);
    sim.start();
    sim.run_for_secs(1);
    assert!(matches!(sim.manager(), ManagerState::AllDown { .. }));
    assert!(!sim.gpu);
    sim.run_until(secs(300));
    sim.pools[0].auth = Auth::Ok; // the user fixed it pool-side; no config edit
    sim.run_until(secs(600));
    assert_eq!(sim.count(is_resolve(P1)), 1, "no retry before 10 min");
    sim.run_until(secs(602));
    // Authorize sent at 150 ms, rejected at 200 ms: retried 600 s later.
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0), ms(600_200)]);
    assert_eq!(sim.manager(), mining(P1));
}

#[test]
fn ban_text_quarantines_for_10_minutes_then_failback_probes_it() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(10));
    sim.send(Event::BanText { slot: P1 });
    assert_eq!(sim.slot(P1), SlotState::Quarantined { until: secs(610) });
    assert!(sim.alerts().iter().any(|m| m.contains("banned")));
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P2));
    // P2 active at 10.25 s → failback check at 310.25 s finds P1 quarantined.
    sim.run_until(secs(609));
    assert_eq!(sim.slot(P1), SlotState::Quarantined { until: secs(610) });
    assert_eq!(sim.count(is_resolve(P1)), 1);
    sim.run_until(secs(611));
    assert_eq!(sim.times(is_probe(P1)), vec![ms(610_250)]);
    sim.run_until(secs(700));
    assert_eq!(sim.activated(P1, secs(11)), Some(ms(670_500)));
    assert_eq!(sim.manager(), mining(P1));
}

// ---------------------------------------------------------------------------------------------
// All down, backoff and jitter
// ---------------------------------------------------------------------------------------------

#[test]
fn all_pools_down_then_recovery() {
    let mut sim = Sim::new(pools(3), vec![FakePool::refusing(), FakePool::refusing(), FakePool::refusing()]);
    sim.start();
    sim.run_until(ms(400));
    assert_eq!(sim.manager(), ManagerState::AllDown { since: ms(300) });
    assert!(sim.alerts().iter().any(|m| m == ALL_DOWN_ALERT));
    assert!(!sim.gpu);
    sim.run_until(secs(60));
    assert!(matches!(sim.manager(), ManagerState::AllDown { .. }));
    assert!(!sim.gpu);
    assert_eq!(sim.count(|a| *a == Action::StartGpu), 0);
    // Round-robin retries honour each slot's backoff: attempts of one slot are ≥ 4 s apart and
    // the gaps grow.
    for s in [P1, P2, P3] {
        let t = sim.times(is_resolve(s));
        assert!(t.len() >= 3, "{s} retried {} times", t.len());
        let gaps: Vec<u64> = t.windows(2).map(|w| w[1].as_millis() - w[0].as_millis()).collect();
        assert!(gaps.iter().all(|&g| g >= 4_000), "{s} gaps {gaps:?}");
        assert!(gaps.windows(2).all(|g| g[1] > g[0]), "{s} gaps {gaps:?}");
    }
    sim.pools[1].net = Net::Ok;
    run_until_cond(&mut sim, 200, |s| s.manager() == mining(P2));
    // The first job on P2 is what resumes mining.
    let t_active = sim.activated(P2, Instant::ZERO).expect("P2 active");
    let job_step = sim.log.iter().find(|(t, ev, _)| *t == t_active && matches!(ev, Event::JobReceived { slot, .. } if *slot == P2));
    assert!(job_step.is_some());
    assert!(sim.gpu);
}

#[test]
fn flapping_pool_ends_in_backoff_with_growing_delays() {
    let mut sim = Sim::new(pools(1), vec![]);
    sim.start();
    let bases = [5_000u64, 10_000, 20_000, 40_000, 80_000, 120_000];
    let mut delays = Vec::new();
    for (k, base) in bases.iter().enumerate() {
        run_until_cond(&mut sim, 200, |s| s.state.active() == Some(P1));
        sim.run_for_secs(1);
        sim.send(Event::Disconnected { slot: P1, was_active_for_s: 1 });
        match sim.slot(P1) {
            SlotState::Backoff { until, n } => {
                assert_eq!(n as usize, k + 1);
                let d = until.as_millis() - sim.now.as_millis();
                assert!(d >= base * 8 / 10 && d <= base * 12 / 10, "delay #{n} = {d} ms");
                delays.push(d);
            }
            s => panic!("after flap {k}: {s:?}"),
        }
    }
    assert!(delays.windows(2).all(|w| w[1] >= w[0]), "delays {delays:?}");
    assert!(delays[..5].windows(2).all(|w| w[1] > w[0]), "delays {delays:?}");
    assert!(matches!(sim.slot(P1), SlotState::Backoff { n: 6, .. }));
    assert!(matches!(sim.manager(), ManagerState::AllDown { .. }));
}

fn backoff_delays(seed: u64) -> Vec<u64> {
    let mut sim = Sim::with_seed(pools(1), vec![FakePool::refusing()], seed);
    sim.start();
    sim.run_until(secs(600));
    // Each attempt fails 100 ms after its Resolve; the next Resolve is exactly at the backoff end.
    sim.times(is_resolve(P1))
        .windows(2)
        .map(|w| w[1].as_millis() - (w[0].as_millis() + 100))
        .collect()
}

#[test]
fn backoff_jitter_is_deterministic_for_a_fixed_seed() {
    let a = backoff_delays(42);
    let b = backoff_delays(42);
    let c = backoff_delays(43);
    assert!(a.len() >= 7);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let bases = [5_000u64, 10_000, 20_000, 40_000, 80_000, 120_000];
    for (k, d) in a.iter().enumerate() {
        let base = bases[k.min(5)];
        assert!(*d >= base * 8 / 10 && *d <= base * 12 / 10, "delay #{k} = {d}");
    }
}

#[test]
fn no_enabled_pool_is_all_down_with_an_alert() {
    let mut cfg = pools(1);
    cfg[0].enabled = false;
    let mut sim = Sim::new(cfg, vec![]);
    sim.start();
    assert!(matches!(sim.manager(), ManagerState::AllDown { .. }));
    assert!(sim.alerts().iter().any(|m| m.contains("no pool")));
    assert_eq!(sim.count(|a| matches!(a, Action::Resolve { .. })), 0);
}

// ---------------------------------------------------------------------------------------------
// Share health
// ---------------------------------------------------------------------------------------------

#[test]
fn consecutive_invalid_rejects_fail_over() {
    let bad = FakePool { share: Share::Reject(RejectKind::Invalid), ..FakePool::default() };
    let mut sim = Sim::new(pools(3), vec![bad]);
    sim.start();
    sim.run_for_secs(1);
    sim.mine(4, 1000);
    assert_eq!(sim.manager(), mining(P1));
    sim.mine(1, 1000);
    assert_eq!(sim.manager(), mining(P2));
    assert!(sim.alerts().iter().any(|m| m.contains("invalid-share storm")));
    assert!(matches!(sim.slot(P1), SlotState::Backoff { .. }));
}

#[test]
fn reject_ratio_over_half_of_last_20_fails_over_stale_and_low_diff_excluded() {
    use Share::*;
    let bad = Reject(RejectKind::Invalid);
    let mut script = Vec::new();
    for _ in 0..6 {
        script.push(Reject(RejectKind::Stale));
        script.push(Reject(RejectKind::LowDiff));
    }
    // 14 bad out of 20, never more than 2 in a row.
    for k in 0..20 {
        script.push(if k % 3 == 2 { Accept } else { bad });
    }
    let p1 = FakePool { share_script: script.into(), ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_for_secs(1);
    sim.mine(12 + 19, 500);
    assert_eq!(sim.manager(), mining(P1), "12 excluded + 19 counted rejects must not trigger");
    sim.mine(1, 1000);
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn low_diff_rejects_never_fail_over() {
    let p1 = FakePool { share: Share::Reject(RejectKind::LowDiff), ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_for_secs(1);
    sim.mine(40, 500);
    assert_eq!(sim.manager(), mining(P1));
}

#[test]
fn reject_storm_on_the_next_pool_pauses_reject_everywhere() {
    let bad = FakePool { share: Share::Reject(RejectKind::Invalid), ..FakePool::default() };
    let mut sim = Sim::new(pools(3), vec![bad.clone(), bad]);
    sim.start();
    sim.run_for_secs(1);
    sim.mine(5, 1000);
    assert_eq!(sim.manager(), mining(P2));
    sim.run_for_secs(1);
    sim.mine(5, 1000);
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::RejectEverywhere });
    assert!(!sim.gpu);
    assert!(sim.alerts().iter().any(|m| m == REJECT_EVERYWHERE_ALERT));
    assert!(SlotId::all().all(|s| !sim.slot(s).is_open()));
    assert_eq!(sim.count(is_resolve(P3)), 0, "pool 3 is not tried: the miner is the problem");
    // Stays paused.
    sim.run_for_secs(600);
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::RejectEverywhere });
    // After an update the user resumes; pool 1 accepts again.
    sim.pools[0].share = Share::Accept;
    sim.send(Event::ResumeRequest);
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P1));
    sim.mine(5, 1000);
    assert_eq!(sim.manager(), mining(P1));
}

#[test]
fn stale_over_2pct_of_last_100_fails_over() {
    use Share::*;
    let mut script = vec![Accept; 100];
    script[10] = Reject(RejectKind::Stale);
    script[50] = Reject(RejectKind::Stale);
    script.push(Reject(RejectKind::Stale)); // 101st share: the last 100 now hold 3 stale
    let p1 = FakePool { share_script: script.into(), ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_for_secs(1);
    sim.mine(100, 200);
    assert_eq!(sim.manager(), mining(P1), "2 % is the limit, not a failure");
    sim.mine(1, 1000);
    assert_eq!(sim.manager(), mining(P2));
    assert!(sim.alerts().iter().any(|m| m.contains("stale")));
}

#[test]
fn three_submit_ack_timeouts_fail_over() {
    let p1 = FakePool { share: Share::NoAck, ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_until(secs(1));
    sim.hit();
    sim.run_until(secs(2));
    sim.hit();
    sim.run_until(secs(3));
    sim.hit();
    assert_eq!(sim.state.pending_acks(P1), 3);
    sim.run_until(ms(32_500));
    assert_eq!(sim.manager(), mining(P1), "two timeouts are tolerated");
    sim.run_until(secs(34));
    assert_eq!(sim.first(is_resolve(P2)), Some(secs(33)));
    assert_eq!(sim.manager(), mining(P2));
    assert!(sim.alerts().iter().any(|m| m.contains("not acknowledged")));
}

#[test]
fn ack_timeout_streak_resets_on_an_ack_and_spurious_timeouts_are_ignored() {
    let p1 = FakePool { share: Share::NoAck, ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_until(secs(1));
    for _ in 0..3 {
        sim.hit();
    }
    sim.send(Event::SubmitAckTimeout { slot: P1 });
    sim.send(Event::SubmitAckTimeout { slot: P1 });
    sim.send(Event::ShareAccepted { slot: P1 });
    assert_eq!(sim.state.pending_acks(P1), 0);
    // No submit pending: ignored.
    for _ in 0..5 {
        sim.send(Event::SubmitAckTimeout { slot: P1 });
    }
    sim.hit();
    sim.hit();
    sim.send(Event::SubmitAckTimeout { slot: P1 });
    sim.send(Event::SubmitAckTimeout { slot: P1 });
    assert_eq!(sim.manager(), mining(P1));
}

#[test]
fn hits_are_submitted_only_on_the_originating_session() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_for_secs(1);
    let job = sim.state.current_job(P1).expect("job").to_string();
    assert_eq!(sim.hit_on(P2, &job), Action::DiscardStale { slot: P2, job_id: job.clone() });
    assert_eq!(sim.hit_on(P1, "p1-old"), Action::DiscardStale { slot: P1, job_id: "p1-old".into() });
    assert_eq!(sim.hit_on(SlotId(3), &job), Action::DiscardStale { slot: SlotId(3), job_id: job.clone() });
    assert_eq!(sim.hit_on(P1, &job), Action::Submit { slot: P1, job_id: job.clone() });
    // A new job supersedes the old one.
    sim.run_for_secs(31);
    assert_ne!(sim.state.current_job(P1), Some(job.as_str()));
    assert_eq!(sim.hit_on(P1, &job), Action::DiscardStale { slot: P1, job_id: job });
}

// ---------------------------------------------------------------------------------------------
// EOF and stalls
// ---------------------------------------------------------------------------------------------

#[test]
fn eof_after_60s_active_reconnects_to_the_same_pool_once() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(100));
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 99 });
    assert_eq!(sim.manager(), ManagerState::FailingOver { from: P1, to: P1 });
    assert!(!sim.gpu);
    sim.run_for_secs(1);
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0), secs(100)]);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.count(is_resolve(P2)), 0);
    // Dropping again shortly after the reconnect is a real failure.
    sim.run_until(secs(110));
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 9 });
    sim.run_for_secs(1);
    assert_eq!(sim.first(is_resolve(P2)), Some(secs(110)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn eof_after_60s_with_failed_reconnect_fails_over() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(100));
    sim.pools[0].net = Net::Refuse;
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 99 });
    sim.run_for_secs(1);
    assert_eq!(sim.first(is_resolve(P2)), Some(ms(100_100)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn eof_before_60s_fails_over_immediately() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(30));
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 29 });
    assert_eq!(sim.manager(), ManagerState::FailingOver { from: P1, to: P2 });
    sim.run_for_secs(1);
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0)]);
    assert_eq!(sim.first(is_resolve(P2)), Some(secs(30)));
    assert_eq!(sim.manager(), mining(P2));
}

#[test]
fn stall_soft_reconnects_first_then_fails_over() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(100));
    sim.pools[0].job_every_ms = None; // the pool goes quiet; last job at 90.25 s
    sim.run_until(secs(990));
    assert_eq!(sim.manager(), mining(P1));
    sim.run_until(secs(991));
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0), ms(990_250)]);
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P1 }), Some(ms(990_250)));
    assert_eq!(sim.manager(), mining(P1), "soft reconnect brought P1 back");
    assert_eq!(sim.count(is_resolve(P2)), 0);
    // Still no jobs after the reconnect's first one (990.5 s): the next stall fails over.
    sim.run_until(secs(1891));
    assert_eq!(sim.first(is_resolve(P2)), Some(ms(1_890_500)));
    assert_eq!(sim.manager(), mining(P2));
}

// ---------------------------------------------------------------------------------------------
// Failback, switching and pinning
// ---------------------------------------------------------------------------------------------

#[test]
fn failback_waits_for_the_300s_probe_and_60s_of_stable_health() {
    let mut sim = Sim::new(pools(2), vec![FakePool::refusing()]);
    sim.start();
    sim.run_until(secs(1));
    assert_eq!(sim.activated(P2, Instant::ZERO), Some(ms(350)));
    sim.run_until(secs(400));
    sim.pools[0].net = Net::Ok; // pool 1 comes back after the first probe failed
    sim.run_until(secs(630));
    let job = sim.state.current_job(P2).expect("job").to_string();
    assert_eq!(sim.hit(), Action::Submit { slot: P2, job_id: job }, "the standby probe never mines");
    sim.run_until(ms(660_599));
    assert_eq!(sim.manager(), mining(P2), "60 s hysteresis");
    assert_eq!(sim.slot(P1), SlotState::Standby);
    sim.run_until(secs(700));
    assert_eq!(sim.times(is_probe(P1)), vec![ms(300_350), ms(600_350)]);
    assert_eq!(sim.activated(P1, secs(1)), Some(ms(660_600)));
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P2 }), Some(ms(665_600)));
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.count(|a| *a == Action::StopGpu), 0, "the switch never idles the GPU");
    // Now on the best slot: no more probes.
    sim.run_until(secs(2000));
    assert_eq!(sim.count(is_probe(P1)), 2);
    assert_eq!(sim.count(is_probe(P2)), 0);
}

#[test]
fn unstable_probe_does_not_fail_back() {
    let mut sim = Sim::new(pools(2), vec![FakePool::refusing()]);
    sim.start();
    sim.run_until(secs(250));
    sim.pools[0].net = Net::Ok;
    sim.run_until(secs(330));
    assert_eq!(sim.slot(P1), SlotState::Standby);
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 0 });
    assert!(matches!(sim.slot(P1), SlotState::Backoff { .. }));
    sim.run_until(secs(659));
    assert_eq!(sim.manager(), mining(P2));
    assert_eq!(sim.activated(P1, secs(1)), None);
    sim.run_until(secs(700));
    assert_eq!(sim.times(is_probe(P1)), vec![ms(300_350), ms(600_350)]);
    assert_eq!(sim.activated(P1, secs(1)), Some(ms(660_600)));
}

#[test]
fn failover_during_pending_submit_discards_hit_for_old_job() {
    let p1 = FakePool { share: Share::NoAck, ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![p1]);
    sim.start();
    sim.run_until(secs(10));
    let old = sim.state.current_job(P1).expect("job").to_string();
    assert_eq!(sim.hit(), Action::Submit { slot: P1, job_id: old.clone() });
    assert_eq!(sim.state.pending_acks(P1), 1);
    sim.send(Event::Disconnected { slot: P1, was_active_for_s: 9 });
    assert_eq!(sim.state.pending_acks(P1), 0);
    // The attempt that was in flight finishes after the switch: its hit is discarded.
    assert_eq!(sim.hit_on(P1, &old), Action::DiscardStale { slot: P1, job_id: old.clone() });
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P2));
    assert_eq!(sim.hit_on(P2, &old), Action::DiscardStale { slot: P2, job_id: old.clone() });
    assert!(matches!(sim.hit(), Action::Submit { slot, .. } if slot == P2));
    assert_eq!(sim.count(|a| matches!(a, Action::Submit { job_id, .. } if *job_id == old)), 1);
}

#[test]
fn planned_switch_drains_in_flight_hit_then_discards() {
    // Pool 1 is back at 250 s: probe at 300.35 s, standby at 300.6 s, switch at 360.6 s.
    let p2 = FakePool { share: Share::NoAck, ..FakePool::default() };
    let mut sim = Sim::new(pools(2), vec![FakePool::refusing(), p2]);
    sim.start();
    sim.run_until(secs(250));
    sim.pools[0].net = Net::Ok;
    // Pool 2 jobs arrive at 0.35 + 30k s; take the one of 360.35 s, just before the switch.
    sim.run_until(ms(360_400));
    let old = sim.state.current_job(P2).expect("job").to_string();
    assert_eq!(sim.hit(), Action::Submit { slot: P2, job_id: old.clone() });
    sim.run_until(ms(361_000));
    assert_eq!(sim.activated(P1, Instant::ZERO), Some(ms(360_600)));
    assert_eq!(sim.manager(), mining(P1));
    assert!(matches!(sim.slot(P2), SlotState::Draining { .. }));
    // Hit from the attempt that was in flight at the boundary: submitted on the old session.
    assert_eq!(sim.hit_on(P2, &old), Action::Submit { slot: P2, job_id: old.clone() });
    assert_eq!(sim.state.pending_acks(P2), 2);
    // Never on the new session.
    assert_eq!(sim.hit_on(P1, &old), Action::DiscardStale { slot: P1, job_id: old.clone() });
    sim.run_until(ms(365_600));
    assert_eq!(sim.slot(P2), SlotState::Idle);
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P2 }), Some(ms(365_600)));
    assert_eq!(sim.hit_on(P2, &old), Action::DiscardStale { slot: P2, job_id: old });
}

#[test]
fn user_switch_is_make_before_break_and_pins() {
    let mut sim = Sim::new(pools(3), vec![]);
    sim.start();
    sim.run_until(secs(10));
    sim.send(Event::UserSwitch { slot: P3 });
    assert_eq!(sim.state.pin(), Some(P3));
    assert_eq!(sim.manager(), mining(P1), "P1 keeps mining while P3 comes up");
    sim.run_until(secs(11));
    assert_eq!(sim.first(is_probe(P3)), Some(secs(10)));
    assert_eq!(sim.activated(P3, Instant::ZERO), Some(ms(10_250)));
    assert!(matches!(sim.slot(P1), SlotState::Draining { .. }));
    sim.run_until(secs(16));
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P1 }), Some(ms(15_250)));
    assert_eq!(sim.count(|a| *a == Action::StopGpu), 0);
    // Pinned: no failback to pool 1.
    sim.run_until(secs(2000));
    assert_eq!(sim.manager(), mining(P3));
    assert_eq!(sim.count(is_probe(P1)), 0);
}

#[test]
fn user_pin_prevents_failback_until_unpinned() {
    let mut sim = Sim::new(pools(2), vec![FakePool::refusing()]);
    sim.start();
    sim.run_until(secs(1));
    sim.send(Event::UserPin { slot: Some(P2) });
    sim.run_until(secs(10));
    sim.pools[0].net = Net::Ok;
    sim.run_until(secs(1000));
    assert_eq!(sim.manager(), mining(P2));
    assert_eq!(sim.count(is_probe(P1)), 0);
    sim.send(Event::UserPin { slot: None });
    sim.run_until(secs(1400));
    assert_eq!(sim.times(is_probe(P1)), vec![secs(1300)]);
    assert_eq!(sim.activated(P1, secs(1)), Some(ms(1_360_250)));
    assert_eq!(sim.manager(), mining(P1));
}

#[test]
fn pinned_slot_is_the_failback_home_after_a_failure() {
    let mut sim = Sim::new(pools(3), vec![]);
    sim.start();
    sim.run_until(secs(5));
    sim.send(Event::UserPin { slot: Some(P2) });
    sim.run_until(secs(6));
    assert_eq!(sim.manager(), mining(P2));
    sim.run_until(secs(20));
    sim.pools[1].net = Net::Refuse;
    sim.send(Event::Disconnected { slot: P2, was_active_for_s: 14 });
    sim.run_for_secs(1);
    // Automatic failover still works while pinned: next after P2 is P3.
    assert_eq!(sim.manager(), mining(P3));
    sim.pools[1].net = Net::Ok;
    sim.run_until(secs(400));
    assert_eq!(sim.manager(), mining(P2), "fails back to the pinned slot, not to pool 1");
    assert_eq!(sim.count(is_probe(P1)), 0);
}

#[test]
fn config_edit_while_failing_over_takes_effect_immediately() {
    let mut sim = Sim::new(pools(3), vec![FakePool::refusing(), FakePool { net: Net::Blackhole, ..FakePool::default() }]);
    sim.start();
    sim.run_until(secs(2));
    assert_eq!(sim.manager(), ManagerState::FailingOver { from: P1, to: P2 });
    assert_eq!(sim.slot(P2), SlotState::Connecting);
    // The user fixes pool 1's port in the GUI.
    let mut cfg = pools(3);
    cfg[0].port = 3333;
    sim.pools[0].net = Net::Ok;
    sim.send(Event::ConfigChanged { pools: cfg });
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P2 }), Some(secs(2)));
    assert_eq!(sim.state.failures(P1), 0);
    assert_eq!(sim.state.failures(P2), 0, "cancelled, not penalized");
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.activated(P1, Instant::ZERO), Some(ms(2_250)));
    assert_eq!(sim.state.slot_config(P1).map(|c| c.port), Some(3333));
}

#[test]
fn config_edit_of_the_active_slot_reconnects_it() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(50));
    let mut cfg = pools(2);
    cfg[0].login = "prl1newwallet.rig".into();
    sim.send(Event::ConfigChanged { pools: cfg });
    assert_eq!(sim.first(|a| *a == Action::Close { slot: P1 }), Some(secs(50)));
    sim.run_for_secs(1);
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0), secs(50)]);
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.count(is_resolve(P2)), 0);
}

// ---------------------------------------------------------------------------------------------
// Pauses and certificate versions
// ---------------------------------------------------------------------------------------------

#[test]
fn cert_version_4_pauses_with_update_required() {
    let mut sim = Sim::new(pools(3), vec![]);
    sim.start();
    sim.run_until(secs(10));
    assert!(sim.gpu);
    sim.pools[0].cert_version = Some(4); // the network forked; next job is V4
    sim.run_until(secs(31));
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::UnsupportedScheme });
    assert!(!sim.gpu);
    assert!(sim.alerts().iter().any(|m| m.contains(UPDATE_REQUIRED_ALERT)));
    assert!(sim.state.is_unsupported(P1));
    assert!(SlotId::all().all(|s| !sim.slot(s).is_open()));
    // Never an invalid share: no other pool is tried, nothing is submitted.
    sim.run_until(secs(3600));
    assert_eq!(sim.count(is_resolve(P2)) + sim.count(is_resolve(P3)), 0);
    assert_eq!(sim.count(|a| matches!(a, Action::Submit { .. })), 0);
    // Resuming without an update pauses again on the first V4 job.
    sim.send(Event::ResumeRequest);
    sim.run_for_secs(1);
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::UnsupportedScheme });
}

#[test]
fn cert_version_omitted_pauses_with_update_required() {
    let mut sim = Sim::new(pools(3), vec![]);
    sim.start();
    sim.run_until(secs(10));
    assert!(sim.gpu);
    sim.pools[0].cert_version = None; // the pool omits cert_version; treat as unsupported
    sim.run_until(secs(31));
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::UnsupportedScheme });
    assert!(!sim.gpu);
    assert!(sim.alerts().iter().any(|m| m.contains(UPDATE_REQUIRED_ALERT)));
    assert!(sim.state.is_unsupported(P1));
    assert!(SlotId::all().all(|s| !sim.slot(s).is_open()));
}

#[test]
fn cert_version_4_on_a_probe_only_disables_that_slot() {
    let mut sim = Sim::new(pools(2), vec![FakePool::refusing()]);
    sim.start();
    sim.run_until(secs(100));
    sim.pools[0].net = Net::Ok;
    sim.pools[0].cert_version = Some(4);
    sim.run_until(secs(301));
    assert_eq!(sim.manager(), mining(P2), "pool 2 still serves V3 jobs");
    assert!(sim.state.is_unsupported(P1));
    assert!(sim.alerts().iter().any(|m| m.contains(UPDATE_REQUIRED_ALERT)));
    sim.run_until(secs(1000));
    assert_eq!(sim.times(is_probe(P1)), vec![ms(300_350)]);
}

#[test]
fn yield_pause_keeps_sessions_and_resume_restarts_the_gpu() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(10));
    let acts = sim.send(Event::PauseRequest { reason: PauseReason::Yield });
    assert_eq!(acts.first(), Some(&Action::StopGpu));
    assert!(!acts.iter().any(|a| matches!(a, Action::Close { .. })));
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::Yield });
    assert_eq!(sim.slot(P1), SlotState::Active);
    sim.run_until(secs(100));
    // Jobs keep flowing on the open session; a hit from the last attempt is still valid.
    let job = sim.state.current_job(P1).expect("job").to_string();
    assert_ne!(job, "p1-j1");
    assert!(matches!(sim.hit_on(P1, &job), Action::Submit { .. }));
    let acts = sim.send(Event::ResumeRequest);
    assert!(acts.contains(&Action::StartGpu));
    assert_eq!(sim.manager(), mining(P1));
    assert_eq!(sim.count(|a| matches!(a, Action::Resolve { .. })), 1, "same session, no reconnect");
    assert_eq!(sim.count(|a| matches!(a, Action::Close { .. })), 0);
}

#[test]
fn user_stop_closes_sessions_and_resume_reconnects() {
    let mut sim = Sim::new(pools(2), vec![]);
    sim.start();
    sim.run_until(secs(10));
    sim.send(Event::PauseRequest { reason: PauseReason::UserStop });
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::UserStop });
    assert_eq!(sim.slot(P1), SlotState::Idle);
    assert!(!sim.gpu);
    // A Yield request cannot weaken a stop.
    sim.send(Event::PauseRequest { reason: PauseReason::Yield });
    assert_eq!(sim.manager(), ManagerState::Paused { reason: PauseReason::UserStop });
    sim.run_until(secs(500));
    assert_eq!(sim.count(|a| matches!(a, Action::Resolve { .. })), 1);
    sim.send(Event::ResumeRequest);
    sim.run_for_secs(1);
    assert_eq!(sim.times(is_resolve(P1)), vec![ms(0), secs(500)]);
    assert_eq!(sim.manager(), mining(P1));
    assert!(sim.gpu);
}

#[test]
fn schedule_tick_tracks_the_earliest_deadline() {
    let mut sim = Sim::new(pools(1), vec![]);
    sim.start();
    assert_eq!(sim.log[0].2.last(), Some(&Action::ScheduleTick { at: secs(10) }), "resolve timeout");
    sim.run_until(secs(1));
    // Active since 250 ms: the next deadline is the stall check of the latest job.
    assert_eq!(sim.state.next_deadline(), Some(ms(250).plus_secs(900)));
}
