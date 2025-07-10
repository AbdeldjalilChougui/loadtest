//! HTTP load testing: closed (fixed concurrency) and open (target rate) models, HDR
//! histograms, live TUI and JSON reports.

pub mod report;
pub mod runner;
pub mod stats;
pub mod tui;

pub use report::Report;
pub use runner::{run, Config, Stop};
pub use stats::{Outcome, Recorder, Snapshot};

use std::time::Duration;

/// Parse `500ms`, `10s`, `2m`, `1h` or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let v: f64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "ms" => v / 1000.0,
        "s" => v,
        "m" => v * 60.0,
        "h" => v * 3600.0,
        _ => return Err(format!("bad duration unit in {s:?} (use ms, s, m, h)")),
    };
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("duration must be positive: {s:?}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("3"), Ok(Duration::from_secs(3)));
        assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1500)));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("0s").is_err());
    }
}
