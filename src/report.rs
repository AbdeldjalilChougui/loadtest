//! Final report: text summary and JSON.

use crate::runner::{Config, Stop};
use crate::stats::Snapshot;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Latency {
    pub min: f64,
    pub mean: f64,
    pub stdev: f64,
    pub max: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Bucket {
    /// Upper bound of the bucket in milliseconds.
    pub le_ms: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub url: String,
    pub method: String,
    /// "closed" (fixed concurrency) or "open" (target rate).
    pub model: &'static str,
    pub concurrency: usize,
    pub target_rate: Option<f64>,
    pub duration_secs: f64,
    pub requests: u64,
    pub responses: u64,
    pub errors: u64,
    pub requests_per_sec: f64,
    pub bytes: u64,
    pub bytes_per_sec: f64,
    /// All latencies in milliseconds. Open model: measured from the scheduled send time.
    pub latency_ms: Latency,
    pub histogram: Vec<Bucket>,
    pub status_codes: BTreeMap<String, u64>,
    pub error_kinds: BTreeMap<String, u64>,
    /// Requests completed in each second of the run.
    pub timeline: Vec<u64>,
}

impl Report {
    pub fn new(cfg: &Config, s: &Snapshot) -> Report {
        let secs = s.active.as_secs_f64().max(1e-9);
        let h = &s.hist;
        let ms = |v: u64| v as f64 / 1000.0;
        let latency = if s.done == 0 {
            Latency { min: 0.0, mean: 0.0, stdev: 0.0, max: 0.0, p50: 0.0, p75: 0.0, p90: 0.0, p95: 0.0, p99: 0.0, p999: 0.0 }
        } else {
            Latency {
                min: ms(h.min()),
                mean: h.mean() / 1000.0,
                stdev: h.stdev() / 1000.0,
                max: ms(h.max()),
                p50: s.quantile_ms(0.50),
                p75: s.quantile_ms(0.75),
                p90: s.quantile_ms(0.90),
                p95: s.quantile_ms(0.95),
                p99: s.quantile_ms(0.99),
                p999: s.quantile_ms(0.999),
            }
        };
        Report {
            url: cfg.url.to_string(),
            method: cfg.method.to_string(),
            model: if cfg.rate.is_some() { "open" } else { "closed" },
            concurrency: cfg.concurrency,
            target_rate: cfg.rate,
            duration_secs: secs,
            requests: s.done,
            responses: s.statuses.values().sum(),
            errors: s.errors_total(),
            requests_per_sec: s.done as f64 / secs,
            bytes: s.bytes,
            bytes_per_sec: s.bytes as f64 / secs,
            latency_ms: latency,
            histogram: histogram(s, 10),
            status_codes: s.statuses.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            error_kinds: s.errors.clone(),
            timeline: s.per_sec.clone(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("serializable")
    }

    pub fn to_text(&self) -> String {
        let mut o = String::new();
        let l = &self.latency_ms;
        let mode = match self.target_rate {
            Some(r) => format!("open model, target {r} req/s, max {} in flight", self.concurrency),
            None => format!("closed model, {} connections", self.concurrency),
        };
        let _ = writeln!(o, "{} {} ({mode})", self.method, self.url);
        let _ = writeln!(o, "\nSummary:");
        let _ = writeln!(o, "  Requests:      {} ({} errors)", self.requests, self.errors);
        let _ = writeln!(o, "  Duration:      {:.3} s", self.duration_secs);
        let _ = writeln!(o, "  Requests/sec:  {:.1}", self.requests_per_sec);
        let _ = writeln!(o, "  Transfer:      {} ({}/s)", human_bytes(self.bytes as f64), human_bytes(self.bytes_per_sec));
        let _ = writeln!(o, "\nLatency (ms):");
        let _ = writeln!(o, "  min {:.3}  mean {:.3}  stdev {:.3}  max {:.3}", l.min, l.mean, l.stdev, l.max);
        let _ = writeln!(
            o,
            "  p50 {:.3}  p75 {:.3}  p90 {:.3}  p95 {:.3}  p99 {:.3}  p99.9 {:.3}",
            l.p50, l.p75, l.p90, l.p95, l.p99, l.p999
        );
        if self.requests > 0 {
            let _ = writeln!(o, "\nLatency histogram:");
            let peak = self.histogram.iter().map(|b| b.count).max().unwrap_or(1).max(1);
            for b in &self.histogram {
                let bar = "■".repeat(((b.count as f64 / peak as f64) * 40.0).ceil() as usize);
                let _ = writeln!(o, "  {:>10.3} ms [{:>8}] |{bar}", b.le_ms, b.count);
            }
        }
        if !self.status_codes.is_empty() {
            let _ = writeln!(o, "\nStatus codes:");
            for (k, v) in &self.status_codes {
                let _ = writeln!(o, "  [{k}] {v}");
            }
        }
        if !self.error_kinds.is_empty() {
            let _ = writeln!(o, "\nErrors:");
            for (k, v) in &self.error_kinds {
                let _ = writeln!(o, "  {k}: {v}");
            }
        }
        o
    }
}

/// `n` equal-width buckets from the minimum to the maximum latency.
fn histogram(s: &Snapshot, n: usize) -> Vec<Bucket> {
    if s.done == 0 {
        return Vec::new();
    }
    let (lo, hi) = (s.hist.min(), s.hist.max());
    let width = (hi - lo).div_ceil(n as u64).max(1);
    let mut counts = vec![0u64; n];
    for v in s.hist.iter_recorded() {
        let i = ((v.value_iterated_to().clamp(lo, hi) - lo) / width) as usize;
        counts[i.min(n - 1)] += v.count_at_value();
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(i, count)| Bucket { le_ms: (lo + width * (i as u64 + 1)).min(hi) as f64 / 1000.0, count })
        .collect()
}

pub fn human_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.2} {}", U[i])
    }
}

/// Describe the stop condition for headers and the TUI.
pub fn describe_stop(stop: Stop) -> String {
    match stop {
        Stop::Requests(n) => format!("{n} requests"),
        Stop::Duration(d) => format!("{:.1}s", d.as_secs_f64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{Outcome, Recorder};
    use std::time::Duration;

    #[test]
    fn histogram_buckets_add_up_and_report_serializes() {
        let r = Recorder::new();
        for i in 0..1000u64 {
            r.record(Duration::from_micros(100 + i * 37), Outcome::Response { status: 200 + (i % 2) as u16 * 304, bytes: 5 });
        }
        r.record(Duration::from_millis(5), Outcome::Error("timeout".into()));
        let snap = r.snapshot();
        let cfg = Config::new("http://127.0.0.1:1/".parse().unwrap());
        let rep = Report::new(&cfg, &snap);
        assert_eq!(rep.histogram.iter().map(|b| b.count).sum::<u64>(), 1001);
        assert_eq!(rep.status_codes["200"] + rep.status_codes["504"], 1000);
        assert_eq!((rep.responses, rep.errors), (1000, 1));
        let v: serde_json::Value = serde_json::from_str(&rep.to_json()).unwrap();
        assert_eq!(v["requests"], 1001);
        assert_eq!(v["model"], "closed");
        assert!(v["latency_ms"]["p99"].as_f64().unwrap() > v["latency_ms"]["p50"].as_f64().unwrap());
        let text = rep.to_text();
        assert!(text.contains("Requests/sec") && text.contains("[504] 500") && text.contains("timeout: 1"));
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512.0), "512 B");
        assert_eq!(human_bytes(1536.0), "1.50 KiB");
        assert_eq!(human_bytes(3.0 * 1024.0 * 1024.0), "3.00 MiB");
    }
}
