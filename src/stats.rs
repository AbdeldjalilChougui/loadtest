//! Shared recorder: an HDR latency histogram, status-code and error counters, and a
//! per-second completion timeline.

use hdrhistogram::Histogram;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What happened to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A response arrived: status code and body bytes read.
    Response { status: u16, bytes: u64 },
    /// No response: a short error class such as "timeout" or "connect".
    Error(String),
}

pub struct Recorder {
    started: Instant,
    inner: Mutex<Inner>,
}

#[derive(Clone)]
struct Inner {
    hist: Histogram<u64>,
    statuses: BTreeMap<u16, u64>,
    errors: BTreeMap<String, u64>,
    bytes: u64,
    done: u64,
    per_sec: Vec<u64>,
    last_done: Option<Instant>,
}

/// Highest latency the histogram can hold: one hour, in microseconds.
const MAX_US: u64 = 3_600_000_000;

/// A consistent copy of everything recorded so far.
#[derive(Clone)]
pub struct Snapshot {
    pub elapsed: Duration,
    /// Time from start to the last completed request (what throughput is based on).
    pub active: Duration,
    /// Latency in microseconds. Includes errors (timeouts count with their full wait).
    pub hist: Histogram<u64>,
    pub statuses: BTreeMap<u16, u64>,
    pub errors: BTreeMap<String, u64>,
    pub bytes: u64,
    pub done: u64,
    pub per_sec: Vec<u64>,
}

impl Snapshot {
    pub fn errors_total(&self) -> u64 {
        self.errors.values().sum()
    }

    /// Latency at quantile `q` (0..=1) in milliseconds.
    pub fn quantile_ms(&self, q: f64) -> f64 {
        self.hist.value_at_quantile(q) as f64 / 1000.0
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Recorder {
            started: Instant::now(),
            inner: Mutex::new(Inner {
                hist: Histogram::new_with_bounds(1, MAX_US, 3).expect("histogram bounds"),
                statuses: BTreeMap::new(),
                errors: BTreeMap::new(),
                bytes: 0,
                done: 0,
                per_sec: Vec::new(),
                last_done: None,
            }),
        }
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn record(&self, latency: Duration, outcome: Outcome) {
        let now = Instant::now();
        let us = (latency.as_micros() as u64).clamp(1, MAX_US);
        let sec = now.duration_since(self.started).as_secs() as usize;
        let mut g = self.inner.lock().unwrap();
        g.hist.record(us).expect("value within bounds");
        match outcome {
            Outcome::Response { status, bytes } => {
                *g.statuses.entry(status).or_default() += 1;
                g.bytes += bytes;
            }
            Outcome::Error(kind) => *g.errors.entry(kind).or_default() += 1,
        }
        g.done += 1;
        if g.per_sec.len() <= sec {
            g.per_sec.resize(sec + 1, 0);
        }
        g.per_sec[sec] += 1;
        g.last_done = Some(now);
    }

    pub fn done(&self) -> u64 {
        self.inner.lock().unwrap().done
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap().clone();
        let elapsed = self.started.elapsed();
        Snapshot {
            elapsed,
            active: g.last_done.map_or(elapsed, |t| t.duration_since(self.started)),
            hist: g.hist,
            statuses: g.statuses,
            errors: g.errors,
            bytes: g.bytes,
            done: g.done,
            per_sec: g.per_sec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_latency_status_and_errors() {
        let r = Recorder::new();
        for ms in 1..=100 {
            r.record(Duration::from_millis(ms), Outcome::Response { status: 200, bytes: 10 });
        }
        r.record(Duration::from_secs(2), Outcome::Error("timeout".into()));
        let s = r.snapshot();
        assert_eq!(s.done, 101);
        assert_eq!(s.statuses[&200], 100);
        assert_eq!(s.errors["timeout"], 1);
        assert_eq!(s.bytes, 1000);
        let p50 = s.quantile_ms(0.5);
        assert!((50.0..=52.0).contains(&p50), "{p50}");
        assert!(s.quantile_ms(1.0) >= 1999.0);
        assert_eq!(s.per_sec.iter().sum::<u64>(), 101);
    }
}
