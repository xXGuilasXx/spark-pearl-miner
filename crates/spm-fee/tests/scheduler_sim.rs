//! Deterministic simulations of the fee scheduler at 1 s ticks (public API only, as the daemon
//! uses it). Rolling 24 h windows are measured over *active* mining time (user + dev hashing),
//! which is what FEE_BPS is defined on; paused/yield/idle seconds are not part of any window.

use spm_fee::{
    AbortReason, Activity, FeeAction, FeePhase, FeeScheduler, PersistedFeeState, CATCHUP_DEBT_SECS,
    DEBT_CAP_SECS, DEBT_DEN, DEBT_NUM, DEV_AUTH_TIMEOUT_SECS, DEV_RETRY_SECS, DEV_WALLET, FEE_BPS,
    FEE_WINDOW_SECS, FIRST_SLICE_MAX_SECS, FIRST_SLICE_MIN_SECS, MIN_SLICE_GAP_SECS, MIN_SLICE_SECS,
    PREWARM_SECS, SLICE_SECS, SUSPEND_SECS,
};

const DAY: u64 = 86_400;
const HOUR: u64 = 3_600;
const USER: &str = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh";
const T0: u64 = 1_790_000_000; // arbitrary UNIX-like start

/// How the dev pool answers a PreWarm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth {
    Ok,
    Fail,
    Silent,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, p_per_million: u64) -> bool {
        self.below(1_000_000) < p_per_million
    }
}

fn debt_units(st: &PersistedFeeState) -> u64 {
    st.debt_secs * DEBT_DEN + st.debt_frac
}

/// Drives a scheduler like the daemon's work arbiter and records everything.
struct Sim {
    s: FeeScheduler,
    now: u64,
    dev_on: bool,
    /// One entry per active second, true = dev.
    timeline: Vec<bool>,
    prewarms: Vec<u64>,
    starts: Vec<u64>,
    /// StartSlice time minus the PreWarm that preceded it.
    leads: Vec<u64>,
    /// Dev seconds of every slice that hashed at all.
    slice_lens: Vec<u64>,
    /// (StartSlice time, dev seconds) of the same slices.
    slices: Vec<(u64, u64)>,
    /// User seconds hashed between the end of a slice and the start of the next one.
    gaps: Vec<u64>,
    aborts: Vec<(u64, AbortReason)>,
    cur_start: u64,
    cur_len: u64,
    user_since: u64,
    had_slice: bool,
    last_prewarm: Option<u64>,
    max_debt_secs: f64,
}

impl Sim {
    fn new(s: FeeScheduler, now: u64) -> Self {
        Sim {
            s,
            now,
            dev_on: false,
            timeline: Vec::new(),
            prewarms: Vec::new(),
            starts: Vec::new(),
            leads: Vec::new(),
            slice_lens: Vec::new(),
            slices: Vec::new(),
            gaps: Vec::new(),
            aborts: Vec::new(),
            cur_start: 0,
            cur_len: 0,
            user_since: 0,
            had_slice: false,
            last_prewarm: None,
            max_debt_secs: 0.0,
        }
    }

    fn fresh(seed: u64) -> Self {
        let mut sim = Sim::new(FeeScheduler::new(seed, USER), T0);
        sim.s.on_mining_started(T0);
        sim
    }

    fn close_slice(&mut self) {
        if self.dev_on {
            self.dev_on = false;
            if self.cur_len > 0 {
                self.slice_lens.push(self.cur_len);
                self.slices.push((self.cur_start, self.cur_len));
                self.had_slice = true;
                self.user_since = 0;
            }
        }
    }

    fn handle(&mut self, auth: Auth) {
        let mut guard = 0;
        while let Some(a) = self.s.poll(self.now) {
            guard += 1;
            assert!(guard < 10, "poll does not settle at {}", self.now);
            match a {
                FeeAction::PreWarm => {
                    assert!(!self.dev_on, "PreWarm during a slice");
                    self.prewarms.push(self.now);
                    self.last_prewarm = Some(self.now);
                    match auth {
                        Auth::Ok => self.s.on_dev_authorized(self.now),
                        Auth::Fail => self.s.on_dev_authorize_failed(self.now),
                        Auth::Silent => {}
                    }
                }
                FeeAction::StartSlice => {
                    assert!(!self.dev_on, "StartSlice twice");
                    let pw = self.last_prewarm.take().expect("StartSlice without PreWarm");
                    self.leads.push(self.now - pw);
                    self.starts.push(self.now);
                    if self.had_slice {
                        self.gaps.push(self.user_since);
                    }
                    self.dev_on = true;
                    self.cur_start = self.now;
                    self.cur_len = 0;
                }
                FeeAction::EndSlice => {
                    assert!(self.dev_on, "EndSlice outside a slice");
                    self.close_slice();
                }
                FeeAction::Abort(r) => {
                    self.aborts.push((self.now, r));
                    self.last_prewarm = None;
                    self.close_slice();
                }
            }
        }
    }

