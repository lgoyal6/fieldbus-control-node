//! Reads `manifest/frozen.json`.
//!
//! The manifest is not documentation of the experiment; it *is* the
//! experiment. The gate thresholds compared against a run, the cycles at
//! which the `can-corrupt` control injects each fault, the cycle at which
//! `sensor-freeze` stops the sensor, and the expected per-reason histogram
//! are all read from it at run time rather than duplicated in Rust. That is
//! the point: a threshold cannot be quietly relaxed after seeing a result
//! without changing the committed file whose hash is published alongside the
//! result.

use std::fs;
use std::path::Path;

use serde::Deserialize;

/// Default location of the frozen manifest, relative to the repository root.
pub const DEFAULT_PATH: &str = "manifest/frozen.json";

/// The subset of `manifest/frozen.json` the host actually acts on.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Loop parameters: period, cycle count, seed.
    #[serde(rename = "loop")]
    pub loop_: LoopSpec,
    /// The sensor-staleness budget the node is built with.
    pub watchdog: WatchdogSpec,
    /// The four positive-gate thresholds.
    pub positive_gate: PositiveGate,
    /// The three negative controls, keyed by `id`.
    pub negative_controls: Vec<NegativeControl>,
}

/// Frozen loop parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct LoopSpec {
    /// Control period in microseconds.
    pub target_period_us: u64,
    /// Number of periods in a full run.
    pub cycles: u64,
    /// Seed for the simulated plant's measurement noise.
    pub seed: u64,
}

/// The frozen watchdog parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct WatchdogSpec {
    /// Sensor staleness budget in microseconds. The host builds the node's
    /// watchdog with this, so the budget a run enforces is the frozen one and
    /// not a constant that merely agrees with it.
    pub timeout_us: u64,
}

/// The conditions a positive run must satisfy.
#[derive(Debug, Clone, Deserialize)]
pub struct PositiveGate {
    /// Maximum tolerated missed deadlines. Frozen at 0.
    pub missed_deadlines_max: u64,
    /// Maximum tolerated p99 wake jitter in microseconds.
    pub p99_jitter_us_max: u64,
    /// Maximum tolerated rejected frames. Frozen at 0.
    pub rejected_frames_max: u32,
    /// Whether a positive run may end with the watchdog tripped. Frozen false.
    pub watchdog_may_trip: bool,
}

/// One negative control's frozen mechanism and expectation.
#[derive(Debug, Clone, Deserialize)]
pub struct NegativeControl {
    /// Stable identifier, matching the CLI subcommand.
    pub id: String,
    /// Cycles at which each fault kind is injected. `can-corrupt` only.
    #[serde(default)]
    pub injection_cycles: Option<InjectionCycles>,
    /// Expected total rejections. `can-corrupt` only.
    #[serde(default)]
    pub expected_rejected_total: Option<u32>,
    /// Expected per-reason rejection histogram. `can-corrupt` only.
    #[serde(default)]
    pub expected_rejected_by_reason: Option<std::collections::BTreeMap<String, u32>>,
    /// Expected accepted count. `can-corrupt` only.
    #[serde(default)]
    pub expected_accepted: Option<u32>,
    /// Scheduling policy the control thread requests during this control's
    /// run. `cpu-hog` only; every other run uses the elevated policy.
    #[serde(default)]
    pub control_thread_policy: Option<String>,
    /// Scheduling policy each spinner requests. `cpu-hog` only.
    #[serde(default)]
    pub hog_thread_policy: Option<String>,
    /// Cycle at which the sensor stops. `sensor-freeze` only.
    #[serde(default)]
    pub freeze_at_cycle: Option<u64>,
    /// Longest tolerated path from the last accepted sensor frame to the
    /// latched safe state, in microseconds. `sensor-freeze` only.
    #[serde(default)]
    pub max_reaction_time_us: Option<u64>,
}

/// The fixed cycle indices at which each fault kind is injected.
#[derive(Debug, Clone, Deserialize)]
pub struct InjectionCycles {
    /// Cycles receiving an extra frame with a corrupted CRC byte.
    pub bad_crc: Vec<u64>,
    /// Cycles receiving a byte-exact repeat of that cycle's frame.
    pub duplicate: Vec<u64>,
    /// Cycles receiving an extra frame with a skipped sequence number.
    pub out_of_order: Vec<u64>,
    /// Cycles receiving an extra frame carrying an implausible value.
    pub out_of_range: Vec<u64>,
    /// Cycles receiving an extra frame with a short payload.
    pub bad_length: Vec<u64>,
}

impl Manifest {
    /// Loads and parses the manifest at `path`.
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("cannot read manifest {}: {e}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| format!("cannot parse manifest {}: {e}", path.display()))
    }

    /// The negative control with the given `id`.
    pub fn control(&self, id: &str) -> Result<&NegativeControl, String> {
        self.negative_controls
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("manifest has no negative control with id {id:?}"))
    }
}
