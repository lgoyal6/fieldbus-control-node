use fieldbus_cloud::{ConfigMetadata, DeviceSimulator, Error, FirmwareMetadata, Store, MAX_BATCH};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn response(mut s: TcpStream, status: &str, body: String, content_type: &str) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
}
fn json<T: serde::Serialize>(v: T) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"error\":\"serialization\"}".into())
}
fn constant_time_hex_equal(actual: &[u8], expected: &str) -> bool {
    if expected.len() != 64 || !expected.is_ascii() {
        return false;
    }
    let actual_hex: String = actual.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut difference = 0u8;
    for (left, right) in actual_hex.as_bytes().iter().zip(expected.as_bytes()) {
        difference |= *left ^ *right;
    }
    actual_hex.len() == expected.len() && difference == 0
}

fn admin_authorized(head: &str) -> bool {
    let expected = match std::env::var("FIELDBUS_ADMIN_TOKEN_SHA256") {
        Ok(value) if !value.is_empty() => value,
        _ => return std::env::var("FIELDBUS_ALLOW_INSECURE_ADMIN").as_deref() == Ok("1"),
    };
    let values: Vec<&str> = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    .then_some(value.trim())
            })
        })
        .collect();
    if values.len() != 1 {
        return false;
    }
    let Some(token) = values[0].strip_prefix("Bearer ") else {
        return false;
    };
    let digest = Sha256::digest(token.as_bytes());
    constant_time_hex_equal(&digest, &expected)
}

fn public_device(d: fieldbus_cloud::DeviceRecord) -> serde_json::Value {
    serde_json::json!({
        "device_id": d.device_id,
        "credential_version": d.credential_version,
        "firmware_version": d.firmware_version,
        "config_version": d.config_version,
        "last_seen_ms": d.last_seen_ms,
        "last_sequence": d.last_sequence,
        "boot_counter": d.boot_counter,
    })
}

#[derive(Default)]
struct Metrics {
    requests: u64,
    failures: u64,
    accepted: u64,
    duration_seconds: f64,
    duration_count: u64,
}

impl Metrics {
    fn observe(&mut self, started: Instant) {
        self.duration_seconds += started.elapsed().as_secs_f64();
        self.duration_count += 1;
    }
}

fn read_request(stream: &mut TcpStream) -> Result<(String, String), Error> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        reader
            .by_ref()
            .take(8193)
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Io(e.to_string()))?;
        if line.is_empty() || line.len() > 8192 || head.len() + line.len() > 8192 {
            return Err(Error::Invalid("invalid or oversized headers".into()));
        }
        let end = line == b"\r\n";
        head.extend(line);
        if end {
            break;
        }
    }
    let head = String::from_utf8(head).map_err(|_| Error::Invalid("invalid headers".into()))?;
    let mut length = None;
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err(Error::Invalid("chunked requests unsupported".into()));
            }
            if name.eq_ignore_ascii_case("content-length") {
                if length.is_some() {
                    return Err(Error::Invalid("duplicate content length".into()));
                }
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| Error::Invalid("invalid content length".into()))?,
                );
            }
        }
    }
    let length = length.unwrap_or(0);
    if length > 65536 {
        return Err(Error::Invalid("request exceeds 64 KiB".into()));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| Error::Io(e.to_string()))?;
    Ok((
        head,
        String::from_utf8(body).map_err(|_| Error::Invalid("invalid JSON encoding".into()))?,
    ))
}