    /// One second: settle the scheduler, then hash (or not) for one second.
    fn tick(&mut self, activity: Activity, auth: Auth) {
        self.handle(auth);
        match activity {
            Activity::UserHashing | Activity::DevHashing => {
                if self.dev_on {
                    self.s.on_activity(Activity::DevHashing, 1);
                    self.timeline.push(true);
                    self.cur_len += 1;
                } else {
                    self.s.on_activity(Activity::UserHashing, 1);
                    self.timeline.push(false);
                    self.user_since += 1;
                }
            }
            other => self.s.on_activity(other, 1),
        }
        self.max_debt_secs = self.max_debt_secs.max(self.s.stats().debt_secs);
        self.now += 1;
    }

    fn run(&mut self, secs: u64, auth: Auth) {
        for _ in 0..secs {
            self.tick(Activity::UserHashing, auth);
        }
    }

    fn state(&self) -> PersistedFeeState {
        self.s.persisted()
    }

    /// dev + debt == accrued, exactly (valid while neither the cap nor a suspension interfered).
    fn assert_conservation(&self) {
        let st = self.state();
        assert_eq!(
            st.dev_hash_secs * DEBT_DEN + debt_units(&st),
            st.user_hash_secs * DEBT_NUM,
            "paid + owed must equal exactly 200/9800 of user time"
        );
    }

    /// Every window of active time (the first day's growing prefixes and every full 24 h window
    /// after that) has dev time at or below FEE_BPS. Returns the worst full-window dev seconds.
    fn assert_windows_at_most_fee(&self) -> u64 {
        let w = FEE_WINDOW_SECS as usize;
        let (mut dev, mut worst) = (0u64, 0u64);
        for i in 0..self.timeline.len() {
            if self.timeline[i] {
                dev += 1;
            }
            if i >= w && self.timeline[i - w] {
                dev -= 1;
            }
            let len = (i + 1).min(w) as u64;
            assert!(
                dev * 10_000 <= u64::from(FEE_BPS) * len,
                "active window ending at second {} holds {dev} s of dev time in {len} s",
                i + 1
            );
            if len == FEE_WINDOW_SECS {
                worst = worst.max(dev);
            }
        }
        worst
    }
}

fn pct(st: &PersistedFeeState) -> f64 {
    st.dev_hash_secs as f64 * 100.0 / (st.user_hash_secs + st.dev_hash_secs) as f64
}

// (1) 30 days at 1 s ticks, continuous hashing: 2.00 % ± 0.02 %.
#[test]
fn thirty_days_continuous_is_two_percent() {
    let mut sim = Sim::fresh(7);
    sim.run(30 * DAY, Auth::Ok);
    let st = sim.state();
    let measured = pct(&st);
    let stats_pct = sim.s.stats().measured_fee_pct;
    eprintln!(
        "30 d continuous: measured {measured:.4} %, slices {}, lens {}..{}, max debt {:.1} s",
        sim.slice_lens.len(),
        sim.slice_lens.iter().min().unwrap(),
        sim.slice_lens.iter().max().unwrap(),
        sim.max_debt_secs
    );
    assert!((measured - 2.0).abs() <= 0.02, "measured fee {measured:.4} %");
    assert!((stats_pct - measured).abs() < 1e-9);
    assert_eq!(st.user_hash_secs + st.dev_hash_secs, 30 * DAY);
    sim.assert_conservation();
    let worst = sim.assert_windows_at_most_fee();
    assert!(worst <= FEE_WINDOW_SECS * u64::from(FEE_BPS) / 10_000);
    assert!(sim.slice_lens.iter().all(|&l| l <= SLICE_SECS));
    assert!(sim.aborts.is_empty());
    // (9) every StartSlice came exactly PREWARM_SECS after its PreWarm.
    assert!(sim.leads.iter().all(|&l| l == PREWARM_SECS), "leads {:?}", sim.leads);
    assert_eq!(sim.prewarms.len(), sim.starts.len());
    // No burst: slices are always separated by at least MIN_SLICE_GAP_SECS of user hashing.
    assert!(sim.gaps.iter().all(|&g| g >= MIN_SLICE_GAP_SECS));
    // The strict 24 h regime held all month.
    assert!(sim.max_debt_secs <= CATCHUP_DEBT_SECS as f64);
}

