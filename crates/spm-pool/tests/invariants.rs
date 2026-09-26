//! Property test: random sequences of transport events, user requests and time jumps. The
//! invariants of `common::check_step` must hold after every step:
//! at most one active slot; a Submit only targets the originating session whose current job is
//! the hit's job; every hit is answered exactly once (never both submitted and discarded, never
//! lost); only user slots are addressed (the dev session is outside this reducer); sessions are
//! never re-created while draining; draining lasts at most `drain_s`; a failure of the active
//! slot ends in a new active slot or AllDown within `FailoverConfig::failover_bound`; manager
//! state, GPU state and slot states agree.
mod common;

use common::check_step;
use proptest::prelude::*;
use spm_pool::*;

#[derive(Debug, Clone)]
enum Op {
    /// Let time pass; the harness delivers `Tick` at every scheduled instant on the way.
    Advance(u64),
    /// The "natural" next event for an open session (a fake pool reacting), chosen by selectors.
    Drive(u8, u8),
    /// Any event at all, including user requests, config edits and nonsense for the state.
    Raw(Event),
}

fn slot() -> impl Strategy<Value = SlotId> {
    (0u8..=3).prop_map(SlotId) // 3 is out of range on purpose
}

fn job() -> impl Strategy<Value = String> {
    prop_oneof![Just("j1".to_string()), Just("j2".to_string()), Just("old".to_string())]
}

fn tls_mode() -> impl Strategy<Value = TlsMode> {
    prop_oneof![Just(TlsMode::Auto), Just(TlsMode::On), Just(TlsMode::Off)]
}

fn pool_cfg() -> impl Strategy<Value = PoolConfig> {
    (0usize..3, tls_mode(), prop::bool::weighted(0.85), 0u8..2).prop_map(|(h, tls, enabled, login)| {
        let mut p = PoolConfig::new(["a.example", "b.example", "c.example"][h], 1200, tls);
        p.enabled = enabled;
        p.login = format!("w{login}");
        p
    })
}

fn reason() -> impl Strategy<Value = PauseReason> {
    prop_oneof![
        3 => Just(PauseReason::Yield),
        2 => Just(PauseReason::Health),
        2 => Just(PauseReason::UserStop),
        1 => Just(PauseReason::UnsupportedScheme),
        1 => Just(PauseReason::RejectEverywhere),
    ]
}

fn reject_kind() -> impl Strategy<Value = RejectKind> {
    prop_oneof![
        Just(RejectKind::Invalid),
        Just(RejectKind::Stale),
        Just(RejectKind::LowDiff),
        Just(RejectKind::Other)
    ]
}

fn raw_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        Just(Event::Tick),
        slot().prop_map(|slot| Event::Resolved { slot }),
        slot().prop_map(|slot| Event::ResolveFailed { slot }),
        slot().prop_map(|slot| Event::Connected { slot }),
        slot().prop_map(|slot| Event::ConnectFailed { slot }),
        slot().prop_map(|slot| Event::TlsOk { slot }),
        slot().prop_map(|slot| Event::TlsProtocolError { slot }),
        slot().prop_map(|slot| Event::TlsOtherError { slot }),
        slot().prop_map(|slot| Event::Authorized { slot }),
        slot().prop_map(|slot| Event::AuthRejected { slot, msg: "bad wallet".into() }),
        (slot(), job(), prop::option::of(2u32..6))
            .prop_map(|(slot, job_id, cert_version)| Event::JobReceived { slot, job_id, cert_version }),
        slot().prop_map(|slot| Event::ShareAccepted { slot }),
        (slot(), reject_kind()).prop_map(|(slot, kind)| Event::ShareRejected { slot, kind }),
        slot().prop_map(|slot| Event::SubmitAckTimeout { slot }),
        (slot(), 0u64..200).prop_map(|(slot, s)| Event::Disconnected { slot, was_active_for_s: s }),
        slot().prop_map(|slot| Event::BanText { slot }),
        slot().prop_map(|slot| Event::ProbeOk { slot }),
        slot().prop_map(|slot| Event::ProbeFailed { slot }),
        slot().prop_map(|slot| Event::UserSwitch { slot }),
        prop::option::of(slot()).prop_map(|slot| Event::UserPin { slot }),
        prop::collection::vec(pool_cfg(), 0..=4).prop_map(|pools| Event::ConfigChanged { pools }),
        reason().prop_map(|reason| Event::PauseRequest { reason }),
        Just(Event::ResumeRequest),
        (slot(), job()).prop_map(|(slot, job_id)| Event::HitFound { slot, job_id }),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => prop_oneof![0u64..2_000, 0u64..40_000, 0u64..1_000_000].prop_map(Op::Advance),
        8 => (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Drive(a, b)),
        2 => raw_event().prop_map(Op::Raw),
    ]
}

