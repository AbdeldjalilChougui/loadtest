//! Runs the load generator against a local hyper server.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use loadtest::{Config, Recorder, Report, Stop};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

struct ServerState {
    hits: AtomicU64,
    /// Requests arriving in [stall_from, stall_to) are held until stall_to.
    stall: Option<(Instant, Instant)>,
}

async fn handle(req: Request<Incoming>, st: Arc<ServerState>) -> Result<Response<Full<Bytes>>, Infallible> {
    let n = st.hits.fetch_add(1, Relaxed);
    let path = req.uri().path().to_string();
    let reply = |code: u16, body: &'static str| {
        Ok(Response::builder().status(StatusCode::from_u16(code).unwrap()).body(Full::new(Bytes::from(body))).unwrap())
    };
    match path.as_str() {
        "/ok" => reply(200, "ok"),
        "/cycle" => reply([200, 404, 503][(n % 3) as usize], "cycle"),
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            reply(200, "slow")
        }
        "/stall" => {
            if let Some((from, to)) = st.stall {
                let now = Instant::now();
                if now >= from && now < to {
                    tokio::time::sleep(to - now).await;
                }
            }
            reply(200, "s")
        }
        "/echo" => {
            let ok_header = req.headers().get("x-test").is_some_and(|v| v == "yes");
            let ok_method = req.method() == Method::POST;
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if ok_header && ok_method && body == "ping" {
                reply(200, "pong")
            } else {
                reply(400, "bad")
            }
        }
        _ => reply(404, "nope"),
    }
}

async fn server(stall: Option<(Duration, Duration)>) -> (SocketAddr, Arc<ServerState>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let t0 = Instant::now();
    let st = Arc::new(ServerState { hits: AtomicU64::new(0), stall: stall.map(|(a, b)| (t0 + a, t0 + b)) });
    let st2 = st.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let st = st2.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |r| handle(r, st.clone()));
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(s), svc).await;
            });
        }
    });
    (addr, st)
}

fn cfg(addr: SocketAddr, path: &str) -> Config {
    Config::new(format!("http://{addr}{path}").parse().unwrap())
}

async fn go(c: Config) -> Report {
    let c = Arc::new(c);
    let rec = Arc::new(Recorder::new());
    loadtest::run(c.clone(), rec.clone(), Arc::new(AtomicBool::new(false))).await;
    Report::new(&c, &rec.snapshot())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixed_count_closed_model() {
    let (addr, st) = server(None).await;
    let mut c = cfg(addr, "/ok");
    c.concurrency = 16;
    c.stop = Stop::Requests(2000);
    let r = go(c).await;
    assert_eq!(r.requests, 2000);
    assert_eq!(r.status_codes["200"], 2000);
    assert_eq!(r.errors, 0);
    assert_eq!(r.bytes, 4000);
    assert_eq!(st.hits.load(Relaxed), 2000, "exactly n requests reach the server");
    assert_eq!(r.histogram.iter().map(|b| b.count).sum::<u64>(), 2000);
    assert!(r.latency_ms.p50 <= r.latency_ms.p99 && r.latency_ms.p99 <= r.latency_ms.max);
    assert!(r.requests_per_sec > 100.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_code_breakdown() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/cycle");
    c.concurrency = 4;
    c.stop = Stop::Requests(300);
    let r = go(c).await;
    assert_eq!(r.status_codes["200"], 100);
    assert_eq!(r.status_codes["404"], 100);
    assert_eq!(r.status_codes["503"], 100);
    let text = r.to_text();
    assert!(text.contains("[404] 100") && text.contains("[503] 100"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duration_mode_stops_on_time() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/slow");
    c.concurrency = 5;
    c.stop = Stop::Duration(Duration::from_millis(500));
    let t = Instant::now();
    let r = go(c).await;
    let el = t.elapsed();
    assert!(el >= Duration::from_millis(500) && el < Duration::from_millis(900), "{el:?}");
    // 5 workers * ~5 requests of 100 ms each.
    assert!((20..=35).contains(&r.requests), "{}", r.requests);
    assert!(r.latency_ms.p50 >= 100.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_model_hits_the_target_rate() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/ok");
    c.rate = Some(400.0);
    c.concurrency = 20;
    c.stop = Stop::Duration(Duration::from_secs(1));
    let r = go(c).await;
    assert_eq!(r.requests, 400, "one request per 2.5 ms for 1 s");
    assert!((0.9..1.2).contains(&r.duration_secs), "{}", r.duration_secs);
    assert!((330.0..450.0).contains(&r.requests_per_sec), "{}", r.requests_per_sec);
    assert_eq!(r.model, "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_model_exposes_a_stall_that_the_closed_model_hides() {
    // The server freezes for 500 ms, 300 ms into the run.
    let stall = Some((Duration::from_millis(300), Duration::from_millis(800)));

    let (addr, _) = server(stall).await;
    let mut closed = cfg(addr, "/stall");
    closed.concurrency = 1;
    closed.stop = Stop::Duration(Duration::from_millis(1500));
    let closed = go(closed).await;

    let (addr, _) = server(stall).await;
    let mut open = cfg(addr, "/stall");
    open.concurrency = 1;
    open.rate = Some(100.0);
    open.stop = Stop::Duration(Duration::from_millis(1500));
    let open = go(open).await;

    // Both saw one ~500 ms response...
    assert!(closed.latency_ms.max >= 400.0, "closed max {}", closed.latency_ms.max);
    assert!(open.latency_ms.max >= 400.0, "open max {}", open.latency_ms.max);
    // ...but the closed model sent nothing while it waited, so its percentiles look healthy.
    assert!(closed.latency_ms.p99 < 50.0, "closed p99 {}", closed.latency_ms.p99);
    // The open model kept its schedule: ~50 of ~150 requests were due during the stall.
    assert!(open.latency_ms.p75 > 50.0, "open p75 {}", open.latency_ms.p75);
    assert!(open.requests >= 145, "open sent {}", open.requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeouts_and_connection_errors_are_classified() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/slow");
    c.timeout = Duration::from_millis(20);
    c.concurrency = 2;
    c.stop = Stop::Requests(6);
    let r = go(c).await;
    assert_eq!(r.error_kinds["timeout"], 6);
    assert_eq!(r.responses, 0);

    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut c = cfg(dead, "/");
    c.concurrency = 2;
    c.stop = Stop::Requests(4);
    let r = go(c).await;
    assert_eq!(r.error_kinds["connect"], 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_headers_and_body_are_sent() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/echo");
    c.method = Method::POST;
    c.headers.push((HeaderName::from_static("x-test"), HeaderValue::from_static("yes")));
    c.body = Bytes::from_static(b"ping");
    c.concurrency = 3;
    c.stop = Stop::Requests(30);
    let r = go(c).await;
    assert_eq!(r.status_codes.get("200"), Some(&30), "{:?}", r.status_codes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_report_has_the_documented_fields() {
    let (addr, _) = server(None).await;
    let mut c = cfg(addr, "/ok");
    c.stop = Stop::Requests(50);
    c.concurrency = 5;
    let v: serde_json::Value = serde_json::from_str(&go(c).await.to_json()).unwrap();
    for k in ["url", "model", "requests", "errors", "requests_per_sec", "latency_ms", "histogram", "status_codes", "timeline"] {
        assert!(v.get(k).is_some(), "missing {k}");
    }
    for k in ["min", "mean", "p50", "p90", "p99", "p999", "max"] {
        assert!(v["latency_ms"][k].is_number(), "missing latency_ms.{k}");
    }
    assert_eq!(v["requests"], 50);
}
