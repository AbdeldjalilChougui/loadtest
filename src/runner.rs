//! The load generators.
//!
//! **Closed model** (`rate: None`): `concurrency` workers each send a request, wait for the
//! answer, and send the next. Latency is measured from send to last body byte. If the server
//! stalls, the workers stall too and fewer requests are sent: the classic coordinated
//! omission problem, so tail latency is underestimated.
//!
//! **Open model** (`rate: Some(r)`): request *i* is scheduled at `start + i/r` regardless of
//! how earlier requests are doing. Latency is measured from the **scheduled** time, so time
//! spent waiting for a free connection (at most `concurrency` in flight) counts against the
//! server. A stall shows up in every request that should have been sent during it. The
//! dispatcher never waits for responses, so it keeps the schedule while the server stalls.

use crate::stats::{Outcome, Recorder};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Request, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stop {
    Requests(u64),
    Duration(Duration),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub url: Uri,
    pub method: Method,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
    pub concurrency: usize,
    pub stop: Stop,
    /// Requests per second for the open model; `None` for the closed model.
    pub rate: Option<f64>,
    pub timeout: Duration,
}

impl Config {
    pub fn new(url: Uri) -> Self {
        Config {
            url,
            method: Method::GET,
            headers: Vec::new(),
            body: Bytes::new(),
            concurrency: 50,
            stop: Stop::Requests(200),
            rate: None,
            timeout: Duration::from_secs(20),
        }
    }
}

type HttpClient = Client<HttpConnector, Full<Bytes>>;

fn client(cfg: &Config) -> HttpClient {
    let mut conn = HttpConnector::new();
    conn.set_nodelay(true);
    conn.set_connect_timeout(Some(cfg.timeout));
    Client::builder(TokioExecutor::new()).pool_max_idle_per_host(cfg.concurrency).build(conn)
}

/// Send one request and read the whole body.
async fn send_one(client: &HttpClient, cfg: &Config) -> Outcome {
    let mut req = Request::builder().method(cfg.method.clone()).uri(cfg.url.clone());
    for (k, v) in &cfg.headers {
        req = req.header(k, v);
    }
    let req = req.body(Full::new(cfg.body.clone())).expect("valid request");
    let work = async {
        let resp = client.request(req).await.map_err(|e| {
            if e.is_connect() {
                "connect".to_string()
            } else {
                "request".to_string()
            }
        })?;
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.map_err(|_| "body".to_string())?;
        Ok::<_, String>(Outcome::Response { status, bytes: body.to_bytes().len() as u64 })
    };
    match tokio::time::timeout(cfg.timeout, work).await {
        Err(_) => Outcome::Error("timeout".into()),
        Ok(Err(kind)) => Outcome::Error(kind),
        Ok(Ok(o)) => o,
    }
}

/// Run the test to completion (or until `cancel` is set). Results go into `rec`.
pub async fn run(cfg: Arc<Config>, rec: Arc<Recorder>, cancel: Arc<AtomicBool>) {
    match cfg.rate {
        None => run_closed(cfg, rec, cancel).await,
        Some(r) => run_open(cfg, rec, cancel, r).await,
    }
}

async fn run_closed(cfg: Arc<Config>, rec: Arc<Recorder>, cancel: Arc<AtomicBool>) {
    let client = client(&cfg);
    let issued = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + if let Stop::Duration(d) = cfg.stop { d } else { Duration::ZERO };
    let mut set = JoinSet::new();
    for _ in 0..cfg.concurrency.max(1) {
        let (cfg, rec, cancel, client, issued) = (cfg.clone(), rec.clone(), cancel.clone(), client.clone(), issued.clone());
        set.spawn(async move {
            while !cancel.load(Relaxed) {
                match cfg.stop {
                    Stop::Requests(n) if issued.fetch_add(1, Relaxed) >= n => break,
                    Stop::Duration(_) if Instant::now() >= deadline => break,
                    _ => {}
                }
                let t = Instant::now();
                let o = send_one(&client, &cfg).await;
                rec.record(t.elapsed(), o);
            }
        });
    }
    while set.join_next().await.is_some() {}
}

async fn run_open(cfg: Arc<Config>, rec: Arc<Recorder>, cancel: Arc<AtomicBool>, rate: f64) {
    let client = client(&cfg);
    let slots = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
    let start = Instant::now();
    let mut set = JoinSet::new();
    let mut i: u64 = 0;
    loop {
        let offset = Duration::from_secs_f64(i as f64 / rate);
        match cfg.stop {
            Stop::Requests(n) if i >= n => break,
            Stop::Duration(d) if offset >= d => break,
            _ => {}
        }
        if cancel.load(Relaxed) {
            break;
        }
        tokio::time::sleep_until(start + offset).await;
        // The clock starts when the dispatcher fires. Tokio timers wake up to ~1 ms late;
        // that slop is ours, not the server's, so it is left out. What does count is any
        // wait for a free slot below, which only happens when the server is slow.
        let scheduled = Instant::now();
        let (cfg, rec, client, slots) = (cfg.clone(), rec.clone(), client.clone(), slots.clone());
        set.spawn(async move {
            let _permit = slots.acquire_owned().await.expect("semaphore closed");
            let o = send_one(&client, &cfg).await;
            rec.record(scheduled.elapsed(), o);
        });
        i += 1;
        // Reap finished tasks so the set does not grow without bound on long runs.
        while set.try_join_next().is_some() {}
    }
    while set.join_next().await.is_some() {}
}
