//! Deterministic discrete-event harness for the failover reducer: fake pools answer the
//! reducer's actions after a fixed latency, `ScheduleTick` arms a timer, and every step is
//! checked against the invariants of `docs/en/ARCHITECTURE.md`.
#![allow(dead_code)]

use spm_pool::*;
use std::collections::VecDeque;
use std::time::Duration;

pub const P1: SlotId = SlotId(0);
pub const P2: SlotId = SlotId(1);
pub const P3: SlotId = SlotId(2);

pub fn secs(s: u64) -> Instant {
    Instant::from_secs(s)
}

pub fn ms(m: u64) -> Instant {
    Instant::from_millis(m)
}

pub fn pool(host: &str) -> PoolConfig {
    PoolConfig::new(host, 1200, TlsMode::Auto)
}

pub fn pools(n: usize) -> Vec<PoolConfig> {
    ["pool-a.example", "pool-b.example", "pool-c.example"][..n].iter().map(|h| pool(h)).collect()
}

/// TCP behaviour of a fake pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    Ok,
    Refuse,
    Blackhole,
}

/// TLS behaviour of a fake pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    Ok,
    /// The server only speaks plain TCP: a TLS attempt yields a protocol error.
    PlainOnly,
    /// Certificate/pin failure.
    BadCert,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    Ok,
    Reject(&'static str),
    Silent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    Accept,
    Reject(RejectKind),
    NoAck,
}

#[derive(Clone, Debug)]
pub struct FakePool {
    pub dns_ok: bool,
    pub net: Net,
    pub tls: Tls,
    pub auth: Auth,
    pub first_job: bool,
    /// Periodic jobs while the session lives (`None` = the pool stops sending jobs).
    pub job_every_ms: Option<u64>,
    pub cert_version: Option<u32>,
    pub share: Share,
    pub share_script: VecDeque<Share>,
    pub latency_ms: u64,
}

impl Default for FakePool {
    fn default() -> Self {
        FakePool {
            dns_ok: true,
            net: Net::Ok,
            tls: Tls::Ok,
            auth: Auth::Ok,
            first_job: true,
            job_every_ms: Some(30_000),
            cert_version: Some(3),
            share: Share::Accept,
            share_script: VecDeque::new(),
            latency_ms: 50,
        }
    }
}

impl FakePool {
    pub fn refusing() -> Self {
        FakePool { net: Net::Refuse, ..FakePool::default() }
    }
}

#[derive(Clone, Debug)]
enum Payload {
    Event(Event),
    /// A job from the pool, built at delivery time from the pool's current behaviour.
    /// `periodic` jobs are dropped once the pool stops sending jobs.
    Job { periodic: bool },
}

#[derive(Clone, Debug)]
struct Queued {
    at: Instant,
    seq: u64,
    /// `(slot, epoch)` of the session that produced the event; dropped after a `Close`.
    origin: Option<(usize, u64)>,
    payload: Payload,
}

pub struct Sim {
    pub state: State,
    pub now: Instant,
    pub pools: Vec<FakePool>,
    /// Every step: time, event, actions.
    pub log: Vec<(Instant, Event, Vec<Action>)>,
    pub gpu: bool,
    queue: Vec<Queued>,
    timer: Option<Instant>,
    epoch: [u64; MAX_SLOTS],
    seq: u64,
    job_counter: u64,
}

impl Sim {
    pub fn new(cfgs: Vec<PoolConfig>, fakes: Vec<FakePool>) -> Sim {
        Sim::with_seed(cfgs, fakes, 7)
    }

    pub fn with_seed(cfgs: Vec<PoolConfig>, mut fakes: Vec<FakePool>, seed: u64) -> Sim {
        fakes.resize(MAX_SLOTS, FakePool::default());
        Sim {
            state: State::new(FailoverConfig::default(), cfgs, seed),
            now: Instant::ZERO,
            pools: fakes,
            log: Vec::new(),
            gpu: false,
            queue: Vec::new(),
            timer: None,
            epoch: [0; MAX_SLOTS],
            seq: 0,
            job_counter: 0,
        }
    }

    /// Boot the manager at t = 0.
    pub fn start(&mut self) {
        self.deliver(Event::Tick);
    }

    /// Deliver an event now (after the timer and in-flight events due now).
    pub fn send(&mut self, ev: Event) -> Vec<Action> {
        self.run_until(self.now);
        self.deliver(ev)
    }

    pub fn run_for(&mut self, d: Duration) {
        let t = self.now + d;
        self.run_until(t);
    }

    pub fn run_for_secs(&mut self, s: u64) {
        self.run_for(Duration::from_secs(s));
    }