fn random_pause(rng: &mut Rng) -> (Activity, u64) {
    let kind = match rng.below(3) {
        0 => Activity::Yielding,
        1 => Activity::Idle,
        _ => Activity::Paused,
    };
    (kind, 60 + rng.below(6 * HOUR))
}

// (2) Random pauses: every rolling 24 h window ≤ 2.0 %, total = 2.00 % of active time, and
// nothing accrues while not hashing.
#[test]
fn random_pauses_keep_every_window_at_or_below_two_percent() {
    for seed in [2u64, 20, 200] {
        random_pauses_run(seed);
    }
}

fn random_pauses_run(seed: u64) {
    let mut rng = Rng(0x5eed_0002 ^ seed);
    let mut sim = Sim::fresh(seed);
    let mut pauses = 0;
    while sim.now < T0 + 45 * DAY {
        // On average one pause every 3 h of wall time.
        if rng.chance(1_000_000 / (3 * HOUR)) {
            let (kind, len) = random_pause(&mut rng);
            let before = sim.state();
            for _ in 0..len {
                sim.tick(kind, Auth::Ok);
            }
            let after = sim.state();
            assert_eq!(debt_units(&before), debt_units(&after), "debt moved during a pause");
            assert_eq!(before.user_hash_secs, after.user_hash_secs);
            assert_eq!(before.dev_hash_secs, after.dev_hash_secs);
            pauses += 1;
        } else {
            sim.tick(Activity::UserHashing, Auth::Ok);
        }
    }
    let st = sim.state();
    let measured = pct(&st);
    eprintln!(
        "seed {seed}, 45 d with {pauses} pauses: active {} s, measured {measured:.4} %, slices {}, max debt {:.1} s",
        sim.timeline.len(),
        sim.slice_lens.len(),
        sim.max_debt_secs
    );
    assert!(pauses > 100);
    assert!(sim.timeline.len() as u64 > 15 * DAY);
    assert!((measured - 2.0).abs() <= 0.02, "measured fee {measured:.4} %");
    sim.assert_conservation();
    sim.assert_windows_at_most_fee();
    assert!(sim.leads.iter().all(|&l| l >= PREWARM_SECS));
    assert!(sim.gaps.iter().all(|&g| g >= MIN_SLICE_GAP_SECS));
}

// (2b) Pauses plus isolated dev failures (refused logins, silent pools, dropped sessions):
// the 24 h ceiling still holds and nothing is paid twice.
#[test]
fn failure_injection_keeps_every_window_at_or_below_two_percent() {
    for seed in [3u64, 30, 300] {
        failure_injection_run(seed);
    }
}

fn failure_injection_run(seed: u64) {
    let mut rng = Rng(0x5eed_0b0b ^ seed);
    let mut sim = Sim::fresh(seed);
    while sim.now < T0 + 21 * DAY {
        if rng.chance(1_000_000 / (4 * HOUR)) {
            let (kind, len) = random_pause(&mut rng);
            for _ in 0..len {
                sim.tick(kind, Auth::Ok);
            }
            continue;
        }
        let auth = match rng.below(100) {
            0..=9 => Auth::Fail,
            10..=14 => Auth::Silent,
            _ => Auth::Ok,
        };
        // About 5 % of slices lose the dev session somewhere in the middle.
        if sim.s.in_slice() && rng.chance(400) {
            sim.s.on_dev_session_lost(sim.now);
        }
        if sim.s.in_slice() && rng.chance(30_000) {
            let accepted = rng.below(100) < 97;
            sim.s.on_dev_share(accepted, sim.now);
        }
        sim.tick(Activity::UserHashing, auth);
    }
    let st = sim.state();
    let reasons: Vec<_> = sim.aborts.iter().map(|a| a.1).collect();
    eprintln!(
        "seed {seed}, 21 d with failures: measured {:.4} %, slices {}, aborts {}, max debt {:.1} s",
        pct(&st),
        sim.slice_lens.len(),
        reasons.len(),
        sim.max_debt_secs
    );
    assert!(reasons.contains(&AbortReason::AuthorizeFailed));
    assert!(reasons.contains(&AbortReason::AuthorizeTimeout));
    assert!(reasons.contains(&AbortReason::SessionLost));
    assert!(sim.s.fee_suspended_until().is_none());
    sim.assert_conservation();
    // Isolated failures never build a backlog, so the strict 24 h regime holds throughout.
    assert!(sim.max_debt_secs <= CATCHUP_DEBT_SECS as f64, "max debt {:.1} s", sim.max_debt_secs);
    sim.assert_windows_at_most_fee();
    assert!(pct(&st) <= 2.0 + 1e-9);
    assert!(sim.gaps.iter().all(|&g| g >= MIN_SLICE_GAP_SECS));
}

