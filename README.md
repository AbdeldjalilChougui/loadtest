# loadtest

An HTTP load tester in the spirit of `wrk`, `wrk2` and `oha`, on Tokio + hyper. It runs
either a fixed number of connections back to back (**closed model**) or a fixed request
rate (**open model**) that does not hide server stalls (no coordinated omission). Latency
goes into an HDR histogram; you get percentiles up to p99.9, a status-code breakdown,
error classes, a live ratatui view while it runs and a JSON report at the end.

## Who it is for

Backend developers who want a single small binary to answer "how many requests per
second, and what does the tail look like?" for a local or staging HTTP service, and people
curious about why closed-loop benchmarks lie about tail latency.

## Install

Prebuilt binaries (Linux x86_64/aarch64, macOS x86_64/arm64, Windows x86_64) are
attached to every [GitHub release](https://github.com/AbdeldjalilChougui/loadtest/releases), with installers:

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AbdeldjalilChougui/loadtest/releases/latest/download/loadtest-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/AbdeldjalilChougui/loadtest/releases/latest/download/loadtest-installer.ps1 | iex"
```

Or build from crates.io:

```sh
cargo install loadtest --locked
```

## Quick start

```sh
cargo build --release
cargo run --release --example hello_server -- 8080 &      # a local target

loadtest http://127.0.0.1:8080/ -n 50000 -c 50            # closed model, 50 connections
loadtest http://127.0.0.1:8080/ -z 30s -q 2000 -c 200     # open model, 2000 req/s
loadtest http://127.0.0.1:8080/api -m POST -H 'Content-Type: application/json' \
         -d '{"x":1}' -n 1000 --json > report.json
```

| flag | meaning |
|---|---|
| `-n N` / `-z 30s` | stop after N requests (default 200) or after a duration |
| `-c N` | connections (closed) or max requests in flight (open), default 50 |
| `-q R` | target rate in requests/s: switches to the open model |
| `-m`, `-H 'K: V'`, `-d`, `-D file` | method, headers, body, body from file |
| `-t 20s` | per-request timeout (counted as error `timeout`) |
| `--json`, `-o file` | JSON report on stdout and/or to a file |
| `--no-tui` | no live view (also off when stdout is not a terminal) |

In a terminal the live view shows progress, requests/s (last second and overall),
p50/p90/p99/p99.9/max, status codes and errors, and a requests-per-second sparkline;
`q`, `Esc` or `Ctrl-C` stops early and still prints the report.

## Sample output

Closed model against the bundled `hello_server` on an 8-thread laptop (client and server
on the same machine):

```
GET http://127.0.0.1:18181/ (closed model, 50 connections)

Summary:
  Requests:      50000 (0 errors)
  Duration:      1.152 s
  Requests/sec:  43407.5
  Transfer:      244.14 KiB (211.95 KiB/s)

Latency (ms):
  min 0.096  mean 1.149  stdev 0.517  max 10.183
  p50 1.065  p75 1.411  p90 1.789  p95 2.073  p99 2.831  p99.9 3.949

Latency histogram:
       1.105 ms [   26706] |■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■
       2.114 ms [   21038] |■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■
       3.123 ms [    1986] |■■■
       4.132 ms [     233] |■
       5.141 ms [      26] |■
       6.150 ms [       5] |■
       7.159 ms [       5] |■
       8.168 ms [       0] |
       9.177 ms [       0] |
      10.183 ms [       1] |■

Status codes:
  [200] 50000
```

### Why the open model matters

`/stall` on `hello_server` freezes for one second every five seconds. Six seconds with one
connection, closed model:

```
  min 0.088  mean 0.171  stdev 5.369  max 1001.983
  p50 0.130  p75 0.159  p90 0.195  p95 0.218  p99 0.272  p99.9 1.550
```

Six seconds at 200 req/s, open model:

```
  min 0.287  mean 105.567  stdev 228.500  max 1000.959
  p50 0.504  p75 4.053  p90 450.559  p95 702.975  p99 941.567  p99.9 996.863
```

Both saw the one-second freeze (max ~1000 ms). The closed run sent nothing while its only
connection was stuck, so the freeze is a single sample out of 34 810 and p99 looks perfect.
The open run kept its schedule, so every request that was due during a freeze
waited, and p90/p95/p99 show what users would actually have felt.

The JSON report (`--json`) has the same numbers plus the histogram buckets and a
per-second timeline:

```json
{
  "url": "http://127.0.0.1:18181/slow", "method": "GET", "model": "closed",
  "concurrency": 2, "target_rate": null, "duration_secs": 0.2175,
  "requests": 20, "responses": 20, "errors": 0, "requests_per_sec": 91.9,
  "bytes": 100, "bytes_per_sec": 459.7,
  "latency_ms": { "min": 20.448, "mean": 21.68, "stdev": 0.479, "max": 22.463,
                  "p50": 21.759, "p75": 21.935, "p90": 22.207, "p95": 22.319,
                  "p99": 22.463, "p999": 22.463 },
  "histogram": [ { "le_ms": 20.65, "count": 1 }, ... ],
  "status_codes": { "200": 20 }, "error_kinds": {}, "timeline": [20]
}
```

## How it works

- `runner.rs`: one `hyper_util` pooled client. **Closed:** `c` tasks loop "send, read the
  whole body, record". **Open:** a dispatcher sleeps until `start + i/rate` and spawns
  request *i*; the request waits for one of `c` slots and its latency is measured from
  when the dispatcher fired, so slot waiting (the server being slow) counts and Tokio's
  ~1 ms timer slop does not. The dispatcher never waits for responses, so it keeps the
  schedule during a stall.
- `stats.rs`: a mutex-protected `hdrhistogram` (1 µs to 1 h, 3 significant digits),
  status and error counters, bytes, and per-second completions.
- `report.rs`: text and JSON reports, 10 equal-width histogram buckets.
- `tui.rs`: ratatui view redrawn every 100 ms from a snapshot; it runs on its own thread.

## Tests

`cargo test`: 5 unit tests (recorder, report/JSON, histogram buckets add up, duration
parsing, TUI rendered into ratatui's `TestBackend`) and 8 integration tests against a
local hyper server: exactly *n* requests reach the server, status-code breakdown
(200/404/503 thirds), duration mode stops on time, the open model hits its target rate,
**the open model exposes a 500 ms stall that the closed model's p99 hides**, timeout and
connection-refused classification, method/headers/body are sent, and the JSON fields.

## Status / limitations

- `http://` only: no TLS, no HTTP/2, no HTTP/3.
- One URL per run (no request files, no scripting).
- The open model spawns a task per request; at very high rates on one machine the
  generator itself becomes the bottleneck and latencies climb (on the laptop above it
  holds 5 000 req/s with p99 around 4 ms, but not 20 000).
- Latency includes reading the full body; connection setup is included for new
  connections. There is no warm-up phase and no per-phase breakdown (DNS/connect/TTFB).
- Recording takes a mutex per request; fine up to tens of thousands of requests/s.

## License

MIT, see [LICENSE](LICENSE).
