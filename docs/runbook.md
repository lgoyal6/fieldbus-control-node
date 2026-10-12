# Operations runbook

## Device is stale

Query `/health/{device_id}` and check `last_seen_ms`, `last_sequence`, and `boot_counter`. If the sequence stopped while the boot counter stayed constant, inspect Wi-Fi, DNS, and the device ring age. If the boot counter changed, inspect the power rail and flash write diagnostics. A stale device should keep sampling locally until its bounded ring is full.

## Ingest rejects requests

`401` means an unknown device or signature mismatch. Rotate the credential only after verifying the device identity, then update the device's provisioned secret. `400` means a contract, quality, value, protocol, or batch-size violation. The cloud does not partially apply a rejected batch.

## Server restart

Stop the process, preserve the event log, and restart with the same path. Startup replay is the recovery operation. Compare the dashboard and per-device reading count before and after restart. Never truncate the log during diagnosis.

## Credential expiry and rotation

Credential rotation is a server-side record transition. A production transport should support an overlap window with the previous credential; this local reference keeps one active credential and makes expiry an explicit rejected-ingest fault for simulator tests.


## Telemetry and storage growth

`GET /metrics` exports Prometheus text. `fieldbus_readings_stored`, device counts, and
`fieldbus_log_bytes` are derived from the replayed durable store; request and failure counters
reset when the process restarts. `fieldbus_http_rejected_total{reason=...}` breaks failures
into bounded `invalid`, `unauthorized`, `conflict`, `not_found`, and `io` causes. No device IDs,
secrets, or readings appear in metric labels.
Structured request logs contain method, outcome, and duration, with no request body.

Use `infra/monitoring/prometheus.yml`, `alerts.yml`, and `dashboard.json` with your monitoring
stack. Point the `cloud:8080` target at the service's private address. Scrape failure, stale
devices, rejected request rate, and storage growth have separate alerts.

### Storage growth

Stop the reference service, back up its event log, and check free disk space. This implementation
retains all readings in memory and in the event log, so it requires explicit retention before a
long-running deployment. Do not truncate an active log. A real device ring pressure metric needs
a protocol extension reporting device backlog and drops; this cloud does not invent that value.

### Verification

Run `python3 scripts/cloud_http_smoke.py`. It starts one ephemeral process, confirms HTTP request
framing with a normal client, observes a rejected batch, restarts on the same event log, and
checks durable telemetry. All child processes are terminated after the gate.

## Local monitoring deployment

`docker compose -f infra/compose.yml up --build -d` runs the cloud, Prometheus, and a provisioned Grafana dashboard on loopback ports 8080, 9090, and 3000. Grafana's anonymous account is read-only. Use `docker compose -f infra/compose.yml down` to stop; append `-v` only when deliberately deleting local readings and monitoring history.

`bash scripts/check-monitoring.sh` checks PromQL and alert behavior for missing targets, stale devices, ingest failures, large logs, and unavailable storage. `fieldbus_store_up` reports whether event log metadata is readable; an error yields a NaN size instead of a false zero. This is a metadata health signal, not proof that a write will succeed. Request duration includes parsing and routing before response transmission.

This remains a local reference deployment. Set `FIELDBUS_ADMIN_TOKEN_SHA256` to require a bearer token for administrative endpoints (`/register`, `/rotate-credential`, `/firmware`, and `/config`); the local smoke test leaves it unset. Alertmanager paging delivery is not configured. Thresholds need real deployment calibration; hardware timing, device queue pressure, and power behavior are not measured by these cloud metrics.