// (3) Restart with accrued debt: state restored, no debt lost, no double payment.
#[test]
fn restart_keeps_debt_and_never_pays_twice() {
    // (a) Before the first slice.
    let mut sim = Sim::fresh(11);
    sim.run(50 * 60, Auth::Ok);
    let saved = sim.state().to_json().expect("serialize");
    assert!(!saved.contains(DEV_WALLET) && !saved.contains(USER));
    let st = PersistedFeeState::from_json(&saved).expect("parse");
    assert_eq!(st, sim.state());
    assert_eq!(debt_units(&st), 50 * 60 * DEBT_NUM);
    sim.now += 10 * 60; // daemon down for 10 min
    sim.s = FeeScheduler::restore(st.clone(), USER);
    assert_eq!(sim.s.persisted().debt_secs, st.debt_secs);
    assert_eq!(sim.s.persisted().debt_frac, st.debt_frac);
    sim.s.on_mining_started(sim.now);
    sim.run(DAY, Auth::Ok);
    sim.assert_conservation();

    // (b) In the middle of a slice.
    while sim.dev_on {
        sim.tick(Activity::UserHashing, Auth::Ok);
    }
    let before_slice = loop {
        let st = sim.state();
        sim.tick(Activity::UserHashing, Auth::Ok);
        if sim.dev_on {
            break st;
        }
    };
    // Interrupt it before its end (a slice is never shorter than MIN_SLICE_SECS).
    let part = MIN_SLICE_SECS - 5;
    while sim.cur_len < part {
        sim.tick(Activity::UserHashing, Auth::Ok);
    }
    assert!(sim.dev_on);
    let mid = sim.state();
    assert_eq!(mid.dev_hash_secs, before_slice.dev_hash_secs + part);
    assert_eq!(debt_units(&mid), debt_units(&before_slice) - part * DEBT_DEN);
    // Crash: the slice is gone; restart 30 s later.
    sim.close_slice();
    sim.now += 30;
    sim.s = FeeScheduler::restore(PersistedFeeState::from_json(&mid.to_json().expect("json")).expect("json"), USER);
    assert_eq!(debt_units(&sim.s.persisted()), debt_units(&mid), "debt lost across restart");
    sim.s.on_mining_started(sim.now);
    let starts_before = sim.starts.len();
    let restart_at = sim.now;
    sim.run(3 * DAY, Auth::Ok);
    // The unpaid rest of the interrupted slice is paid later, never on top of a fresh slice
    // right away: the next slice comes after the first-slice delay and the minimum gap.
    let next = sim.starts[starts_before];
    assert!(next >= restart_at + FIRST_SLICE_MIN_SECS.max(MIN_SLICE_GAP_SECS), "slice {} s after restart", next - restart_at);
    sim.assert_conservation();
    sim.assert_windows_at_most_fee();
    let st = sim.state();
    assert!(st.dev_hash_secs * DEBT_DEN <= st.user_hash_secs * DEBT_NUM);
    assert!(debt_units(&st) <= CATCHUP_DEBT_SECS * DEBT_DEN);
}