fn handle(mut stream: TcpStream, mut store: Store, metrics: &mut Metrics) -> Store {
    let started = Instant::now();
    metrics.requests += 1;
    let (head, body) = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => {
            metrics.failures += 1;
            metrics.observe(started);
            response(
                stream,
                "400 Bad Request",
                "{\"error\":\"invalid request\"}".into(),
                "application/json",
            );
            return store;
        }
    };
    let mut line = head.lines().next().unwrap_or("").split_whitespace();
    let method = line.next().unwrap_or("");
    let path = line.next().unwrap_or("");
    if matches!(
        (method, path),
        ("POST", "/register")
            | ("POST", "/rotate-credential")
            | ("POST", "/firmware")
            | ("POST", "/config")
    ) && !admin_authorized(&head)
    {
        metrics.failures += 1;
        metrics.observe(started);
        response(
            stream,
            "401 Unauthorized",
            "{\"error\":\"admin authentication required\"}".into(),
            "application/json",
        );
        return store;
    }
    if method == "GET" && path == "/metrics" {
        let health = store.dashboard(now(), 120_000);
        let stale = health.iter().filter(|h| h.stale).count();
        let log_bytes = store.log_bytes();
        let store_up = u8::from(log_bytes.is_ok());
        // An unreadable log is unavailable, not an empty healthy log.
        let bytes = log_bytes
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "NaN".into());
        metrics.observe(started);
        let total = store.reading_count();
        let body = format!("# TYPE fieldbus_http_requests_total counter\nfieldbus_http_requests_total {}\n# TYPE fieldbus_http_failures_total counter\nfieldbus_http_failures_total {}\n# TYPE fieldbus_readings_stored gauge\nfieldbus_readings_stored {total}\n# TYPE fieldbus_devices_stale gauge\nfieldbus_devices_stale {stale}\n# TYPE fieldbus_devices_registered gauge\nfieldbus_devices_registered {}\n# TYPE fieldbus_log_bytes gauge\nfieldbus_log_bytes {bytes}\n# TYPE fieldbus_store_up gauge\nfieldbus_store_up {store_up}\n# TYPE fieldbus_ingest_accepted_total counter\nfieldbus_ingest_accepted_total {}\n# TYPE fieldbus_http_duration_seconds summary\nfieldbus_http_duration_seconds_sum {}\nfieldbus_http_duration_seconds_count {}\n", metrics.requests, metrics.failures, health.len(), metrics.accepted, metrics.duration_seconds, metrics.duration_count);
        response(stream, "200 OK", body, "text/plain; version=0.0.4");
        return store;
    }
    let result: Result<(String, String), Error> = match (method, path) {
        ("GET", "/healthz") => Ok(("200 OK".into(), "{\"status\":\"ok\"}".into())),
        ("GET", "/dashboard") => Ok(("200 OK".into(), json(store.dashboard(now(), 120_000)))),
        ("GET", p) if p.starts_with("/health/") => store
            .health(&p[8..], now(), 120_000)
            .map(|h| ("200 OK".into(), json(h))),
        ("POST", "/register") => serde_json::from_str::<serde_json::Value>(&body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|v| {
                store.register(
                    v["device_id"].as_str().unwrap_or(""),
                    v["secret_hex"].as_str().unwrap_or(""),
                    v["firmware_version"].as_str().unwrap_or(""),
                )
            })
            .map(|d| ("201 Created".into(), json(public_device(d)))),
        ("POST", "/rotate-credential") => serde_json::from_str::<serde_json::Value>(&body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|v| {
                store.rotate_credential(
                    v["device_id"].as_str().unwrap_or(""),
                    v["secret_hex"].as_str().unwrap_or(""),
                )
            })
            .map(|d| ("200 OK".into(), json(public_device(d)))),
        ("POST", "/ingest") => serde_json::from_str::<Vec<fieldbus_cloud::Envelope>>(&body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|b| {
                if b.len() <= MAX_BATCH {
                    store.ingest(&b, now())
                } else {
                    Err(Error::Invalid("batch too large".into()))
                }
            })
            .map(|n| {
                metrics.accepted += n as u64;
                ("200 OK".into(), json(serde_json::json!({"accepted": n})))
            }),
        ("POST", "/firmware") => serde_json::from_str::<FirmwareMetadata>(&body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|m| store.publish_firmware(m))
            .map(|_| ("201 Created".into(), "{}".into())),
        ("POST", "/config") => serde_json::from_str::<ConfigMetadata>(&body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|m| store.publish_config(m))
            .map(|_| ("201 Created".into(), "{}".into())),
        _ => Err(Error::NotFound),
    };
    metrics.observe(started);
    let status = if result.is_ok() {
        "success"
    } else {
        "rejected"
    };
    if result.is_err() {
        metrics.failures += 1;
    }
    eprintln!(
        "{}",
        json(
            serde_json::json!({"event":"request", "method":method, "outcome":status, "duration_ms": started.elapsed().as_millis()})
        )
    );
    match result {
        Ok((status, body)) => response(stream, &status, body, "application/json"),
        Err(e) => response(
            stream,
            match e {
                Error::NotFound => "404 Not Found",
                Error::Unauthorized => "401 Unauthorized",
                Error::Conflict => "409 Conflict",
                _ => "400 Bad Request",
            },
            json(serde_json::json!({"error": e.to_string()})),
            "application/json",
        ),
    };
    store
}
fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("simulate") => {
            let p = PathBuf::from(args.next().unwrap_or_else(|| "cloud/events.log".into()));
            let mut s = Store::open(&p).expect("open store");
            let secret = "00112233445566778899aabbccddeeff";
            let _ = s.register("sim-1", secret, "1.0.0");
            let mut d = DeviceSimulator::new("sim-1", "1.0.0", secret, 1);
            for i in 0..10 {
                d.sample(i * 1000, 20.0 + i as f64, "ok");
            }
            let n = d.sync(|b| s.ingest(b, now())).expect("sync");
            println!(
                "accepted={n} pending={} dropped={}",
                d.pending(),
                d.dropped_when_full
            );
        }
        _ => {
            let addr = args.next().unwrap_or_else(|| "127.0.0.1:8080".into());
            let path = PathBuf::from(args.next().unwrap_or_else(|| "cloud/events.log".into()));
            let listener = TcpListener::bind(&addr).expect("bind");
            println!("fieldbus-cloud listening on {addr}");
            let mut store = Store::open(path).expect("open store");
            let mut metrics = Metrics::default();
            for stream in listener.incoming().flatten() {
                store = handle(stream, store, &mut metrics);
            }
        }
    }
}
