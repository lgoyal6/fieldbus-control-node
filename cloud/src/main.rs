use fieldbus_cloud::{ConfigMetadata, DeviceSimulator, Error, FirmwareMetadata, Store, MAX_BATCH};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

fn response(mut s: TcpStream, status: &str, body: String) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
}
fn json<T: serde::Serialize>(v: T) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"error\":\"serialization\"}".into())
}
fn handle(mut stream: TcpStream, mut store: Store) -> Store {
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let mut line = head.lines().next().unwrap_or("").split_whitespace();
    let method = line.next().unwrap_or("");
    let path = line.next().unwrap_or("");
    let result: Result<(String, String), Error> = match (method, path) {
        ("GET", "/dashboard") => Ok(("200 OK".into(), json(store.dashboard(now(), 120_000)))),
        ("GET", p) if p.starts_with("/health/") => store
            .health(&p[8..], now(), 120_000)
            .map(|h| ("200 OK".into(), json(h))),
        ("POST", "/register") => serde_json::from_str::<serde_json::Value>(body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|v| {
                store.register(
                    v["device_id"].as_str().unwrap_or(""),
                    v["secret_hex"].as_str().unwrap_or(""),
                    v["firmware_version"].as_str().unwrap_or(""),
                )
            })
            .map(|d| ("201 Created".into(), json(d))),
        ("POST", "/rotate-credential") => serde_json::from_str::<serde_json::Value>(body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|v| {
                store.rotate_credential(
                    v["device_id"].as_str().unwrap_or(""),
                    v["secret_hex"].as_str().unwrap_or(""),
                )
            })
            .map(|d| ("200 OK".into(), json(d))),
        ("POST", "/ingest") => serde_json::from_str::<Vec<fieldbus_cloud::Envelope>>(body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|b| {
                if b.len() <= MAX_BATCH {
                    store.ingest(&b, now())
                } else {
                    Err(Error::Invalid("batch too large".into()))
                }
            })
            .map(|n| ("200 OK".into(), json(serde_json::json!({"accepted": n})))),
        ("POST", "/firmware") => serde_json::from_str::<FirmwareMetadata>(body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|m| store.publish_firmware(m))
            .map(|_| ("201 Created".into(), "{}".into())),
        ("POST", "/config") => serde_json::from_str::<ConfigMetadata>(body)
            .map_err(|e| Error::Invalid(e.to_string()))
            .and_then(|m| store.publish_config(m))
            .map(|_| ("201 Created".into(), "{}".into())),
        _ => Err(Error::NotFound),
    };
    match result {
        Ok((status, body)) => response(stream, &status, body),
        Err(e) => response(
            stream,
            match e {
                Error::NotFound => "404 Not Found",
                Error::Unauthorized => "401 Unauthorized",
                Error::Conflict => "409 Conflict",
                _ => "400 Bad Request",
            },
            json(serde_json::json!({"error": e.to_string()})),
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
            for conn in listener.incoming() {
                if let Ok(stream) = conn {
                    store = handle(stream, store);
                }
            }
        }
    }
}
