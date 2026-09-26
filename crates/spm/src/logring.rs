//! In-memory log for the GUI: a `tracing` layer that keeps the last lines (INFO and above) in a
//! ring buffer and pushes each one as a `log` SSE event.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spm_api::{ApiEvent, LogEntry};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

pub const RING_LINES: usize = 5000;

#[derive(Debug)]
pub struct LogRing {
    lines: Mutex<VecDeque<LogEntry>>,
    seq: AtomicU64,
    cap: usize,
    tx: broadcast::Sender<ApiEvent>,
}

impl LogRing {
    pub fn new(cap: usize, tx: broadcast::Sender<ApiEvent>) -> Arc<LogRing> {
        Arc::new(LogRing { lines: Mutex::new(VecDeque::with_capacity(cap)), seq: AtomicU64::new(0), cap, tx })
    }

    pub fn push(&self, level: &str, target: &str, msg: String) {
        let e = LogEntry {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            at_ms: crate::paths::unix_ms(),
            level: level.to_string(),
            target: target.to_string(),
            msg,
        };
        if let Ok(mut l) = self.lines.lock() {
            if l.len() >= self.cap {
                l.pop_front();
            }
            l.push_back(e.clone());
        }
        let _ = self.tx.send(ApiEvent::Log(e));
    }

    pub fn since(&self, since: u64, limit: usize) -> Vec<LogEntry> {
        let Ok(l) = self.lines.lock() else { return Vec::new() };
        let newer: Vec<&LogEntry> = l.iter().filter(|e| e.seq > since).collect();
        let skip = newer.len().saturating_sub(limit);
        newer.into_iter().skip(skip).cloned().collect()
    }
}

/// The layer feeding a [`LogRing`].
pub struct RingLayer(pub Arc<LogRing>);

#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.rest, " {}={}", field.name(), value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, " {}={:?}", field.name(), value);
        }
    }
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::INFO {
            return;
        }
        let mut f = Fields::default();
        event.record(&mut f);
        let level = match *meta.level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "info",
        };
        self.0.push(level, meta.target(), format!("{}{}", f.message, f.rest));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_lines() {
        let (tx, mut rx) = broadcast::channel(16);
        let r = LogRing::new(3, tx);
        for i in 0..5 {
            r.push("info", "t", format!("line {i}"));
        }
        let all = r.since(0, 100);
        assert_eq!(all.iter().map(|e| e.msg.as_str()).collect::<Vec<_>>(), ["line 2", "line 3", "line 4"]);
        assert_eq!(r.since(4, 100).len(), 1);
        assert_eq!(r.since(0, 1)[0].msg, "line 4");
        assert!(matches!(rx.try_recv(), Ok(ApiEvent::Log(_))));
    }
}
