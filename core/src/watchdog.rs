//! Sensor-staleness watchdog.
//!
//! The watchdog is fed only by *accepted* sensor frames. That is the whole
//! design: a bus that goes silent and a bus that keeps delivering frames the
//! validator refuses are the same fault as far as the controller is concerned,
//! because in both cases it has no trustworthy reading. Feeding the watchdog
//! from frame arrival rather than frame acceptance would let a stuck sensor
//! babbling bad-CRC frames hold the node in normal operation forever.
//!
//! Once tripped the safe state latches and stays latched until [`Watchdog::reset`]
//! is called explicitly. Nothing on the bus can clear it, including a sensor
//! that starts working again: recovering automatically from a fault whose cause
//! is unknown is how a node ends up oscillating between safe and unsafe.

/// Why the node entered its safe state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafeReason {
    /// No sensor frame was accepted within `timeout_us` of the last one.
    SensorStale {
        /// Monotonic timestamp of the last accepted sensor frame.
        last_seen_us: u64,
        /// Monotonic timestamp of the check that observed the timeout.
        tripped_at_us: u64,
        /// The staleness budget that was exceeded.
        timeout_us: u64,
    },
}

/// A latched safe state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafeState {
    /// The fault that caused the latch.
    pub reason: SafeReason,
}

impl SafeState {
    /// How late the trip was, in microseconds past the deadline it enforces.
    ///
    /// The deadline is `last_seen_us + timeout_us`. This is the quantity that
    /// says whether the watchdog is actually enforcing its budget or merely
    /// reporting it: on a node checked once per 10 ms period, it should never
    /// exceed one period.
    pub const fn reaction_time_us(&self) -> u64 {
        match self.reason {
            SafeReason::SensorStale {
                last_seen_us,
                tripped_at_us,
                timeout_us,
            } => tripped_at_us.saturating_sub(last_seen_us + timeout_us),
        }
    }

    /// A short stable name for the reason, used as a JSON value in `results/`.
    pub const fn reason_name(&self) -> &'static str {
        match self.reason {
            SafeReason::SensorStale { .. } => "sensor_stale",
        }
    }
}

/// Trips a latched [`SafeState`] when accepted sensor frames stop arriving.
#[derive(Clone, Copy, Debug)]
pub struct Watchdog {
    timeout_us: u64,
    last_seen_us: u64,
    latched: Option<SafeState>,
}

impl Watchdog {
    /// A watchdog armed at `start_us` with the given staleness budget.
    ///
    /// Arming at construction rather than at the first accepted frame is
    /// deliberate: a node whose sensor never comes up at all must trip, and a
    /// watchdog that waits for a first frame before arming would sit in normal
    /// operation indefinitely on exactly that fault.
    pub const fn new(timeout_us: u64, start_us: u64) -> Self {
        Watchdog {
            timeout_us,
            last_seen_us: start_us,
            latched: None,
        }
    }

    /// Records an accepted sensor frame at `now_us`.
    ///
    /// Timestamps are taken as monotonic and non-decreasing; a feed with an
    /// earlier timestamp than the one on record does not move the deadline
    /// backwards.
    pub fn feed(&mut self, now_us: u64) {
        if now_us > self.last_seen_us {
            self.last_seen_us = now_us;
        }
    }

    /// Evaluates the staleness budget and returns the latched state, if any.
    ///
    /// Trips when `now_us` is strictly past `last_seen_us + timeout_us`. Once
    /// latched, the same state is returned on every later call with the
    /// original timestamps, so the reaction time reported at the end of a run
    /// is the reaction time of the trip and not of the last check.
    pub fn check(&mut self, now_us: u64) -> Option<SafeState> {
        if self.latched.is_none() && now_us > self.last_seen_us + self.timeout_us {
            self.latched = Some(SafeState {
                reason: SafeReason::SensorStale {
                    last_seen_us: self.last_seen_us,
                    tripped_at_us: now_us,
                    timeout_us: self.timeout_us,
                },
            });
        }
        self.latched
    }

    /// The latched state without evaluating the budget.
    pub const fn latched(&self) -> Option<SafeState> {
        self.latched
    }

    /// Clears the latch and re-arms at `now_us`.
    ///
    /// This is the only way out of a safe state. It exists as an explicit call
    /// with no bus-visible trigger so that clearing a fault is always an
    /// operator action.
    pub fn reset(&mut self, now_us: u64) {
        self.latched = None;
        self.last_seen_us = now_us;
    }

    /// The staleness budget in microseconds.
    pub const fn timeout_us(&self) -> u64 {
        self.timeout_us
    }

    /// Timestamp of the last accepted sensor frame.
    pub const fn last_seen_us(&self) -> u64 {
        self.last_seen_us
    }
}
