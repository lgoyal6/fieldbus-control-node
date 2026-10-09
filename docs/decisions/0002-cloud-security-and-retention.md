# Cloud security and bounded retention

Administrative HTTP routes fail closed unless `FIELDBUS_ADMIN_TOKEN_SHA256` is set. A
local-only development process may explicitly opt into the old open behavior with
`FIELDBUS_ALLOW_INSECURE_ADMIN=1`; production compose does not set that flag. Exactly one
Authorization header is accepted and its SHA-256 digest is compared in constant time.
Registration and rotation responses never return the credential. The secret remains in the
append-only log for replay because this reference adapter has no encryption key management;
production deployment must encrypt the log and restrict filesystem access.

The cloud retains at most 100,000 unique readings in memory. Once full, ingest rejects the
whole batch with conflict and the device keeps readings in its bounded ring for retry. This
is backpressure, not silent dropping. The event log remains append-only and therefore still
needs operational rotation or archival before long-running production use. Firmware metadata
is descriptive only: no remote flashing or execution is implemented.
