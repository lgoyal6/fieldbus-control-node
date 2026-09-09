//! The control law: a bounded first-order low-pass filter feeding a PI
//! controller with output saturation and conditional-integration anti-windup.
//!
//! Everything here is `f32` with explicit clamps and uses only `+`, `-`, `*`
//! and comparison. There is no `sqrt`, `exp` or trigonometry, so the crate
//! needs no `libm` on a bare-metal target, and there is no iteration count
//! that depends on the data, so a step costs the same every period.
//!
//! Fixed period: [`PERIOD_US`] microseconds. The controller does not read a
//! clock. The runner is responsible for calling it once per period, and the
//! deadline telemetry in [`crate::telemetry`] is what proves it did.

/// The control period. Frozen in `manifest/frozen.json`.
pub const PERIOD_US: u64 = 10_000;
/// The control period in seconds, used as the PI integration step.
pub const PERIOD_S: f32 = 0.010;

/// A first-order low-pass filter, `y += alpha * (x - y)`.
///
/// `alpha` is clamped into `(0, 1]` at construction. At `alpha == 1` the
/// filter is a pass-through; it can never be zero, because a filter that
/// ignores its input is a silent failure rather than a configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LowPass {
    alpha: f32,
    y: f32,
    primed: bool,
}

impl LowPass {
    /// A filter with the given smoothing factor, not yet primed.
    pub fn new(alpha: f32) -> Self {
        LowPass {
            alpha: alpha.clamp(f32::MIN_POSITIVE, 1.0),
            y: 0.0,
            primed: false,
        }
    }

    /// Feeds one sample and returns the filtered output.
    ///
    /// The first sample is adopted outright rather than blended toward from
    /// zero. Blending from zero would make the controller chase a startup
    /// transient that never existed on the bus.
    pub fn update(&mut self, x: f32) -> f32 {
        if !self.primed {
            self.y = x;
            self.primed = true;
        } else {
            self.y += self.alpha * (x - self.y);
        }
        self.y
    }

    /// The current filtered value.
    pub const fn value(&self) -> f32 {
        self.y
    }

    /// Drops the filter state, so the next sample is adopted outright.
    pub fn reset(&mut self) {
        self.y = 0.0;
        self.primed = false;
    }
}

/// A PI controller with output saturation and anti-windup.
///
/// Anti-windup is conditional integration: the integral term only accumulates
/// when the resulting unsaturated output would stay inside the output limits.
/// The alternative, back-calculation, needs a division and gives the same
/// behaviour for a controller that is either inside its limits or hard
/// against them, which is the only regime a bounded plant reaches here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pi {
    kp: f32,
    ki: f32,
    out_min: f32,
    out_max: f32,
    integral: f32,
}

impl Pi {
    /// A PI controller with proportional gain `kp`, integral gain `ki` (per
    /// second) and output limits `out_min..=out_max`.
    ///
    /// The limits are ordered at construction rather than trusted, because a
    /// swapped pair would make every `clamp` panic at runtime.
    pub fn new(kp: f32, ki: f32, out_min: f32, out_max: f32) -> Self {
        let (lo, hi) = if out_min <= out_max {
            (out_min, out_max)
        } else {
            (out_max, out_min)
        };
        Pi {
            kp,
            ki,
            out_min: lo,
            out_max: hi,
            integral: 0.0,
        }
    }

    /// Advances one [`PERIOD_S`] step and returns the saturated output.
    pub fn update(&mut self, setpoint: f32, measurement: f32) -> f32 {
        let error = setpoint - measurement;
        let candidate_integral = self.integral + error * PERIOD_S;
        let unsaturated = self.kp * error + self.ki * candidate_integral;
        // Integrate only if doing so would not push the output past a limit.
        // Once saturated the integral is frozen, so the controller comes off
        // the limit the moment the error changes sign instead of unwinding a
        // term it accumulated while it had no authority.
        if unsaturated >= self.out_min && unsaturated <= self.out_max {
            self.integral = candidate_integral;
        }
        let out = self.kp * error + self.ki * self.integral;
        out.clamp(self.out_min, self.out_max)
    }

    /// The accumulated integral term, in error-seconds.
    pub const fn integral(&self) -> f32 {
        self.integral
    }

    /// Clears the integral. Called when a latched safe state is cleared, so
    /// the controller does not resume with a term earned before the fault.
    pub fn reset(&mut self) {
        self.integral = 0.0;
    }
}
