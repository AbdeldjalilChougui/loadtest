//! A local target for trying loadtest: `cargo run --release --example hello_server -- 8080`
//! Routes: `/` → "hello", `/slow` → 20 ms delay, `/stall` → freezes for 1 s every 5 s.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    let t0 = Instant::now();
    println!("listening on http://127.0.0.1:{port}");
    loop {
        let (s, _) = l.accept().await.expect("accept");
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| async move {
                match req.uri().path() {
                    "/slow" => tokio::time::sleep(Duration::from_millis(20)).await,
                    "/stall" => {
                        let phase = t0.elapsed().as_millis() % 5000;
                        if phase < 1000 {
                            tokio::time::sleep(Duration::from_millis((1000 - phase) as u64)).await;
                        }
                    }
                    _ => {}
                }
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hello"))))
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(s), svc).await;
        });
    }
}
