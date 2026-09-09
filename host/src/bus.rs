//! The bus abstraction and the deterministic in-process simulator.
//!
//! [`CanBus`] is the whole surface the runner sees, so a run against
//! [`SimBus`] and a run against a real `AF_CAN` socket go through identical
//! code above this line, including the same validator. That is what makes the
//! simulator worth anything: it is not a mock of the control node, it is a
//! substitute for the wire.
//!
//! It is still a simulation. There is no transceiver, no arbitration, no bit
//! stuffing, no bus-off state and no electrical fault model. What it does
//! reproduce is the frame-level faults a validator has to survive, at exact
//! and repeatable cycle indices.

use std::collections::BTreeSet;

use fieldbus_core::j1939::{crc8_j1850, sensor_frame, Frame, Payload, STATUS_OK, VALUE_MAX};

/// Why a send failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// The underlying transport refused or failed the write.
    Io(String),
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusError::Io(m) => write!(f, "bus io error: {m}"),
        }
    }
}

impl std::error::Error for BusError {}

/// A CAN transport the runner can drive.
pub trait CanBus {
    /// Transmits one frame.
    fn send(&mut self, frame: Frame) -> Result<(), BusError>;

    /// Collects frames that arrive before `deadline_us`, measured on the same
    /// monotonic microsecond scale the runner uses for scheduling.
    ///
    /// Implementations must return promptly at the deadline rather than
    /// blocking past it: overrunning here is indistinguishable, from the
    /// runner's side, from the control step being slow, and would show up as
    /// a deadline miss attributed to the wrong cause.
    fn recv_until(&mut self, deadline_us: u64) -> Vec<Frame>;
}

/// Which fault to inject at a given cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Deliver an extra frame whose CRC byte is wrong.
    BadCrc,
    /// Deliver this cycle's frame a second time, byte for byte.
    Duplicate,
    /// Deliver an extra frame whose sequence number skips ahead.
    OutOfOrder,
    /// Deliver an extra frame carrying a value outside the accepted range.
    OutOfRange,
    /// Deliver an extra frame with a short payload.
    BadLength,
    /// Deliver nothing this cycle: the sensor did not transmit.
    ///
    /// The sequence counter does not advance either, because a frame that
    /// was never sent was never numbered. So the next frame is still in
    /// sequence and nothing is rejected; the node simply goes a period
    /// without a fresh reading, and enough of those trip the watchdog.
    ///
    /// This is not a model of a wire-level loss, where the sender did
    /// transmit and its counter did advance, leaving a real gap. That case
    /// is documented as a limitation rather than simulated: see the note on
    /// sequence resynchronisation in `fieldbus_core::j1939::Validator`.
    Drop,
}

/// Which cycles get which faults, and when the sensor stops for good.
///
/// Empty by default. Every field is populated only by an explicit negative
/// control, and the runner has no path that turns one on by itself.
#[derive(Debug, Clone, Default)]
pub struct FaultSchedule {
    /// Cycles receiving an extra bad-CRC frame.
    pub bad_crc: BTreeSet<u64>,
    /// Cycles receiving a duplicate of their own frame.
    pub duplicate: BTreeSet<u64>,
    /// Cycles receiving an extra frame with a skipped sequence number.
    pub out_of_order: BTreeSet<u64>,
    /// Cycles receiving an extra out-of-range frame.
    pub out_of_range: BTreeSet<u64>,
    /// Cycles receiving an extra short frame.
    pub bad_length: BTreeSet<u64>,
    /// Cycles whose legitimate frame is dropped entirely.
    pub dropped: BTreeSet<u64>,
    /// Cycle from which the sensor stops emitting and never resumes.
    pub freeze_at: Option<u64>,
}

impl FaultSchedule {
    /// A clean bus: no faults at all. This is what a positive run uses.
    pub fn clean() -> Self {
        Self::default()
    }

    /// Whether this schedule injects nothing, which is what a positive run
    /// asserts about itself.
    pub fn is_clean(&self) -> bool {
        self.bad_crc.is_empty()
            && self.duplicate.is_empty()
            && self.out_of_order.is_empty()
            && self.out_of_range.is_empty()
            && self.bad_length.is_empty()
            && self.dropped.is_empty()
            && self.freeze_at.is_none()
    }

    /// Total number of frames this schedule will cause to be rejected, for a
    /// run long enough to reach every listed cycle.
    pub fn injected_count(&self) -> usize {
        self.bad_crc.len()
            + self.duplicate.len()
            + self.out_of_order.len()
            + self.out_of_range.len()
            + self.bad_length.len()
    }
}