    pub fn run_until(&mut self, t: Instant) {
        loop {
            let next_q = self.queue.iter().map(|q| (q.at, q.seq)).min();
            let next_t = self.timer;
            let (at, from_timer) = match (next_q, next_t) {
                (Some((qa, _)), Some(ta)) if ta < qa => (ta, true),
                (Some((qa, _)), _) => (qa, false),
                (None, Some(ta)) => (ta, true),
                (None, None) => break,
            };
            if at > t {
                break;
            }
            self.now = self.now.max(at);
            if from_timer {
                self.timer = None;
                self.deliver(Event::Tick);
                continue;
            }
            let pos = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, q)| (q.at, q.seq))
                .map(|(i, _)| i)
                .expect("queue is not empty");
            let q = self.queue.remove(pos);
            if let Some((slot, ep)) = q.origin {
                if self.epoch[slot] != ep {
                    continue; // the session was closed: the event never reaches the reducer
                }
            }
            match (q.payload, q.origin) {
                (Payload::Event(ev), _) => {
                    self.deliver(ev);
                }
                (Payload::Job { periodic }, Some((slot, _))) => {
                    let every = self.pools[slot].job_every_ms;
                    if periodic && every.is_none() {
                        continue; // the pool went quiet
                    }
                    if let Some(every) = every {
                        self.push_job(self.now.plus_millis(every), slot, true);
                    }
                    let ev = self.new_job(SlotId(slot as u8));
                    self.deliver(ev);
                }
                (Payload::Job { .. }, None) => {}
            }
        }
        self.now = self.now.max(t);
    }

    fn new_job(&mut self, slot: SlotId) -> Event {
        self.job_counter += 1;
        Event::JobReceived {
            slot,
            job_id: format!("p{}-j{}", slot.0 + 1, self.job_counter),
            cert_version: self.pools[slot.index()].cert_version,
        }
    }

    fn push(&mut self, at: Instant, slot: Option<usize>, ev: Event) {
        self.seq += 1;
        let origin = slot.map(|s| (s, self.epoch[s]));
        self.queue.push(Queued { at, seq: self.seq, origin, payload: Payload::Event(ev) });
    }

    fn push_job(&mut self, at: Instant, slot: usize, periodic: bool) {
        self.seq += 1;
        let origin = Some((slot, self.epoch[slot]));
        self.queue.push(Queued { at, seq: self.seq, origin, payload: Payload::Job { periodic } });
    }

    fn deliver(&mut self, ev: Event) -> Vec<Action> {
        let pre = self.state.clone();
        let actions = step(&mut self.state, ev.clone(), self.now);
        if let Err(e) = check_step(&pre, &ev, &actions, &self.state, self.now) {
            panic!("invariant violated at {}: {e}\nevent: {ev:?}\nactions: {actions:#?}", self.now);
        }
        self.log.push((self.now, ev, actions.clone()));
        for a in &actions {
            self.apply(a);
        }
        actions
    }

    fn apply(&mut self, a: &Action) {
        let now = self.now;
        match a {
            Action::Resolve { slot } => {
                let p = &self.pools[slot.index()];
                let at = now.plus_millis(p.latency_ms);
                let ev = if p.dns_ok {
                    Event::Resolved { slot: *slot }
                } else {
                    Event::ResolveFailed { slot: *slot }
                };
                self.push(at, Some(slot.index()), ev);
            }
            Action::Connect { slot, tls } => {
                let p = self.pools[slot.index()].clone();
                let lat = p.latency_ms;
                match p.net {
                    Net::Refuse => {
                        self.push(now.plus_millis(lat), Some(slot.index()), Event::ConnectFailed { slot: *slot })
                    }
                    Net::Blackhole => {}
                    Net::Ok => {
                        self.push(now.plus_millis(lat), Some(slot.index()), Event::Connected { slot: *slot });
                        if *tls {
                            let ev = match p.tls {
                                Tls::Ok => Event::TlsOk { slot: *slot },
                                Tls::PlainOnly => Event::TlsProtocolError { slot: *slot },
                                Tls::BadCert => Event::TlsOtherError { slot: *slot },
                            };
                            self.push(now.plus_millis(2 * lat), Some(slot.index()), ev);
                        }
                    }
                }
            }
            Action::Authorize { slot } => {
                let p = self.pools[slot.index()].clone();
                let lat = p.latency_ms;
                match p.auth {
                    Auth::Ok => {
                        self.push(now.plus_millis(lat), Some(slot.index()), Event::Authorized { slot: *slot });
                        if p.first_job {
                            // Delivered even if the pool is otherwise quiet; re-arms periodic jobs.
                            self.push_job(now.plus_millis(2 * lat), slot.index(), false);
                        }
                    }
                    Auth::Reject(msg) => self.push(
                        now.plus_millis(lat),
                        Some(slot.index()),
                        Event::AuthRejected { slot: *slot, msg: msg.to_string() },
                    ),
                    Auth::Silent => {}
                }
            }
            Action::Close { slot } => self.epoch[slot.index()] += 1,
            Action::Submit { slot, .. } => {
                let p = &mut self.pools[slot.index()];
                let outcome = p.share_script.pop_front().unwrap_or(p.share);
                let at = now.plus_millis(p.latency_ms);
                match outcome {
                    Share::Accept => self.push(at, Some(slot.index()), Event::ShareAccepted { slot: *slot }),
                    Share::Reject(kind) => {
                        self.push(at, Some(slot.index()), Event::ShareRejected { slot: *slot, kind })
                    }
                    Share::NoAck => {}
                }
            }
            Action::ScheduleTick { at } => self.timer = Some(*at),
            Action::StartGpu => self.gpu = true,
            Action::StopGpu => self.gpu = false,
            Action::SetActive { .. }
            | Action::DiscardStale { .. }
            | Action::Probe { .. }
            | Action::Alert { .. }
            | Action::Log { .. } => {}
        }
    }

    // ----- helpers for assertions -----

    pub fn manager(&self) -> ManagerState {
        self.state.manager()
    }

    pub fn slot(&self, s: SlotId) -> SlotState {
        self.state.slot_state(s).cloned().expect("valid slot")
    }

    pub fn actions(&self) -> impl Iterator<Item = (Instant, &Action)> {
        self.log.iter().flat_map(|(t, _, acts)| acts.iter().map(move |a| (*t, a)))
    }

    pub fn first(&self, pred: impl Fn(&Action) -> bool) -> Option<Instant> {
        self.actions().find(|(_, a)| pred(a)).map(|(t, _)| t)
    }

    pub fn first_after(&self, after: Instant, pred: impl Fn(&Action) -> bool) -> Option<Instant> {
        self.actions().find(|(t, a)| *t >= after && pred(a)).map(|(t, _)| t)
    }

    pub fn times(&self, pred: impl Fn(&Action) -> bool) -> Vec<Instant> {
        self.actions().filter(|(_, a)| pred(a)).map(|(t, _)| t).collect()
    }

    pub fn count(&self, pred: impl Fn(&Action) -> bool) -> usize {
        self.actions().filter(|(_, a)| pred(a)).count()
    }

    pub fn alerts(&self) -> Vec<String> {
        self.actions()
            .filter_map(|(_, a)| match a {
                Action::Alert { msg } => Some(msg.clone()),
                _ => None,
            })
            .collect()
    }

    /// Time the slot was first set active at or after `after`.
    pub fn activated(&self, s: SlotId, after: Instant) -> Option<Instant> {
        self.first_after(after, |a| *a == Action::SetActive { slot: s })
    }

    /// A GPU hit on the active slot's current job; returns the reducer's answer.
    pub fn hit(&mut self) -> Action {
        let slot = self.state.active().expect("an active slot");
        let job_id = self.state.current_job(slot).expect("a current job").to_string();
        self.hit_on(slot, &job_id)
    }

    pub fn hit_on(&mut self, slot: SlotId, job_id: &str) -> Action {
        let acts = self.send(Event::HitFound { slot, job_id: job_id.to_string() });
        acts.into_iter()
            .find(|a| matches!(a, Action::Submit { .. } | Action::DiscardStale { .. }))
            .expect("every hit is answered")
    }

    /// `n` hits on the active slot, `gap_ms` apart, letting the pool answer each.
    pub fn mine(&mut self, n: usize, gap_ms: u64) {
        for _ in 0..n {
            if self.state.active().is_none() {
                return;
            }
            self.hit();
            self.run_for(Duration::from_millis(gap_ms));
        }
    }
}