/// What a fake pool would plausibly send next for one of the open sessions.
fn drive(st: &State, sel: u8, var: u8, jobs: &mut u64) -> Event {
    let open: Vec<SlotId> = SlotId::all().filter(|s| st.slot_state(*s).is_some_and(SlotState::is_open)).collect();
    if open.is_empty() {
        return Event::Tick;
    }
    let slot = open[sel as usize % open.len()];
    let v = var % 20;
    let mut new_job = |cert_version: Option<u32>| {
        *jobs += 1;
        Event::JobReceived { slot, job_id: format!("n{jobs}"), cert_version }
    };
    let current = st.current_job(slot).map(str::to_string);
    let hit_current = |job_id: Option<String>| Event::HitFound { slot, job_id: job_id.unwrap_or_else(|| "none".into()) };
    match st.slot_state(slot) {
        Some(SlotState::Resolving) => match v {
            0..=16 => Event::Resolved { slot },
            _ => Event::ResolveFailed { slot },
        },
        Some(SlotState::Connecting) => match v {
            0..=15 => Event::Connected { slot },
            _ => Event::ConnectFailed { slot },
        },
        Some(SlotState::TlsHandshake) => match v {
            0..=14 => Event::TlsOk { slot },
            15..=17 => Event::TlsProtocolError { slot },
            _ => Event::TlsOtherError { slot },
        },
        Some(SlotState::Authorizing) => match v {
            0..=15 => Event::Authorized { slot },
            16..=17 => new_job(Some(3)),
            _ => Event::AuthRejected { slot, msg: "unknown worker".into() },
        },
        Some(SlotState::AwaitingJob) => match v {
            0..=16 => new_job(Some(3)),
            17 => new_job(None),
            18 => new_job(Some(4)),
            _ => Event::Disconnected { slot, was_active_for_s: 0 },
        },
        Some(SlotState::Active) => match v {
            0..=4 => hit_current(current),
            5..=7 => Event::ShareAccepted { slot },
            8 => Event::ShareRejected { slot, kind: RejectKind::Invalid },
            9 => Event::ShareRejected { slot, kind: RejectKind::Stale },
            10 => Event::ShareRejected { slot, kind: RejectKind::LowDiff },
            11..=12 => new_job(Some(3)),
            13 => Event::SubmitAckTimeout { slot },
            14 => Event::Disconnected { slot, was_active_for_s: u64::from(sel) },
            15 => Event::BanText { slot },
            16 => Event::HitFound { slot, job_id: "old".into() },
            17 => new_job(Some(4)),
            _ => Event::Tick,
        },
        Some(SlotState::Standby) => match v {
            0..=6 => new_job(Some(3)),
            7..=9 => Event::ProbeOk { slot },
            10 => Event::Disconnected { slot, was_active_for_s: 0 },
            // A probe session never mines: a hit naming it must be discarded.
            11..=16 => hit_current(current),
            _ => Event::Tick,
        },
        Some(SlotState::Draining { .. }) => match v {
            0..=7 => hit_current(current),
            8..=12 => Event::ShareAccepted { slot },
            13 => Event::ShareRejected { slot, kind: RejectKind::Invalid },
            14 => Event::Disconnected { slot, was_active_for_s: 0 },
            15 => new_job(Some(3)),
            _ => Event::Tick,
        },
        _ => Event::Tick,
    }
}

struct Harness {
    st: State,
    now: Instant,
    timer: Option<Instant>,
    jobs: u64,
    steps: usize,
}

impl Harness {
    fn step(&mut self, ev: Event) -> Result<(), TestCaseError> {
        let pre = self.st.clone();
        let acts = step(&mut self.st, ev.clone(), self.now);
        if let Err(e) = check_step(&pre, &ev, &acts, &self.st, self.now) {
            return Err(TestCaseError::fail(format!(
                "step {} at {}: {e}\nevent: {ev:?}\nactions: {acts:?}\nmanager: {:?}",
                self.steps,
                self.now,
                self.st.manager()
            )));
        }
        for a in &acts {
            if let Action::ScheduleTick { at } = a {
                prop_assert!(*at > self.now, "tick scheduled in the past: {at} at {}", self.now);
                self.timer = Some(*at);
            }
        }
        self.steps += 1;
        Ok(())
    }

    fn advance(&mut self, dt: u64) -> Result<(), TestCaseError> {
        let target = self.now.plus_millis(dt);
        while let Some(t) = self.timer.filter(|t| *t <= target) {
            self.timer = None;
            self.now = self.now.max(t);
            self.step(Event::Tick)?;
        }
        self.now = target;
        Ok(())
    }
}

