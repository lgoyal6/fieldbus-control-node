//! Device-to-cloud protocol, durable store, and deterministic device simulator.
//! The domain is transport independent; the binary supplies a small HTTP adapter.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_BATCH: usize = 64;
pub const MAX_RING: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reading {
    pub device_id: String,
    pub firmware_version: String,
    pub sequence: u64,
    pub device_timestamp_ms: i64,
    pub boot_counter: u64,
    pub value: f64,
    pub quality: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub protocol_version: u8,
    pub reading: Reading,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceRecord {
    pub device_id: String,
    pub credential_version: u64,
    pub secret_hex: String,
    pub firmware_version: String,
    pub config_version: u64,
    pub last_seen_ms: Option<i64>,
    pub last_sequence: Option<u64>,
    pub boot_counter: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirmwareMetadata {
    pub version: String,
    pub sha256: String,
    pub min_bootloader: String,
    pub rollback_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigMetadata {
    pub version: u64,
    pub sample_period_ms: u64,
    pub stale_after_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub device_id: String,
    pub online: bool,
    pub stale: bool,
    pub last_seen_ms: Option<i64>,
    pub last_sequence: Option<u64>,
    pub boot_counter: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Event {
    Registered(DeviceRecord),
    CredentialRotated(DeviceRecord),
    ReadingAccepted {
        envelope: Envelope,
        received_ms: i64,
    },
    FirmwarePublished(FirmwareMetadata),
    ConfigPublished(ConfigMetadata),
}

#[derive(Debug)]
pub enum Error {
    Io(String),
    Invalid(String),
    Unauthorized,
    NotFound,
    Conflict,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(s) | Error::Invalid(s) => write!(f, "{s}"),
            Error::Unauthorized => write!(f, "unauthorized"),
            Error::NotFound => write!(f, "not found"),
            Error::Conflict => write!(f, "conflict"),
        }
    }
}
impl std::error::Error for Error {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>, Error> {
    if !s.len().is_multiple_of(2) {
        return Err(Error::Invalid("secret must be even-length hex".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| Error::Invalid("secret is not hex".into()))
        })
        .collect()
}

/// HMAC-SHA256 without a mutable global or a vendor SDK.
pub fn sign(secret_hex: &str, reading: &Reading) -> Result<String, Error> {
    let mut key = unhex(secret_hex)?;
    if key.len() > 64 {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(64, 0);
    let mut inner = vec![0x36; 64];
    let mut outer = vec![0x5c; 64];
    for i in 0..64 {
        inner[i] ^= key[i];
        outer[i] ^= key[i];
    }
    let body = serde_json::to_vec(reading).map_err(|e| Error::Invalid(e.to_string()))?;
    let mut h = Sha256::new();
    h.update(&inner);
    h.update(&body);
    let inner_sum = h.finalize();
    let mut h = Sha256::new();
    h.update(&outer);
    h.update(inner_sum);
    Ok(hex(&h.finalize()))
}

pub fn signed(secret_hex: &str, reading: Reading) -> Result<Envelope, Error> {
    Ok(Envelope {
        protocol_version: PROTOCOL_VERSION,
        signature: sign(secret_hex, &reading)?,
        reading,
    })
}

pub struct Store {
    path: PathBuf,
    devices: BTreeMap<String, DeviceRecord>,
    readings: BTreeMap<String, BTreeMap<u64, Envelope>>,
    firmware: BTreeMap<String, FirmwareMetadata>,
    config: BTreeMap<u64, ConfigMetadata>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let mut s = Self {
            path: path.clone(),
            devices: BTreeMap::new(),
            readings: BTreeMap::new(),
            firmware: BTreeMap::new(),
            config: BTreeMap::new(),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }
        // Create an empty writable log at startup. A fresh deployment should not
        // page as unavailable just because it has not received its first event.
        let log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::Io(e.to_string()))?;
        for line in BufReader::new(log).lines() {
            let line = line.map_err(|e| Error::Io(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event =
                serde_json::from_str(&line).map_err(|e| Error::Invalid(e.to_string()))?;
            s.apply(event)?;
        }
        Ok(s)
    }
    fn append(&mut self, event: Event) -> Result<(), Error> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| Error::Io(e.to_string()))?;
        let line = serde_json::to_string(&event).map_err(|e| Error::Invalid(e.to_string()))?;
        f.write_all(line.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .and_then(|_| f.sync_data())
            .map_err(|e| Error::Io(e.to_string()))?;
        self.apply(event)
    }
    fn apply(&mut self, event: Event) -> Result<(), Error> {
        match event {
            Event::Registered(d) | Event::CredentialRotated(d) => {
                self.devices.insert(d.device_id.clone(), d);
            }
            Event::ReadingAccepted {
                envelope: e,
                received_ms,
            } => {
                let device_id = e.reading.device_id.clone();
                let sequence = e.reading.sequence;
                let boot_counter = e.reading.boot_counter;
                self.readings
                    .entry(device_id.clone())
                    .or_default()
                    .insert(e.reading.sequence, e);
                if let Some(d) = self.devices.get_mut(&device_id) {
                    d.last_seen_ms = Some(received_ms);
                    d.last_sequence = Some(sequence);
                    d.boot_counter = Some(boot_counter);
                }
            }
            Event::FirmwarePublished(m) => {
                self.firmware.insert(m.version.clone(), m);
            }
            Event::ConfigPublished(m) => {
                self.config.insert(m.version, m);
            }
        }
        Ok(())
    }
    pub fn register(
        &mut self,
        device_id: &str,
        secret_hex: &str,
        firmware_version: &str,
    ) -> Result<DeviceRecord, Error> {
        if self.devices.contains_key(device_id) {
            return Err(Error::Conflict);
        }
        let _ = unhex(secret_hex)?;
        let d = DeviceRecord {
            device_id: device_id.into(),
            credential_version: 1,
            secret_hex: secret_hex.into(),
            firmware_version: firmware_version.into(),
            config_version: 1,
            last_seen_ms: None,
            last_sequence: None,
            boot_counter: None,
        };
        self.append(Event::Registered(d.clone()))?;
        Ok(d)
    }
    pub fn rotate_credential(&mut self, id: &str, secret_hex: &str) -> Result<DeviceRecord, Error> {
        let mut d = self.devices.get(id).cloned().ok_or(Error::NotFound)?;
        let _ = unhex(secret_hex)?;
        d.credential_version += 1;
        d.secret_hex = secret_hex.into();
        self.append(Event::CredentialRotated(d.clone()))?;
        Ok(d)
    }
    pub fn ingest(&mut self, batch: &[Envelope], received_ms: i64) -> Result<usize, Error> {
        if batch.is_empty() || batch.len() > MAX_BATCH {
            return Err(Error::Invalid(format!(
                "batch must contain 1..{MAX_BATCH} readings"
            )));
        }
        let mut unseen = Vec::with_capacity(batch.len());
        for e in batch {
            if e.protocol_version != PROTOCOL_VERSION {
                return Err(Error::Invalid("unsupported protocol version".into()));
            }
            let d = self
                .devices
                .get(&e.reading.device_id)
                .cloned()
                .ok_or(Error::Unauthorized)?;
            if sign(&d.secret_hex, &e.reading)? != e.signature {
                return Err(Error::Unauthorized);
            }
            if !e.reading.value.is_finite() || e.reading.quality.is_empty() {
                return Err(Error::Invalid("invalid reading".into()));
            }
            let seen = self
                .readings
                .get(&e.reading.device_id)
                .is_some_and(|r| r.contains_key(&e.reading.sequence));
            if !seen
                && !unseen.iter().any(|prior: &Envelope| {
                    prior.reading.device_id == e.reading.device_id
                        && prior.reading.sequence == e.reading.sequence
                })
            {
                unseen.push(e.clone());
            }
        }
        let mut accepted = 0;
        for e in unseen {
            let device_id = e.reading.device_id.clone();
            let sequence = e.reading.sequence;
            let boot_counter = e.reading.boot_counter;
            self.append(Event::ReadingAccepted {
                envelope: e,
                received_ms,
            })?;
            let d = self.devices.get_mut(&device_id).expect("device exists");
            d.last_seen_ms = Some(received_ms);
            d.last_sequence = Some(sequence);
            d.boot_counter = Some(boot_counter);
            accepted += 1;
        }
        Ok(accepted)
    }
    pub fn health(&self, id: &str, now: i64, stale_after_ms: i64) -> Result<Health, Error> {
        let d = self.devices.get(id).ok_or(Error::NotFound)?;
        Ok(Health {
            device_id: id.into(),
            online: d
                .last_seen_ms
                .is_some_and(|t| now.saturating_sub(t) <= stale_after_ms),
            stale: d
                .last_seen_ms
                .is_none_or(|t| now.saturating_sub(t) > stale_after_ms),
            last_seen_ms: d.last_seen_ms,
            last_sequence: d.last_sequence,
            boot_counter: d.boot_counter,
        })
    }
    pub fn dashboard(&self, now: i64, stale_after_ms: i64) -> Vec<Health> {
        self.devices
            .keys()
            .filter_map(|id| self.health(id, now, stale_after_ms).ok())
            .collect()
    }
    pub fn publish_firmware(&mut self, m: FirmwareMetadata) -> Result<(), Error> {
        if m.version.is_empty() || m.sha256.len() != 64 {
            return Err(Error::Invalid("invalid firmware metadata".into()));
        }
        self.append(Event::FirmwarePublished(m))
    }
    pub fn publish_config(&mut self, m: ConfigMetadata) -> Result<(), Error> {
        if m.sample_period_ms == 0 || m.stale_after_ms < m.sample_period_ms {
            return Err(Error::Invalid("invalid config metadata".into()));
        }
        self.append(Event::ConfigPublished(m))
    }
    pub fn readings(&self, id: &str) -> Vec<Envelope> {
        self.readings
            .get(id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
    pub fn reading_count(&self) -> usize {
        self.readings.values().map(|r| r.len()).sum()
    }
    pub fn log_bytes(&self) -> Result<u64, Error> {
        fs::metadata(&self.path)
            .map(|m| m.len())
            .map_err(|e| Error::Io(e.to_string()))
    }
    pub fn device(&self, id: &str) -> Option<DeviceRecord> {
        self.devices.get(id).cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFault {
    WifiLoss,
    DnsFailure,
    Timeout,
    ServerRestart,
    CredentialExpiry,
}

/// Deterministic bounded device model. It keeps unsent readings in a ring and
/// removes them only after the cloud acknowledges the batch.
pub struct DeviceSimulator {
    pub device_id: String,
    pub firmware_version: String,
    pub secret_hex: String,
    pub boot_counter: u64,
    next_sequence: u64,
    ring: Vec<Reading>,
    pub dropped_when_full: u64,
}
impl DeviceSimulator {
    pub fn new(device_id: &str, firmware: &str, secret_hex: &str, boot_counter: u64) -> Self {
        Self {
            device_id: device_id.into(),
            firmware_version: firmware.into(),
            secret_hex: secret_hex.into(),
            boot_counter,
            next_sequence: 0,
            ring: Vec::with_capacity(MAX_RING),
            dropped_when_full: 0,
        }
    }
    pub fn sample(&mut self, timestamp_ms: i64, value: f64, quality: &str) {
        let reading = Reading {
            device_id: self.device_id.clone(),
            firmware_version: self.firmware_version.clone(),
            sequence: self.next_sequence,
            device_timestamp_ms: timestamp_ms,
            boot_counter: self.boot_counter,
            value,
            quality: quality.into(),
        };
        self.next_sequence += 1;
        if self.ring.len() == MAX_RING {
            self.ring.remove(0);
            self.dropped_when_full += 1;
        }
        self.ring.push(reading);
    }
    pub fn pending(&self) -> usize {
        self.ring.len()
    }
    pub fn sync<F>(&mut self, mut send: F) -> Result<usize, Error>
    where
        F: FnMut(&[Envelope]) -> Result<usize, Error>,
    {
        let batch: Vec<_> = self
            .ring
            .iter()
            .take(MAX_BATCH)
            .cloned()
            .map(|r| signed(&self.secret_hex, r))
            .collect::<Result<_, _>>()?;
        if batch.is_empty() {
            return Ok(0);
        }
        let n = send(&batch)?;
        if n == batch.len() {
            self.ring.drain(..batch.len());
        }
        Ok(n)
    }
    pub fn reboot(&mut self) {
        self.boot_counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "fieldbus-cloud-{}.log",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn signed_ingest_deduplicates_and_recovers() {
        let p = tmp();
        let mut s = Store::open(&p).unwrap();
        s.register("d1", "00112233445566778899aabbccddeeff", "1.0.0")
            .unwrap();
        let mut dev = DeviceSimulator::new("d1", "1.0.0", "00112233445566778899aabbccddeeff", 4);
        dev.sample(10, 12.0, "ok");
        let b = |x: &[Envelope]| s.ingest(x, 20);
        assert_eq!(dev.sync(b).unwrap(), 1);
        let duplicate = signed(
            &dev.secret_hex,
            Reading {
                device_id: "d1".into(),
                firmware_version: "1.0.0".into(),
                sequence: 0,
                device_timestamp_ms: 10,
                boot_counter: 4,
                value: 12.0,
                quality: "ok".into(),
            },
        )
        .unwrap();
        assert_eq!(s.ingest(&[duplicate], 21).unwrap(), 0);
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.readings("d1").len(), 1);
        let _ = fs::remove_file(p);
    }
    #[test]
    fn offline_ring_is_bounded_and_retries() {
        let mut d = DeviceSimulator::new("d", "1", "00", 1);
        for i in 0..(MAX_RING + 3) {
            d.sample(i as i64, 1.0, "ok");
        }
        assert_eq!(d.pending(), MAX_RING);
        assert_eq!(d.dropped_when_full, 3);
        let mut attempts = 0;
        let n = d.sync(|_| {
            attempts += 1;
            if attempts == 1 {
                Err(Error::Io("timeout".into()))
            } else {
                Ok(MAX_BATCH)
            }
        });
        assert!(n.is_err());
        assert_eq!(d.pending(), MAX_RING);
        assert_eq!(d.sync(|b| Ok(b.len())).unwrap(), MAX_BATCH);
        assert_eq!(d.pending(), MAX_RING - MAX_BATCH);
    }
    #[test]
    fn stale_health_is_explicit() {
        let p = tmp();
        let mut s = Store::open(&p).unwrap();
        s.register("d", "00", "1").unwrap();
        assert!(s.health("d", 100, 50).unwrap().stale);
        let _ = fs::remove_file(p);
    }
    #[test]
    fn invalid_batch_is_atomic() {
        let p = tmp();
        let mut s = Store::open(&p).unwrap();
        let secret = "00";
        s.register("d", secret, "1").unwrap();
        let good = signed(
            secret,
            Reading {
                device_id: "d".into(),
                firmware_version: "1".into(),
                sequence: 0,
                device_timestamp_ms: 1,
                boot_counter: 1,
                value: 1.0,
                quality: "ok".into(),
            },
        )
        .unwrap();
        let mut bad = good.clone();
        bad.signature = "ff".into();
        assert!(s.ingest(&[good, bad], 2).is_err());
        assert!(s.readings("d").is_empty());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn empty_store_has_a_readable_log_and_missing_storage_is_explicit() {
        let p = tmp();
        let store = Store::open(&p).unwrap();
        assert_eq!(store.log_bytes().unwrap(), 0);
        fs::remove_file(&p).unwrap();
        assert!(store.log_bytes().is_err());
    }
}