fn slot_of(a: &Action) -> Option<SlotId> {
    match a {
        Action::Resolve { slot }
        | Action::Connect { slot, .. }
        | Action::Close { slot }
        | Action::Authorize { slot }
        | Action::SetActive { slot }
        | Action::Submit { slot, .. }
        | Action::Probe { slot } => Some(*slot),
        _ => None,
    }
}

/// The invariants of `docs/en/ARCHITECTURE.md` ("Failover"), checked after every step.
pub fn check_step(pre: &State, ev: &Event, actions: &[Action], post: &State, now: Instant) -> Result<(), String> {
    // 1. At most one active user slot.
    let active = SlotId::all().filter(|s| post.slot_state(*s) == Some(&SlotState::Active)).count();
    if active > 1 {
        return Err(format!("{active} active slots"));
    }
    // 2. Dev-session isolation: the reducer only ever addresses user slots.
    for a in actions {
        if let Some(s) = slot_of(a) {
            if s.index() >= MAX_SLOTS {
                return Err(format!("action {a:?} targets a non-user slot"));
            }
        }
    }
    // 3. Hits: exactly one answer per hit, never both submitted and discarded, never lost; a
    //    Submit only goes to the originating session whose current job is the hit's job.
    let answers: Vec<&Action> = actions
        .iter()
        .filter(|a| matches!(a, Action::Submit { .. } | Action::DiscardStale { .. }))
        .collect();
    match ev {
        Event::HitFound { slot, job_id } => {
            if answers.len() != 1 {
                return Err(format!("hit answered {} times", answers.len()));
            }
            match answers[0] {
                Action::Submit { slot: s, job_id: j } => {
                    if s != slot || j != job_id {
                        return Err("submit does not match the hit".into());
                    }
                    let open = matches!(pre.slot_state(*s), Some(SlotState::Active | SlotState::Draining { .. }));
                    if !open || pre.current_job(*s) != Some(j.as_str()) || post.current_job(*s) != Some(j.as_str()) {
                        return Err(format!("submit of {j} on {s} whose current job is {:?}", pre.current_job(*s)));
                    }
                }
                Action::DiscardStale { slot: s, job_id: j } => {
                    if s != slot || j != job_id {
                        return Err("discard does not match the hit".into());
                    }
                }
                _ => unreachable!(),
            }
        }
        _ => {
            if !answers.is_empty() {
                return Err("submit/discard without a hit".into());
            }
        }
    }
    // 4. Sessions are never re-created while draining.
    for (k, a) in actions.iter().enumerate() {
        if let Action::Resolve { slot } | Action::Connect { slot, .. } | Action::Probe { slot } = a {
            let was_draining = matches!(pre.slot_state(*slot), Some(SlotState::Draining { .. }));
            let closed_before = actions[..k].contains(&Action::Close { slot: *slot });
            if was_draining && !closed_before {
                return Err(format!("{a:?} while {slot} is draining"));
            }
        }
    }
    // 5. Draining lasts at most drain_s.
    for s in SlotId::all() {
        if let Some(SlotState::Draining { until }) = post.slot_state(s) {
            if *until <= now || *until > now.plus_secs(post.config().drain_s) {
                return Err(format!("{s} draining until {until} at {now}"));
            }
        }
    }
    // 6. Every failure of the active slot ends in a new active slot or AllDown in bounded time.
    if matches!(post.manager(), ManagerState::Starting | ManagerState::FailingOver { .. }) {
        let started = post.failover_started().ok_or("failover without a start time")?;
        let bound = post.config().failover_bound() + Duration::from_secs(1);
        if now.saturating_since(started) > bound {
            return Err(format!("failing over since {started}, now {now} (bound {bound:?})"));
        }
    }
    // 7. Manager and slots agree.
    match post.manager() {
        ManagerState::Mining { active } => {
            if post.slot_state(active) != Some(&SlotState::Active) {
                return Err(format!("Mining on {active} which is not Active"));
            }
        }
        ManagerState::FailingOver { to, .. } => {
            if !post.slot_state(to).is_some_and(SlotState::is_open) {
                return Err(format!("failing over to {to} which has no session"));
            }
        }
        ManagerState::Paused { reason }
            if reason.is_frozen()
                && SlotId::all().any(|s| post.slot_state(s).is_some_and(SlotState::is_open)) =>
        {
            return Err(format!("sessions open while paused ({reason:?})"));
        }
        _ => {}
    }
    // 8. The GPU hashes iff an active slot has a job and nothing is paused.
    let want = post.pause_reason().is_none() && post.active().is_some_and(|a| post.current_job(a).is_some());
    if post.gpu_running() != want {
        return Err(format!("gpu_running={} but expected {want}", post.gpu_running()));
    }
    let starts = actions.iter().filter(|a| **a == Action::StartGpu).count();
    let stops = actions.iter().filter(|a| **a == Action::StopGpu).count();
    if starts + stops > 1 || (starts == 1 && pre.gpu_running()) || (stops == 1 && !pre.gpu_running()) {
        return Err("inconsistent StartGpu/StopGpu".into());
    }
    // 9. SetActive names the slot that ends up active.
    if let Some(Action::SetActive { slot }) = actions.iter().rev().find(|a| matches!(a, Action::SetActive { .. })) {
        if post.active() != Some(*slot) {
            return Err(format!("SetActive {slot} but active is {:?}", post.active()));
        }
    }
    Ok(())
}