// (4) Dev pool unreachable for 10 h: the user keeps mining, debt stays under the cap, and the
// backlog is repaid afterwards in normal slices separated by MIN_SLICE_GAP_SECS (no burst).
#[test]
fn ten_hour_dev_outage_is_repaid_without_a_burst() {
    let mut sim = Sim::fresh(4);
    sim.run(DAY, Auth::Ok);
    let before = sim.state();
    let (slices_before, aborts_before) = (sim.slice_lens.len(), sim.aborts.len());
    sim.run(10 * HOUR, Auth::Fail);
    let during = sim.state();
    assert_eq!(sim.slice_lens.len(), slices_before, "no slice may run while the dev pool is down");
    assert_eq!(during.user_hash_secs - before.user_hash_secs, 10 * HOUR, "the user kept mining every second");
    assert_eq!(during.dev_hash_secs, before.dev_hash_secs);
    let retries = sim.aborts.len() - aborts_before;
    // One login attempt per DEV_RETRY_SECS once a slice is due (the first may wait for the debt).
    let due_after = SLICE_SECS * DEBT_DEN / DEBT_NUM;
    assert!(retries as u64 >= (10 * HOUR - due_after) / DEV_RETRY_SECS, "retries {retries}");
    assert!(sim.aborts[aborts_before..].iter().all(|a| a.1 == AbortReason::AuthorizeFailed));
    let backlog = debt_units(&during) as f64 / DEBT_DEN as f64;
    eprintln!("after 10 h outage: debt {backlog:.1} s, {retries} failed logins");
    assert!(backlog > 10.0 * HOUR as f64 * 2.0 / 98.0 && backlog <= DEBT_CAP_SECS as f64);
    // Recovery.
    let first_after = sim.slices.len();
    sim.run(2 * DAY, Auth::Ok);
    let after = sim.state();
    let repaid: Vec<_> = sim.slices[first_after..].to_vec();
    eprintln!(
        "recovery: {} slices, debt now {:.1} s, catch-up lens {:?}",
        repaid.len(),
        debt_units(&after) as f64 / DEBT_DEN as f64,
        repaid.iter().take(8).map(|s| s.1).collect::<Vec<_>>()
    );
    sim.assert_conservation();
    assert!(debt_units(&after) <= CATCHUP_DEBT_SECS * DEBT_DEN, "backlog not repaid");
    assert!(repaid.iter().all(|s| s.1 <= SLICE_SECS));
    // The backlog is paid in full-length slices, never merged into a burst.
    let catch_up: Vec<_> = repaid.iter().take(4).collect();
    assert!(catch_up.iter().all(|s| s.1 == SLICE_SECS), "catch-up slices {catch_up:?}");
    assert!(sim.gaps.iter().all(|&g| g >= MIN_SLICE_GAP_SECS));
    for pair in repaid.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= pair[0].1 + MIN_SLICE_GAP_SECS);
    }
}

// (4b) A multi-day outage hits the cap exactly; the cap is then repaid in normal slices.
#[test]
fn long_outage_is_capped_and_repaid_in_normal_slices() {
    let mut sim = Sim::fresh(44);
    sim.run(3 * HOUR, Auth::Ok);
    sim.run(3 * DAY, Auth::Silent);
    let st = sim.state();
    assert_eq!((st.debt_secs, st.debt_frac), (DEBT_CAP_SECS, 0), "debt must stop at the cap");
    assert!(sim.aborts.iter().all(|a| a.1 == AbortReason::AuthorizeTimeout));
    assert_eq!(st.user_hash_secs + st.dev_hash_secs, 3 * HOUR + 3 * DAY);
    let first_after = sim.slices.len();
    let recovery_start = sim.now;
    let mut repaid_at = None;
    for _ in 0..(2 * DAY) {
        sim.tick(Activity::UserHashing, Auth::Ok);
        if repaid_at.is_none() && sim.state().debt_secs <= CATCHUP_DEBT_SECS {
            repaid_at = Some(sim.now);
        }
    }
    let repaid_at = repaid_at.expect("cap never repaid");
    let slices = &sim.slices[first_after..];
    eprintln!(
        "cap repaid in {:.1} h with {} slices",
        (repaid_at - recovery_start) as f64 / HOUR as f64,
        slices.iter().filter(|s| s.0 < repaid_at).count()
    );
    assert!(slices.iter().filter(|s| s.0 < repaid_at).all(|s| s.1 == SLICE_SECS));
    assert!(slices.iter().all(|s| s.1 <= SLICE_SECS));
    assert!(sim.gaps.iter().all(|&g| g >= MIN_SLICE_GAP_SECS));
    // No burst: at most one slice per MIN_SLICE_GAP_SECS + SLICE_SECS of wall time.
    for pair in slices.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= pair[0].1 + MIN_SLICE_GAP_SECS);
    }
    // Repaying 3600 s at one 120 s slice per 32 min takes most of a day, not minutes.
    assert!(repaid_at - recovery_start > 12 * HOUR);
}