/// A deterministic in-process CAN bus with a simulated sensor and plant.
///
/// # What is simulated
///
/// The plant is a first-order lag driven by the node's own actuator command:
/// `x += PLANT_GAIN * (u - x)`. The sensor samples it once per period, adds a
/// small deterministic measurement error derived from the seed, truncates to
/// `i16`, and emits one frame with an incrementing sequence number. So the
/// loop really is closed: the reading the controller acts on is a consequence
/// of the command it issued.
///
/// # What makes it deterministic
///
/// The noise generator is a seeded linear congruential generator advanced
/// exactly once per emitted frame, and the fault schedule is keyed on cycle
/// index rather than on the noise. Same seed and same schedule gives the same
/// frames, byte for byte, on every run.
pub struct SimBus {
    cycle: u64,
    plant: f32,
    seq: u8,
    rng: u64,
    faults: FaultSchedule,
    sent: u64,
}

/// Per-period gain of the simulated first-order plant.
pub const PLANT_GAIN: f32 = 0.05;

impl SimBus {
    /// A bus seeded for reproducible measurement error, with the given faults.
    pub fn new(seed: u64, faults: FaultSchedule) -> Self {
        SimBus {
            cycle: 0,
            plant: 0.0,
            seq: 0,
            // Offset so a seed of 0 is not a degenerate generator state.
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            faults,
            sent: 0,
        }
    }

    /// The cycle index the next [`CanBus::recv_until`] will produce.
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Current plant state, for a caller that wants to log it.
    pub fn plant(&self) -> f32 {
        self.plant
    }

    /// Number of command frames accepted from the controller.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Deterministic measurement error in the range -2..=2 engineering units.
    ///
    /// Advanced exactly once per emitted frame. A frame that is never emitted
    /// (a frozen or dropped cycle) does not advance it, which is what keeps a
    /// control run's frame contents identical to a positive run's up to the
    /// point where they diverge.
    fn noise(&mut self) -> f32 {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.rng >> 33) % 5) as f32 - 2.0
    }
}

impl CanBus for SimBus {
    fn send(&mut self, frame: Frame) -> Result<(), BusError> {
        // The command drives the plant. Decoding it here rather than taking
        // the actuator value directly is deliberate: the simulator only ever
        // sees what actually went on the wire, so a framing bug in the
        // command path shows up as a plant that does not move.
        if frame.dlc as usize == 8 && frame.data[7] == crc8_j1850(&frame.data[0..7]) {
            let u = Payload::decode(&frame.data).value as f32;
            self.plant += PLANT_GAIN * (u - self.plant);
            self.sent += 1;
        }
        Ok(())
    }

    fn recv_until(&mut self, _deadline_us: u64) -> Vec<Frame> {
        // There is no wire, so there is nothing to wait for and the deadline
        // is unused: every frame for this cycle is available immediately.
        // Honouring the deadline by sleeping to it would only add latency the
        // real bus does not have.
        let cycle = self.cycle;
        self.cycle += 1;

        let frozen = self.faults.freeze_at.is_some_and(|f| cycle >= f);
        if frozen || self.faults.dropped.contains(&cycle) {
            return Vec::new();
        }

        let value = (self.plant + self.noise()) as i16;
        let good = sensor_frame(self.seq, value, STATUS_OK);
        let seq_now = self.seq;
        self.seq = self.seq.wrapping_add(1);

        let mut out = Vec::with_capacity(2);
        out.push(good);

        // Injected faults arrive as an *additional* frame after the valid one
        // for this cycle. That ordering is what makes the accounting exact:
        // the legitimate frame is accepted first and advances the sequence
        // position, so the injected frame is attributable to one reason and
        // cannot cascade into a rejection on the next cycle's good frame.
        if self.faults.bad_crc.contains(&cycle) {
            let mut f = good;
            f.data[7] ^= 0xFF;
            out.push(f);
        }
        if self.faults.duplicate.contains(&cycle) {
            out.push(good);
        }
        if self.faults.out_of_order.contains(&cycle) {
            // Skip five ahead: far enough that it cannot be mistaken for the
            // next in-sequence frame under any wrap.
            out.push(sensor_frame(seq_now.wrapping_add(5), value, STATUS_OK));
        }
        if self.faults.out_of_range.contains(&cycle) {
            out.push(sensor_frame(
                seq_now.wrapping_add(1),
                VALUE_MAX + 1,
                STATUS_OK,
            ));
        }
        if self.faults.bad_length.contains(&cycle) {
            let mut f = sensor_frame(seq_now.wrapping_add(1), value, STATUS_OK);
            f.dlc = 4;
            out.push(f);
        }
        out
    }
}
