# Operations runbook

## Device is stale

Query `/health/{device_id}` and check `last_seen_ms`, `last_sequence`, and `boot_counter`. If the sequence stopped while the boot counter stayed constant, inspect Wi-Fi, DNS, and the device ring age. If the boot counter changed, inspect the power rail and flash write diagnostics. A stale device should keep sampling locally until its bounded ring is full.

## Ingest rejects requests

`401` means an unknown device or signature mismatch. Rotate the credential only after verifying the device identity, then update the device's provisioned secret. `400` means a contract, quality, value, protocol, or batch-size violation. The cloud does not partially apply a rejected batch.

## Server restart

Stop the process, preserve the event log, and restart with the same path. Startup replay is the recovery operation. Compare the dashboard and per-device reading count before and after restart. Never truncate the log during diagnosis.

## Credential expiry and rotation

Credential rotation is a server-side record transition. A production transport should support an overlap window with the previous credential; this local reference keeps one active credential and makes expiry an explicit rejected-ingest fault for simulator tests.