/// Runs until the next PreWarm; returns its time.
fn until_prewarm(sim: &mut Sim, auth: Auth, limit: u64) -> u64 {
    let n = sim.prewarms.len();
    for _ in 0..limit {
        sim.tick(Activity::UserHashing, auth);
        if sim.prewarms.len() > n {
            return sim.prewarms[n];
        }
    }
    panic!("no PreWarm within {limit} s");
}

// (5) A refused dev login aborts the slice without any fee time; the debt is kept.
#[test]
fn authorize_failure_aborts_without_counting_fee_time() {
    let mut s = FeeScheduler::new(5, USER);
    let mut now = T0;
    s.on_mining_started(now);
    let pw = loop {
        if let Some(a) = s.poll(now) {
            assert_eq!(a, FeeAction::PreWarm);
            break now;
        }
        s.on_user_hashing(1);
        now += 1;
    };
    let before = s.persisted();
    // Dev work reported before the login is not fee time.
    s.on_dev_hashing(5);
    s.on_activity(Activity::DevHashing, 5);
    assert_eq!(s.persisted(), before);
    for _ in 0..3 {
        now += 1;
        s.on_user_hashing(1);
        assert_eq!(s.poll(now), None);
    }
    s.on_dev_authorize_failed(now);
    assert_eq!(s.poll(now), Some(FeeAction::Abort(AbortReason::AuthorizeFailed)));
    assert_eq!(s.poll(now), None);
    let after = s.persisted();
    assert_eq!(after.dev_hash_secs, 0);
    assert_eq!(debt_units(&after), debt_units(&before) + 3 * DEBT_NUM, "debt must be kept");
    assert_eq!(after.slices_aborted, 1);
    assert!(!s.in_slice());
    // Retried after DEV_RETRY_SECS, not before.
    let mut next_pw = None;
    for _ in 0..(2 * DEV_RETRY_SECS) {
        now += 1;
        s.on_user_hashing(1);
        if s.poll(now) == Some(FeeAction::PreWarm) {
            next_pw = Some(now);
            break;
        }
    }
    let next_pw = next_pw.expect("no retry");
    assert!(next_pw + PREWARM_SECS >= pw + 3 + DEV_RETRY_SECS);
    // A pool that never answers times out the same way.
    for _ in 1..DEV_AUTH_TIMEOUT_SECS {
        now += 1;
        s.on_user_hashing(1);
        assert_eq!(s.poll(now), None);
    }
    now += 1;
    assert_eq!(s.poll(now), Some(FeeAction::Abort(AbortReason::AuthorizeTimeout)));
    assert_eq!(s.persisted().dev_hash_secs, 0);
    assert_eq!(s.stats().slices_paid, 0);
}

