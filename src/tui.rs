//! Live terminal UI: progress, current and overall throughput, latency percentiles,
//! status codes and a requests-per-second sparkline. `q`, Esc or Ctrl-C stops the test.

use crate::report::describe_stop;
use crate::runner::{Config, Stop};
use crate::stats::{Recorder, Snapshot};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Gauge, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// Fraction of the run completed, 0..=1.
pub fn progress(cfg: &Config, s: &Snapshot) -> f64 {
    match cfg.stop {
        Stop::Requests(n) => s.done as f64 / n.max(1) as f64,
        Stop::Duration(d) => s.elapsed.as_secs_f64() / d.as_secs_f64().max(1e-9),
    }
    .clamp(0.0, 1.0)
}

pub fn draw(f: &mut Frame, cfg: &Config, s: &Snapshot) {
    let [title, gauge, middle, spark, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(9),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let mode = match cfg.rate {
        Some(r) => format!("open model @ {r} req/s, ≤{} in flight", cfg.concurrency),
        None => format!("closed model, {} connections", cfg.concurrency),
    };
    f.render_widget(Line::from(format!(" loadtest  {} {}  ({mode}, {})", cfg.method, cfg.url, describe_stop(cfg.stop))).bold(), title);

    let p = progress(cfg, s);
    f.render_widget(
        Gauge::default()
            .block(Block::bordered().title(" progress "))
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(p)
            .label(format!("{:.0}%  {} done  {:.1}s", p * 100.0, s.done, s.elapsed.as_secs_f64())),
        gauge,
    );

    let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(middle);
    let last_full = s.per_sec.len().checked_sub(2).and_then(|i| s.per_sec.get(i)).copied().unwrap_or(0);
    let overall = s.done as f64 / s.elapsed.as_secs_f64().max(1e-9);
    let q = |x| if s.done == 0 { 0.0 } else { s.quantile_ms(x) };
    let lines = vec![
        Line::from(format!("requests/s  last second {last_full}   overall {overall:.1}")),
        Line::from(format!("errors      {}", s.errors_total())),
        Line::from(""),
        Line::from(format!("p50   {:>10.3} ms", q(0.5))),
        Line::from(format!("p90   {:>10.3} ms", q(0.9))),
        Line::from(format!("p99   {:>10.3} ms", q(0.99))),
        Line::from(format!("p99.9 {:>10.3} ms", q(0.999))),
        Line::from(format!("max   {:>10.3} ms", if s.done == 0 { 0.0 } else { s.hist.max() as f64 / 1000.0 })),
    ];
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" latency ")), left);

    let mut rows: Vec<Row> = s.statuses.iter().map(|(k, v)| Row::new(vec![k.to_string(), v.to_string()])).collect();
    rows.extend(s.errors.iter().map(|(k, v)| Row::new(vec![k.clone(), v.to_string()]).red()));
    f.render_widget(
        Table::new(rows, [Constraint::Length(12), Constraint::Min(8)])
            .header(Row::new(vec!["status", "count"]).bold())
            .block(Block::bordered().title(" responses ")),
        right,
    );

    // Leave out the current (incomplete) second.
    let full = &s.per_sec[..s.per_sec.len().saturating_sub(1)];
    let width = spark.width.saturating_sub(2) as usize;
    let tail = &full[full.len().saturating_sub(width)..];
    f.render_widget(
        Sparkline::default().block(Block::bordered().title(" requests per second ")).data(tail).style(Style::default().fg(Color::Cyan)),
        spark,
    );
    f.render_widget(Line::from(" q / Esc / Ctrl-C: stop").dim(), help);
}

/// Blocking UI loop; returns when `finished` is set or the user quits (then sets `cancel`).
pub fn run(cfg: Arc<Config>, rec: Arc<Recorder>, finished: Arc<AtomicBool>, cancel: Arc<AtomicBool>) -> std::io::Result<()> {
    let mut term = ratatui::try_init()?;
    let result = (|| -> std::io::Result<()> {
        while !finished.load(Relaxed) {
            let snap = rec.snapshot();
            term.draw(|f| draw(f, &cfg, &snap))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                    if k.kind == KeyEventKind::Press && (ctrl_c || matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)) {
                        cancel.store(true, Relaxed);
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Outcome;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_numbers_into_a_test_backend() {
        let cfg = Config::new("http://127.0.0.1:8080/".parse().unwrap());
        let rec = Recorder::new();
        for i in 0..100 {
            let o = if i < 98 { Outcome::Response { status: 200, bytes: 1 } } else { Outcome::Error("timeout".into()) };
            rec.record(Duration::from_millis(1 + i), o);
        }
        let snap = rec.snapshot();
        let mut t = Terminal::new(TestBackend::new(90, 24)).unwrap();
        t.draw(|f| draw(f, &cfg, &snap)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("50%"), "progress label missing");
        assert!(text.contains("p99"));
        assert!(text.contains("200") && text.contains("98"));
        assert!(text.contains("timeout"));
        assert_eq!(progress(&cfg, &snap), 0.5);
    }
}
