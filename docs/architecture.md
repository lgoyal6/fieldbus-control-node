# Embedded device to cloud architecture

The existing `fieldbus-core` crate is the allocation-free control plane. The new `fieldbus-cloud` crate supplies the edge-to-cloud contract and a deterministic host simulator. Its library contains the domain types, HMAC signing, append-only store, device ring buffer, and health projection. The binary is a small single-threaded HTTP adapter with these endpoints:

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/register` | Register a device and initial credential |
| POST | `/rotate-credential` | Replace the active device credential |
| POST | `/ingest` | Verify and durably accept a batch of readings |
| GET | `/health/{device_id}` | Return stale/online state and last sequence |
| GET | `/dashboard` | Return the health projection for all devices |
| POST | `/firmware` | Publish firmware metadata only |
| POST | `/config` | Publish validated sampling and stale thresholds |

Run `cargo run -p fieldbus-cloud -- simulate /tmp/fieldbus-events.log` for a deterministic offline, retry, and ingest demonstration. Run `cargo run -p fieldbus-cloud -- serve 127.0.0.1:8080 /tmp/fieldbus-events.log` for the local HTTP adapter.

The service is intentionally single-process and dependency-light. Production deployment should put TLS and an authenticated reverse proxy in front of it, rotate event-log files, restrict file permissions, and expose metrics for ingest rejection reasons, stale devices, ring overflow, retry age, and accepted latency.