/// Guard against a vacuous generator: random runs must reach every interesting state.
#[test]
fn random_runs_cover_the_state_space() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    use std::collections::BTreeSet;

    let strategy = (
        prop::collection::vec(pool_cfg(), 1..=3),
        any::<u64>(),
        prop::collection::vec(op(), 1..300),
    );
    let mut runner = TestRunner::deterministic();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for _ in 0..512 {
        let (cfgs, seed, ops) = strategy.new_tree(&mut runner).expect("value").current();
        let mut h = Harness { st: State::new(FailoverConfig::default(), cfgs, seed), now: Instant::ZERO, timer: None, jobs: 0, steps: 0 };
        let mut note = |st: &State, acts: &[Action]| {
            match st.manager() {
                ManagerState::Starting => seen.insert("Starting"),
                ManagerState::Mining { .. } => seen.insert("Mining"),
                ManagerState::FailingOver { from, to } if from == to => seen.insert("FailingOver(same)"),
                ManagerState::FailingOver { .. } => seen.insert("FailingOver"),
                ManagerState::AllDown { .. } => seen.insert("AllDown"),
                ManagerState::Paused { reason: PauseReason::RejectEverywhere } => seen.insert("Paused(RejectEverywhere)"),
                ManagerState::Paused { reason: PauseReason::UnsupportedScheme } => seen.insert("Paused(UnsupportedScheme)"),
                ManagerState::Paused { .. } => seen.insert("Paused(other)"),
            };
            for s in SlotId::all() {
                match st.slot_state(s) {
                    Some(SlotState::Standby) => seen.insert("Standby"),
                    Some(SlotState::Draining { .. }) => seen.insert("Draining"),
                    Some(SlotState::Backoff { .. }) => seen.insert("Backoff"),
                    Some(SlotState::ConfigError { .. }) => seen.insert("ConfigError"),
                    Some(SlotState::Quarantined { .. }) => seen.insert("Quarantined"),
                    _ => false,
                };
            }
            for a in acts {
                match a {
                    Action::Submit { .. } => seen.insert("Submit"),
                    Action::DiscardStale { slot, job_id }
                        if st.slot_state(*slot) == Some(&SlotState::Standby)
                            && st.current_job(*slot) == Some(job_id.as_str()) =>
                    {
                        seen.insert("DiscardStale(standby current job)")
                    }
                    Action::DiscardStale { .. } => seen.insert("DiscardStale"),
                    Action::Probe { .. } => seen.insert("Probe"),
                    Action::Connect { tls: false, .. } => seen.insert("Connect(plain)"),
                    _ => false,
                };
            }
        };
        let acts = step(&mut h.st, Event::Tick, h.now);
        note(&h.st, &acts);
        for op in ops {
            let ev = match op {
                Op::Advance(dt) => {
                    h.now = h.now.plus_millis(dt);
                    Event::Tick
                }
                Op::Drive(a, b) => drive(&h.st, a, b, &mut h.jobs),
                Op::Raw(ev) => ev,
            };
            let acts = step(&mut h.st, ev, h.now);
            note(&h.st, &acts);
        }
    }
    let want = [
        "Starting", "Mining", "FailingOver", "FailingOver(same)", "AllDown", "Paused(RejectEverywhere)",
        "Paused(UnsupportedScheme)", "Paused(other)", "Standby", "Draining", "Backoff", "ConfigError",
        "Quarantined", "Submit", "DiscardStale", "DiscardStale(standby current job)", "Probe",
        "Connect(plain)",
    ];
    let missing: Vec<&str> = want.iter().copied().filter(|w| !seen.contains(w)).collect();
    assert!(missing.is_empty(), "never reached: {missing:?}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_for_random_event_sequences(
        cfgs in prop::collection::vec(pool_cfg(), 1..=3),
        seed in any::<u64>(),
        ops in prop::collection::vec(op(), 1..300),
    ) {
        let mut h = Harness {
            st: State::new(FailoverConfig::default(), cfgs, seed),
            now: Instant::ZERO,
            timer: None,
            jobs: 0,
            steps: 0,
        };
        h.step(Event::Tick)?;
        for op in ops {
            match op {
                Op::Advance(dt) => h.advance(dt)?,
                Op::Drive(a, b) => {
                    let ev = drive(&h.st, a, b, &mut h.jobs);
                    h.step(ev)?;
                }
                Op::Raw(ev) => h.step(ev)?,
            }
        }
        // Let every pending deadline play out: the manager must settle.
        h.advance(3_600_000)?;
    }

    #[test]
    fn same_seed_same_decisions(
        cfgs in prop::collection::vec(pool_cfg(), 1..=3),
        seed in any::<u64>(),
        ops in prop::collection::vec(op(), 1..120),
    ) {
        let run = || -> Result<Vec<Vec<Action>>, TestCaseError> {
            let mut st = State::new(FailoverConfig::default(), cfgs.clone(), seed);
            let mut now = Instant::ZERO;
            let mut jobs = 0;
            let mut log = vec![step(&mut st, Event::Tick, now)];
            for op in &ops {
                let ev = match op {
                    Op::Advance(dt) => { now = now.plus_millis(*dt); Event::Tick }
                    Op::Drive(a, b) => drive(&st, *a, *b, &mut jobs),
                    Op::Raw(ev) => ev.clone(),
                };
                log.push(step(&mut st, ev, now));
            }
            Ok(log)
        };
        prop_assert_eq!(run()?, run()?);
    }
}