// (6) More than half of the last 20 dev submits rejected: fee suspended for 1 h, no accrual.
#[test]
fn reject_ratio_suspends_the_fee_for_an_hour() {
    let mut sim = Sim::fresh(6);
    until_prewarm(&mut sim, Auth::Ok, 3 * HOUR);
    while !sim.dev_on {
        sim.tick(Activity::UserHashing, Auth::Ok);
    }
    // 10 of 20 rejected is not "more than half".
    for i in 0..20 {
        sim.s.on_dev_share(i % 2 == 0, sim.now);
    }
    assert!(sim.s.fee_suspended_until().is_none());
    sim.tick(Activity::UserHashing, Auth::Ok);
    // One more reject: 11 of the last 20.
    sim.s.on_dev_share(false, sim.now);
    let until = sim.now + SUSPEND_SECS;
    assert_eq!(sim.s.fee_suspended_until(), Some(until));
    assert_eq!(sim.s.stats().fee_suspended_until, Some(until));
    let paid_before = sim.state().dev_hash_secs;
    sim.tick(Activity::UserHashing, Auth::Ok);
    assert_eq!(sim.aborts.last().map(|a| a.1), Some(AbortReason::Suspended));
    assert!(!sim.dev_on);
    assert_eq!(sim.s.stats().phase, FeePhase::Suspended);
    // Survives a restart.
    let st = sim.state();
    sim.s = FeeScheduler::restore(st, USER);
    sim.s.on_mining_started(sim.now);
    assert_eq!(sim.s.fee_suspended_until(), Some(until));
    // No accrual, no slice for the whole hour although the user keeps hashing.
    let debt = debt_units(&sim.state());
    let prewarms = sim.prewarms.len();
    while sim.now < until {
        sim.tick(Activity::UserHashing, Auth::Ok);
    }
    assert_eq!(debt_units(&sim.state()), debt, "debt accrued during the suspension");
    assert_eq!(sim.prewarms.len(), prewarms);
    assert_eq!(sim.state().dev_hash_secs, paid_before);
    // Afterwards accrual and slices resume.
    sim.run(3 * HOUR, Auth::Ok);
    assert!(sim.s.fee_suspended_until().is_none());
    assert!(sim.prewarms.len() > prewarms);
    assert!(sim.state().dev_hash_secs > paid_before);
    let st = sim.state();
    assert_eq!((st.dev_shares_accepted, st.dev_shares_rejected), (10, 11));

    // The ratio is only judged over a full window of 20 submits.
    let mut s = FeeScheduler::new(6, USER);
    for _ in 0..19 {
        s.on_dev_share(false, T0);
    }
    assert!(s.fee_suspended_until().is_none(), "judged before 20 submits");
    s.on_dev_share(false, T0);
    assert_eq!(s.fee_suspended_until(), Some(T0 + SUSPEND_SECS));
    // The window restarts after a suspension.
    s.on_dev_share(false, T0 + 1);
    assert_eq!(s.persisted().recent_dev_submits, vec![false]);
}

// (7) No fee at all when the user mines to the dev wallet.
#[test]
fn fee_disabled_when_user_wallet_is_dev_wallet() {
    for w in [DEV_WALLET.to_string(), format!("  {}  ", DEV_WALLET.to_uppercase())] {
        let mut sim = Sim::new(FeeScheduler::new(9, &w), T0);
        assert!(!sim.s.is_enabled());
        sim.s.on_mining_started(T0);
        sim.run(2 * DAY, Auth::Ok);
        sim.s.on_dev_share(false, sim.now);
        let st = sim.state();
        assert!(sim.prewarms.is_empty() && sim.starts.is_empty());
        assert_eq!((st.dev_hash_secs, st.debt_secs, st.debt_frac), (0, 0, 0));
        assert_eq!(st.user_hash_secs, 2 * DAY);
        let stats = sim.s.stats();
        assert_eq!(stats.phase, FeePhase::Disabled);
        assert_eq!(stats.measured_fee_pct, 0.0);
        assert_eq!(stats.next_slice_at, None);
    }
    assert!(FeeScheduler::new(9, USER).is_enabled());
}

// (8) The first slice point is random in [30, 90] min; it is where the first slice lands when
// the debt allows it, i.e. with debt carried over from an earlier run.
#[test]
fn first_slice_lands_in_the_random_window() {
    let mut fresh_gates = Vec::new();
    for seed in 0..32u64 {
        // Fresh install: the point is armed in the window, but 120 s of debt takes 98 min of
        // hashing, so the first slice waits for the debt.
        let mut sim = Sim::fresh(seed);
        let gate = sim.state().next_slice_at.expect("armed");
        assert!((T0 + FIRST_SLICE_MIN_SECS..=T0 + FIRST_SLICE_MAX_SECS).contains(&gate));
        fresh_gates.push(gate - T0);
        let pw = until_prewarm(&mut sim, Auth::Ok, 3 * HOUR);
        let debt_ready = T0 + SLICE_SECS * DEBT_DEN / DEBT_NUM;
        assert_eq!(pw + PREWARM_SECS, debt_ready.max(gate));

        // Run 1 of an install whose dev pool was down: debt carried into run 2.
        let mut sim = Sim::fresh(seed ^ 0xabc);
        sim.run(2 * HOUR, Auth::Fail);
        let st = sim.state();
        assert!(st.debt_secs >= SLICE_SECS);
        // Run 2, an hour later.
        let t1 = sim.now + HOUR;
        sim.now = t1;
        sim.s = FeeScheduler::restore(st, USER);
        sim.s.on_mining_started(t1);
        let gate = sim.state().next_slice_at.expect("re-armed");
        assert!((t1 + FIRST_SLICE_MIN_SECS..=t1 + FIRST_SLICE_MAX_SECS).contains(&gate), "gate {}", gate - t1);
        let pw = until_prewarm(&mut sim, Auth::Ok, 2 * HOUR);
        assert_eq!(pw, gate - PREWARM_SECS);
        while !sim.dev_on {
            sim.tick(Activity::UserHashing, Auth::Ok);
        }
        assert_eq!(*sim.starts.last().expect("started"), gate);
    }
    fresh_gates.sort_unstable();
    fresh_gates.dedup();
    assert!(fresh_gates.len() >= 30, "seeds must spread the first slice");
    let (lo, hi) = (fresh_gates[0], fresh_gates[fresh_gates.len() - 1]);
    assert!(hi - lo > (FIRST_SLICE_MAX_SECS - FIRST_SLICE_MIN_SECS) / 2, "spread {lo}..{hi}");
    // Deterministic from the seed.
    assert_eq!(Sim::fresh(3).state().next_slice_at, Sim::fresh(3).state().next_slice_at);
    // Pausing and resuming within a run does not re-arm (and so never postpones) the slice.
    let mut sim = Sim::fresh(5);
    let gate = sim.state().next_slice_at;
    sim.run(600, Auth::Ok);
    sim.s.on_mining_started(sim.now + 5_000);
    assert_eq!(sim.state().next_slice_at, gate);
}

