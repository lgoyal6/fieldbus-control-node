# Device to cloud boundaries

## Decisions

- The device contract is protocol version 1 and carries a device id, firmware version, monotonically increasing sequence within an authenticated boot session, device timestamp, boot counter, reading, quality, and HMAC-SHA256 signature.
- The boot counter identifies the session. The cloud deduplicates on `(device_id, boot_counter, sequence)` and accepts out-of-order batches. A duplicate is acknowledged without creating a second reading.
- The device keeps at most 256 unsent readings. When full it drops the oldest reading and increments a diagnostic counter. This is explicit data loss under sustained outage rather than unbounded memory growth.
- The cloud store is an append-only JSON-lines event log. `sync_data` is called before an ingest is acknowledged, and replay reconstructs readings, credentials, and health after restart.
- Firmware metadata is publish-only. Remote execution is out of scope until provisioning, rollback, signature verification, and failure recovery are documented and tested.
- Staleness is evaluated from server receipt time with a 120-second dashboard threshold. A device with no accepted reading is stale.

## Recovery invariants

1. A reading leaves the device ring only after the cloud accepts the complete batch.
2. A malformed, unauthenticated, or unsupported batch changes no cloud state.
3. Replaying the event log is idempotent for the same session and sequence.
4. A reboot increments the device boot counter and resets the session-local sequence to zero.

