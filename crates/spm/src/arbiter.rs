//! WorkArbiter: decides what the GPU works on and keeps every work unit bound to the session
//! whose job produced it.
//!
//! Priority: **pause → dev slice → active user pool → idle**. The fee switch lives only here (the
//! worker is fee-agnostic) and failover lives only in the spm-pool reducer (the worker never sees
//! pool identities).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use spm_pool::SlotId;
use spm_proto::Job;
use spm_work::{Shape, WorkError, WorkUnit};

/// What the GPU hashes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Idle,
    User(SlotId),
    Dev,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Idle => "idle",
            Target::User(_) => "user",
            Target::Dev => "dev",
        }
    }
}

/// Inputs of one decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArbiterInput {
    /// Stopped, paused by the user, or stopped after a hardware fault.
    pub paused: bool,
    /// The fee scheduler is in a slice.
    pub dev_slice: bool,
    /// The dev session is authorized and has a mineable job.
    pub dev_job: bool,
    /// The reducer's active slot.
    pub user_active: Option<SlotId>,
    /// The reducer lets the GPU hash (active slot has a job, nothing paused).
    pub user_gpu: bool,
    /// The active slot has a mineable job.
    pub user_job: bool,
}

/// pause → dev slice → active user pool → idle.
pub fn decide(i: &ArbiterInput) -> Target {
    if i.paused {
        return Target::Idle;
    }
    if i.dev_slice && i.dev_job {
        return Target::Dev;
    }
    match i.user_active {
        Some(slot) if i.user_gpu && i.user_job => Target::User(slot),
        _ => Target::Idle,
    }
}

/// A work unit and the session it belongs to.
#[derive(Debug, Clone)]
pub struct Binding {
    pub target: Target,
    pub session_uid: u64,
    pub job_id: String,
    pub wu: WorkUnit,
}

/// Recent work units by id (hits can arrive after the job changed).
#[derive(Debug)]
pub struct JobBook {
    next_wu: u64,
    bound: VecDeque<Binding>,
    cap: usize,
}

impl Default for JobBook {
    fn default() -> Self {
        JobBook { next_wu: 1, bound: VecDeque::new(), cap: 64 }
    }
}

impl JobBook {
    /// Build a work unit for `job` of `session_uid` and remember the binding.
    pub fn bind(&mut self, target: Target, session_uid: u64, job: &Job, shape: Shape) -> Result<WorkUnit, WorkError> {
        let wu = WorkUnit::build(job, shape, session_uid, self.next_wu)?;
        self.next_wu += 1;
        self.bound.push_back(Binding { target, session_uid, job_id: job.job_id.clone(), wu: wu.clone() });
        while self.bound.len() > self.cap {
            self.bound.pop_front();
        }
        Ok(wu)
    }

    pub fn get(&self, wu_id: u64) -> Option<&Binding> {
        self.bound.iter().rev().find(|b| b.wu.wu_id == wu_id)
    }
}

/// Credited-MAC counters (MACs of the full m·n·k problem per attempt, as pools credit them).
#[derive(Debug, Default)]
pub struct Credit {
    pub user_macs: f64,
    pub dev_macs: f64,
    window: VecDeque<(Instant, u64)>,
}

pub const RATE_WINDOW: Duration = Duration::from_secs(10);
/// One attempt credits m·n·k MACs (7.04e13 in the production shape, about 1.4 s), so a 10 s window is
/// quantized to whole attempts (±7 T-MAC/s); the 60 s window is what to read as "the rate".
pub const RATE_WINDOW_LONG: Duration = Duration::from_secs(60);

impl Credit {
    pub fn add(&mut self, target: Target, macs: u64, now: Instant) {
        match target {
            Target::Dev => self.dev_macs += macs as f64,
            Target::User(_) => self.user_macs += macs as f64,
            Target::Idle => {}
        }
        if target != Target::Idle {
            self.window.push_back((now, macs));
        }
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while self.window.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW_LONG) {
            self.window.pop_front();
        }
    }

    /// Credited MAC/s over the last 10 s (quantized to whole attempts).
    pub fn rate(&mut self, now: Instant) -> f64 {
        self.rate_over(now, RATE_WINDOW)
    }

    /// Credited MAC/s over the last 60 s.
    pub fn rate_60s(&mut self, now: Instant) -> f64 {
        self.rate_over(now, RATE_WINDOW_LONG)
    }

    fn rate_over(&mut self, now: Instant, window: Duration) -> f64 {
        self.trim(now);
        let total: u64 = self.window.iter().filter(|(t, _)| now.duration_since(*t) <= window).map(|(_, m)| m).sum();
        total as f64 / window.as_secs_f64()
    }

    pub fn total(&self) -> f64 {
        self.user_macs + self.dev_macs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order() {
        let base = ArbiterInput { user_active: Some(SlotId(1)), user_gpu: true, user_job: true, ..Default::default() };
        assert_eq!(decide(&base), Target::User(SlotId(1)));
        assert_eq!(decide(&ArbiterInput { dev_slice: true, dev_job: true, ..base }), Target::Dev);
        // A slice without a dev job keeps the user pool hashing.
        assert_eq!(decide(&ArbiterInput { dev_slice: true, dev_job: false, ..base }), Target::User(SlotId(1)));
        // Pause wins over everything.
        assert_eq!(decide(&ArbiterInput { paused: true, dev_slice: true, dev_job: true, ..base }), Target::Idle);
        assert_eq!(decide(&ArbiterInput { user_gpu: false, ..base }), Target::Idle);
        assert_eq!(decide(&ArbiterInput::default()), Target::Idle);
    }

    #[test]
    fn book_binds_and_forgets_old_units() {
        let job = Job {
            job_id: "j_1".into(),
            header: spm_mockpool::trivial_header(),
            target: spm_pow::target_from_compact(spm_mockpool::TRIVIAL_NBITS),
            height: Some(1),
            diff: Some(1),
            cert_version: Some(3),
        };
        let mut b = JobBook::default();
        let shape = crate::worker_sim::SIM_SHAPE;
        let first = b.bind(Target::User(SlotId(0)), 7, &job, shape).unwrap();
        assert_eq!(first.session_id, 7);
        for _ in 0..100 {
            b.bind(Target::Dev, 8, &job, shape).unwrap();
        }
        assert!(b.get(first.wu_id).is_none());
        let last = b.get(101).unwrap();
        assert_eq!((last.target, last.session_uid), (Target::Dev, 8));
        let old = Job { cert_version: Some(4), ..job };
        assert!(matches!(b.bind(Target::Dev, 8, &old, shape), Err(WorkError::UpdateRequired(Some(4)))));
    }

    #[test]
    fn credit_rate_uses_a_ten_second_window() {
        let mut c = Credit::default();
        let t0 = Instant::now();
        c.add(Target::User(SlotId(0)), 1000, t0);
        c.add(Target::Dev, 1000, t0);
        c.add(Target::Idle, 1000, t0);
        assert_eq!(c.rate(t0), 200.0);
        assert_eq!(c.total(), 2000.0);
        assert_eq!(c.rate(t0 + Duration::from_secs(11)), 0.0);
        // The 60 s window still sees the credit after the 10 s one has forgotten it.
        assert!((c.rate_60s(t0 + Duration::from_secs(11)) - 2000.0 / 60.0).abs() < 1e-9);
        assert_eq!(c.rate_60s(t0 + Duration::from_secs(61)), 0.0);
    }
}
