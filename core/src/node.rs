//! The control node itself: one pure step function over frames and time.
//!
//! [`ControlNode::step`] is a function of `(state, now_us, frames)` and
//! nothing else. It reads no clock, allocates nothing, and performs no I/O.
//! Everything time-dependent or bus-dependent is the host's job, which is
//! what makes a run reproducible: replay the same frames with the same
//! timestamps and the outputs are bit-identical.

use crate::control::{LowPass, Pi};
use crate::j1939::{command_frame, Frame, Validator, REJECT_KINDS, STATUS_OK, STATUS_SAFE};
use crate::watchdog::{SafeState, Watchdog};

/// Control setpoint in sensor engineering units. Frozen in
/// `manifest/frozen.json`.
pub const SETPOINT: f32 = 500.0;
/// Low-pass smoothing factor on the sensor stream.
pub const FILTER_ALPHA: f32 = 0.30;
/// Proportional gain.
pub const KP: f32 = 0.60;
/// Integral gain, per second.
pub const KI: f32 = 4.00;
/// Actuator command lower limit.
pub const OUT_MIN: f32 = -1_000.0;
/// Actuator command upper limit.
pub const OUT_MAX: f32 = 1_000.0;
/// Sensor staleness budget in microseconds. Frozen in
/// `manifest/frozen.json`.
pub const WATCHDOG_TIMEOUT_US: u64 = 100_000;
/// The actuator value commanded while a safe state is latched.
pub const SAFE_ACTUATOR: f32 = 0.0;

/// What one control period produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepOutput {
    /// The command frame to transmit, if any. The node emits one every period,
    /// including periods in which no sensor frame arrived, because the
    /// actuator needs a command on schedule and silence is not a command.
    pub command_frame: Option<Frame>,
    /// The commanded actuator value, before quantisation to `i16`.
    pub actuator: f32,
    /// The latched safe state, if the node is in one.
    pub safe_state: Option<SafeState>,
    /// Cumulative accepted sensor frames since construction.
    pub accepted: u32,
    /// Cumulative rejected sensor frames since construction.
    pub rejected: u32,
}

/// A periodic control node: validate, filter, control, watch, command.
#[derive(Clone, Debug)]
pub struct ControlNode {
    validator: Validator,
    filter: LowPass,
    pi: Pi,
    watchdog: Watchdog,
    tx_seq: u8,
    setpoint: f32,
}

impl ControlNode {
    /// A node armed at `start_us` with the frozen gains and timeout.
    pub fn new(start_us: u64) -> Self {
        ControlNode {
            validator: Validator::new(),
            filter: LowPass::new(FILTER_ALPHA),
            pi: Pi::new(KP, KI, OUT_MIN, OUT_MAX),
            watchdog: Watchdog::new(WATCHDOG_TIMEOUT_US, start_us),
            tx_seq: 0,
            setpoint: SETPOINT,
        }
    }

    /// Runs one control period.
    ///
    /// Order of operations, and why:
    ///
    /// 1. Validate every received frame in arrival order. Only accepted frames
    ///    feed the filter and the watchdog.
    /// 2. Check the watchdog. This happens after ingest so a frame that
    ///    arrived this period counts against staleness, and before control so
    ///    a stale period never produces a control output.
    /// 3. If a safe state is latched, command [`SAFE_ACTUATOR`] with status
    ///    [`STATUS_SAFE`] and do not run the control law. The frame is still
    ///    sent: an actuator told nothing holds its last command, which on a
    ///    stale-sensor fault is the last command computed from good data and
    ///    is exactly the wrong thing to hold.
    /// 4. Otherwise filter, run PI, and command the result with status
    ///    [`STATUS_OK`].
    pub fn step(&mut self, now_us: u64, received: &[Frame]) -> StepOutput {
        let mut fresh: Option<f32> = None;
        for frame in received {
            if let Ok(payload) = self.validator.validate(frame) {
                self.watchdog.feed(now_us);
                fresh = Some(payload.value as f32);
            }
        }

        let safe_state = self.watchdog.check(now_us);

        let actuator = if safe_state.is_some() {
            SAFE_ACTUATOR
        } else {
            if let Some(v) = fresh {
                self.filter.update(v);
            }
            self.pi.update(self.setpoint, self.filter.value())
        };

        let status = if safe_state.is_some() {
            STATUS_SAFE
        } else {
            STATUS_OK
        };
        // Truncation toward zero rather than rounding: `f32::round` is not
        // available in `core`, and pulling in `libm` for one call on the
        // command path is a poor trade for a half-unit of actuator resolution.
        let quantised = actuator as i16;
        let frame = command_frame(self.tx_seq, quantised, status);
        self.tx_seq = self.tx_seq.wrapping_add(1);

        StepOutput {
            command_frame: Some(frame),
            actuator,
            safe_state,
            accepted: self.validator.accepted(),
            rejected: self.validator.rejected(),
        }
    }

    /// Clears a latched safe state and the control state behind it.
    ///
    /// The integral term and the filter are dropped too. Resuming with an
    /// integral accumulated before a fault would command a correction for an
    /// error the node can no longer vouch for.
    pub fn reset(&mut self, now_us: u64) {
        self.watchdog.reset(now_us);
        self.pi.reset();
        self.filter.reset();
        self.validator.reset_sequence();
    }

    /// Per-reason rejection counts, indexed by [`crate::j1939::Reject::index`].
    pub const fn rejected_by_reason(&self) -> &[u32; REJECT_KINDS] {
        self.validator.by_reason()
    }

    /// The latched safe state, if any.
    pub const fn safe_state(&self) -> Option<SafeState> {
        self.watchdog.latched()
    }
}
