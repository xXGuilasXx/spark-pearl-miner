//! Just enough of the Prometheus text exposition format to read two vLLM gauges.
//!
//! ```text
//! # TYPE vllm:num_requests_running gauge
//! vllm:num_requests_running{engine="0",model_name="qwen36-35b-nvfp4"} 0.0
//! vllm:num_requests_waiting{engine="0",model_name="qwen36-35b-nvfp4"} 0.0
//! ```
//!
//! Every series of a gauge is summed (one per engine and model). Names must match exactly:
//! `vllm:num_requests_waiting_by_reason` is a different metric. Lines of other metrics are
//! skipped without being parsed, so an odd line elsewhere never hides the signal.

use std::fmt;

/// Requests in the running batches.
pub const RUNNING: &str = "vllm:num_requests_running";
/// Requests queued.
pub const WAITING: &str = "vllm:num_requests_waiting";

/// The load of a vLLM server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VllmLoad {
    /// Sum of `vllm:num_requests_running` over all series.
    pub running: f64,
    /// Sum of `vllm:num_requests_waiting` over all series.
    pub waiting: f64,
}

impl VllmLoad {
    /// No request running or waiting.
    pub fn is_idle(&self) -> bool {
        self.running == 0.0 && self.waiting == 0.0
    }
}

/// Why the metrics could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromError {
    /// The gauge is not in the exposition (not a vLLM, or an incompatible version).
    Missing(&'static str),
    /// A line of one of our gauges is malformed (1-based line number).
    BadLine { line: usize, reason: &'static str },
}

impl fmt::Display for PromError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromError::Missing(name) => write!(f, "metric {name} not found"),
            PromError::BadLine { line, reason } => write!(f, "metrics line {line}: {reason}"),
        }
    }
}

impl std::error::Error for PromError {}

/// One parsed sample line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromSample<'a> {
    pub name: &'a str,
    /// The raw label set between the braces (empty when there is none).
    pub labels: &'a str,
    pub value: f64,
}

/// The metric name of a sample line, or `None` for blank and comment lines.
pub fn metric_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c == '{' || c.is_ascii_whitespace()).unwrap_or(line.len());
    Some(&line[..end])
}

/// Parses one line. `Ok(None)` for blank and comment lines.
pub fn parse_line(line: &str) -> Result<Option<PromSample<'_>>, &'static str> {
    let line = line.trim();
    let Some(name) = metric_name(line) else { return Ok(None) };
    if name.is_empty() {
        return Err("missing metric name");
    }
    let mut rest = &line[name.len()..];
    let mut labels = "";
    if let Some(after) = rest.strip_prefix('{') {
        let close = closing_brace(after).ok_or("unterminated label set")?;
        labels = &after[..close];
        rest = &after[close + 1..];
    }
    let mut fields = rest.split_ascii_whitespace();
    let value = fields.next().ok_or("missing value")?;
    let value: f64 = value.parse().map_err(|_| "value is not a number")?;
    if let Some(ts) = fields.next() {
        ts.parse::<i64>().map_err(|_| "timestamp is not an integer")?;
    }
    if fields.next().is_some() {
        return Err("trailing data");
    }
    Ok(Some(PromSample { name, labels, value }))
}

/// Index of the `}` closing a label set, skipping quoted label values (with `\"` escapes).
fn closing_brace(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if in_quotes {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_quotes = false,
                _ => {}
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == '}' {
            return Some(i);
        }
    }
    None
}

/// Sum of every series of the gauge `name`; `Ok(None)` when it is absent. Values must be finite
/// and non-negative.
pub fn gauge_sum(text: &str, name: &str) -> Result<Option<f64>, PromError> {
    let mut sum: Option<f64> = None;
    for (i, line) in text.lines().enumerate() {
        if metric_name(line) != Some(name) {
            continue;
        }
        let bad = |reason| PromError::BadLine { line: i + 1, reason };
        let sample = parse_line(line).map_err(bad)?.ok_or(bad("empty"))?;
        if !sample.value.is_finite() || sample.value < 0.0 {
            return Err(bad("gauge is negative or not finite"));
        }
        *sum.get_or_insert(0.0) += sample.value;
    }
    Ok(sum)
}

/// Reads both vLLM gauges. Both must be present.
pub fn parse_vllm_load(text: &str) -> Result<VllmLoad, PromError> {
    let running = gauge_sum(text, RUNNING)?.ok_or(PromError::Missing(RUNNING))?;
    let waiting = gauge_sum(text, WAITING)?.ok_or(PromError::Missing(WAITING))?;
    Ok(VllmLoad { running, waiting })
}
