use bytes::Bytes;
use clap::Parser;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Uri};
use loadtest::{parse_duration, Config, Recorder, Report, Stop};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// HTTP load tester. Closed model by default (-c workers back to back); with --rate it
/// switches to an open model that schedules requests on a fixed clock and measures latency
/// from the scheduled time, so server stalls are not hidden (no coordinated omission).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Target URL (http:// only).
    url: String,
    /// Total number of requests (default 200 when -z is not given).
    #[arg(short = 'n', long)]
    requests: Option<u64>,
    /// Run for this long instead of a fixed count, e.g. 30s, 2m.
    #[arg(short = 'z', long, value_parser = parse_duration)]
    duration: Option<Duration>,
    /// Concurrent connections (closed model) or max requests in flight (open model).
    #[arg(short = 'c', long, default_value_t = 50)]
    concurrency: usize,
    /// Target requests per second: switches to the open model.
    #[arg(short = 'q', long)]
    rate: Option<f64>,
    /// HTTP method.
    #[arg(short = 'm', long, default_value = "GET")]
    method: String,
    /// Extra header "Name: value" (repeatable).
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Request body.
    #[arg(short = 'd', long)]
    body: Option<String>,
    /// Read the request body from a file.
    #[arg(short = 'D', long)]
    body_file: Option<std::path::PathBuf>,
    /// Per-request timeout.
    #[arg(short = 't', long, default_value = "20s", value_parser = parse_duration)]
    timeout: Duration,
    /// Print the report as JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Also write the JSON report to this file.
    #[arg(short = 'o', long)]
    output: Option<std::path::PathBuf>,
    /// Never show the live TUI (it is off anyway when stdout is not a terminal).
    #[arg(long)]
    no_tui: bool,
}

fn build(a: &Args) -> Result<Config, String> {
    let url: Uri = a.url.parse().map_err(|e| format!("bad URL {:?}: {e}", a.url))?;
    if url.scheme_str() != Some("http") {
        return Err("only http:// URLs are supported (no TLS)".into());
    }
    let mut cfg = Config::new(url);
    cfg.method = Method::from_bytes(a.method.to_uppercase().as_bytes()).map_err(|_| format!("bad method {:?}", a.method))?;
    for h in &a.headers {
        let (k, v) = h.split_once(':').ok_or_else(|| format!("header {h:?} must be 'Name: value'"))?;
        let k = HeaderName::from_bytes(k.trim().as_bytes()).map_err(|_| format!("bad header name in {h:?}"))?;
        let v = HeaderValue::from_str(v.trim()).map_err(|_| format!("bad header value in {h:?}"))?;
        cfg.headers.push((k, v));
    }
    cfg.body = match (&a.body, &a.body_file) {
        (Some(_), Some(_)) => return Err("use either -d or -D".into()),
        (Some(b), None) => Bytes::from(b.clone()),
        (None, Some(p)) => Bytes::from(std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?),
        (None, None) => Bytes::new(),
    };
    cfg.concurrency = a.concurrency.max(1);
    cfg.stop = match (a.requests, a.duration) {
        (Some(_), Some(_)) => return Err("use either -n or -z".into()),
        (Some(n), None) => Stop::Requests(n),
        (None, Some(d)) => Stop::Duration(d),
        (None, None) => Stop::Requests(200),
    };
    if let Some(r) = a.rate {
        if !(r.is_finite() && r > 0.0) {
            return Err("--rate must be positive".into());
        }
        cfg.rate = Some(r);
    }
    cfg.timeout = a.timeout;
    Ok(cfg)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let cfg = match build(&args) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let rec = Arc::new(Recorder::new());
    let cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let use_tui = !args.no_tui && !args.json && std::io::stdout().is_terminal();

    let ui = if use_tui {
        let (c, r, f, x) = (cfg.clone(), rec.clone(), finished.clone(), cancel.clone());
        Some(std::thread::spawn(move || loadtest::tui::run(c, r, f, x)))
    } else {
        let x = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                x.store(true, Relaxed);
            }
        });
        None
    };

    loadtest::run(cfg.clone(), rec.clone(), cancel.clone()).await;
    finished.store(true, Relaxed);
    if let Some(h) = ui {
        if let Ok(Err(e)) = h.join() {
            eprintln!("tui: {e}");
        }
    }

    let report = Report::new(&cfg, &rec.snapshot());
    if let Some(p) = &args.output {
        if let Err(e) = std::fs::write(p, report.to_json()) {
            eprintln!("error: writing {}: {e}", p.display());
        }
    }
    if args.json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_text());
    }
    if cancel.load(Relaxed) {
        eprintln!("(stopped early)");
    }
}