// (9) PreWarm always precedes StartSlice by PREWARM_SECS (more only if hashing paused between).
#[test]
fn prewarm_precedes_every_slice() {
    let mut rng = Rng(9);
    let mut sim = Sim::fresh(99);
    while sim.now < T0 + 5 * DAY {
        if rng.chance(1_000_000 / 1_800) {
            let len = 1 + rng.below(40);
            for _ in 0..len {
                sim.tick(Activity::Yielding, Auth::Ok);
            }
        } else {
            sim.tick(Activity::UserHashing, Auth::Ok);
        }
    }
    assert!(sim.starts.len() > 50);
    assert_eq!(sim.leads.len(), sim.starts.len());
    assert!(sim.leads.iter().all(|&l| l >= PREWARM_SECS));
    let exact = sim.leads.iter().filter(|&&l| l == PREWARM_SECS).count();
    assert!(exact * 10 >= sim.leads.len() * 9, "most leads are exactly {PREWARM_SECS} s: {:?}", sim.leads);
    assert!(sim.leads.iter().all(|&l| l <= PREWARM_SECS + 40));
}

#[test]
fn nothing_accrues_unless_hashing_for_the_user() {
    let mut s = FeeScheduler::new(1, USER);
    s.on_mining_started(T0);
    for a in [Activity::Paused, Activity::Yielding, Activity::Idle, Activity::Benchmark, Activity::Mock, Activity::DevHashing] {
        s.on_activity(a, 10 * DAY);
    }
    let st = s.persisted();
    assert_eq!((st.debt_secs, st.debt_frac, st.user_hash_secs, st.dev_hash_secs), (0, 0, 0, 0));
    assert!(st.window.is_empty());
    s.on_activity(Activity::UserHashing, 98);
    assert_eq!((s.persisted().debt_secs, s.persisted().debt_frac), (2, 0));
}

#[test]
fn stats_report_the_accounting() {
    let mut sim = Sim::fresh(8);
    let eta = sim.s.stats().next_slice_at.expect("eta");
    assert_eq!(eta, T0 + SLICE_SECS * DEBT_DEN / DEBT_NUM);
    sim.run(DAY, Auth::Ok);
    for ok in [true, true, false] {
        sim.s.on_dev_share(ok, sim.now);
    }
    let st = sim.s.stats();
    assert!(st.enabled);
    assert_eq!(st.user_hash_secs + st.dev_hash_secs, DAY);
    assert_eq!((st.dev_shares_accepted, st.dev_shares_rejected), (2, 1));
    assert!((st.measured_fee_pct - 2.0).abs() < 0.2);
    assert!(st.window_fee_pct <= 2.0 + 1e-9);
    assert!(st.debt_secs < SLICE_SECS as f64 * 2.0);
    assert_eq!(st.slices_paid as usize, sim.slice_lens.len());
    assert!(st.next_slice_at.is_some() || st.phase != FeePhase::Waiting);
    let json = serde_json::to_string(&st).expect("stats serialize");
    assert!(json.contains("measured_fee_pct"));
}
