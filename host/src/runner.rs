//! The periodic scheduler, the jitter measurement, and the deadline accounting.
//!
//! # What "periodic" means here, precisely
//!
//! This is a POSIX-hosted periodic loop with deadline telemetry. It is not a
//! hard real-time system: there is no admission control, no bounded worst-case
//! execution time, no RTOS and no guarantee from the operating system that a
//! deadline will be met. What there is instead is a measurement of how often
//! it was not met, which is the honest version of the same claim.
//!
//! # No drift accumulation
//!
//! Cycle `i` is scheduled at `t0 + i * period`, where `t0` is read once from
//! the monotonic clock. Sleeping for `period` at the end of each cycle would
//! accumulate every wake-up error and every microsecond of work into a
//! permanent phase drift; computing an absolute target from a fixed origin
//! cannot.
//!
//! # Jitter and deadlines
//!
//! - **Wake jitter** is `actual_start - scheduled_start`. An early wake is
//!   recorded as zero rather than as a negative number, because the loop
//!   never intentionally starts early and treating an early start as
//!   "negative lateness" would let it cancel out real lateness in the mean.
//! - A cycle **misses its deadline** when its work finishes later than
//!   `scheduled_start + period`.
//! - A period that elapses with no cycle executing in it at all is counted as
//!   a miss too. Without this, a loop that fell four periods behind and then
//!   ran one fast cycle could report a single miss for five periods of
//!   silence, and the actuator would have gone four periods without a command.
//!
//! # Sleeping strategy, and why it is the plain one
//!
//! The loop sleeps until the scheduled start and does not busy-wait. Both
//! were measured on this machine at a 10 ms period: plain sleep gave a p99
//! wake jitter of 58 us, and a 400 us guard band followed by a spin gave a
//! p99 of 0 us with a max of 15 us. The spin is better and is still the wrong
//! choice here. Plain sleep already clears the frozen 1000 us threshold by a
//! factor of about seventeen, while the spin would hold a core busy for four
//! percent of every period across a five-run gate on a shared machine, and it
//! would also make the `cpu-hog` control harder to catch by hardening the
//! control thread against exactly the contention that control exists to
//! create.

use std::time::{Duration, Instant};

use fieldbus_core::j1939::REJECT_KINDS;
use fieldbus_core::telemetry::JitterStats;
use fieldbus_core::{ControlNode, SafeState};

use crate::bus::CanBus;
use crate::hog::CpuHog;

/// How the clock is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Sleep against the real monotonic clock and measure what happens. This
    /// is the only mode whose timing numbers mean anything.
    RealTime,
    /// Advance a simulated clock with no sleeping. Deterministic and fast, so
    /// it is what CI runs, and its jitter is identically zero by construction.
    /// It is proof that the pipeline works, never timing evidence.
    Virtual,
}

impl Mode {
    /// The stable name written into `results/`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::RealTime => "real-time",
            Mode::Virtual => "virtual-time",
        }
    }
}

/// Why a period counted against the deadline budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissKind {
    /// The cycle ran, but its work finished after its own deadline.
    WorkOverran,
    /// A whole period elapsed with no cycle executing in it.
    SkippedPeriod,
}

impl MissKind {
    /// The stable name written into `results/`.
    pub fn as_str(&self) -> &'static str {
        match self {
            MissKind::WorkOverran => "work_overran",
            MissKind::SkippedPeriod => "skipped_period",
        }
    }
}

/// One explained deadline miss.
#[derive(Debug, Clone, Copy)]
pub struct Miss {
    /// Index of the cycle the miss is attributed to.
    pub cycle: u64,
    /// Whether the cycle overran or the period was skipped entirely.
    pub kind: MissKind,
    /// Wake jitter of that cycle, in microseconds.
    pub jitter_us: u64,
    /// How long the cycle's work took, in microseconds.
    pub work_us: u64,
    /// How far past the deadline the work finished, in microseconds.
    pub overrun_us: u64,
}

/// Whether the elevated scheduling policy was asked for and whether it stuck.
#[derive(Debug, Clone)]
pub struct SchedulingReport {
    /// What was requested, named concretely.
    pub requested: String,
    /// Whether the operating system granted it.
    pub granted: bool,
    /// The return code or reason, verbatim.
    pub detail: String,
}

impl SchedulingReport {
    /// The single-line form written into `results/` as `scheduling_policy`.
    pub fn summary(&self) -> String {
        format!(
            "requested={}, granted={}, detail={}",
            self.requested, self.granted, self.detail
        )
    }
}

/// Requests an elevated scheduling policy for the calling thread.
///
/// Best effort, always. A refusal is recorded and the run continues, because
/// a run that reports honest jitter under the default policy is worth more
/// than a run that refuses to start. Nothing in the gate depends on the
/// policy being granted; the grant result is reported alongside the numbers
/// so a reader knows which regime produced them.
// libc deprecates its Mach bindings in favour of the mach2 crate. Two calls,
// made once per thread, are not worth another dependency, so the deprecation
// is allowed here and nowhere else.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn request_realtime_policy(period_us: u64) -> SchedulingReport {
    let (numer, denom) = {
        let mut tb = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
        let rc = unsafe { libc::mach_timebase_info(&mut tb) };
        if rc != libc::KERN_SUCCESS || tb.numer == 0 {
            return SchedulingReport {
                requested: "mach THREAD_TIME_CONSTRAINT_POLICY".to_string(),
                granted: false,
                detail: format!("mach_timebase_info failed, rc={rc}"),
            };
        }
        (tb.numer as u64, tb.denom as u64)
    };
    // Policy fields are in mach absolute time units, not nanoseconds.
    let to_ticks = |ns: u64| -> u32 { ((ns * denom) / numer) as u32 };
    let period_ns = period_us * 1_000;

    let mut policy = libc::thread_time_constraint_policy_data_t {
        period: to_ticks(period_ns),
        // A tenth of the period. The real step is far shorter than this; the
        // budget is a declaration to the scheduler, and declaring more than
        // is needed makes the thread likelier to be demoted for overrunning.
        computation: to_ticks(period_ns / 10),
        // Half the period: the window inside which the computation must land.
        constraint: to_ticks(period_ns / 2),
        // Non-preemptible, which is the whole reason to ask.
        preemptible: 0,
    };
    // mach_thread_self yields a send right that is conventionally deallocated.
    // It is called once per thread, at most 2 * cores + 1 times in the whole
    // process, and the process is short-lived, so the leak is bounded and
    // stated rather than handled.
    let rc = unsafe {
        libc::thread_policy_set(
            libc::mach_thread_self(),
            libc::THREAD_TIME_CONSTRAINT_POLICY as u32,
            &mut policy as *mut _ as libc::thread_policy_t,
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    SchedulingReport {
        requested: format!(
            "mach THREAD_TIME_CONSTRAINT_POLICY period={}us computation={}us constraint={}us preemptible=0",
            period_us,
            period_us / 10,
            period_us / 2
        ),
        granted: rc == libc::KERN_SUCCESS,
        detail: format!("thread_policy_set rc={rc}"),
    }
}

/// Requests `SCHED_FIFO` for the calling thread. Best effort; see the macOS
/// variant for why a refusal is not fatal.
#[cfg(target_os = "linux")]
pub fn request_realtime_policy(_period_us: u64) -> SchedulingReport {
    let priority = 80;
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // Per-thread rather than per-process: the hog threads and the control
    // thread are asked separately and their results are reported separately.
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
    SchedulingReport {
        requested: format!("SCHED_FIFO priority={priority}"),
        // Without CAP_SYS_NICE this returns EPERM, which is the normal case
        // on a CI runner and is reported as such rather than worked around.
        granted: rc == 0,
        detail: format!("pthread_setschedparam rc={rc}"),
    }
}

/// No elevated policy is attempted on other platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn request_realtime_policy(_period_us: u64) -> SchedulingReport {
    SchedulingReport {
        requested: "none".to_string(),
        granted: false,
        detail: "no elevated scheduling policy is implemented for this platform".to_string(),
    }
}

/// Frozen loop parameters for one run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Number of periods to execute.
    pub cycles: u64,
    /// Period length in microseconds.
    pub period_us: u64,
    /// Real or virtual clock.
    pub mode: Mode,
}

/// A window of deliberate CPU contention, in cycle indices.
#[derive(Debug, Clone, Copy)]
pub struct HogWindow {
    /// First cycle at which the spinners run.
    pub start_cycle: u64,
    /// First cycle at which they are stopped again.
    pub end_cycle: u64,
    /// How many spinners to spawn.
    pub threads: usize,
}

/// Everything one run produced.
#[derive(Debug)]
pub struct RunOutcome {
    /// Wake-jitter histogram and the miss counter.
    pub jitter: JitterStats,
    /// Every miss, explained. Not truncated here; the JSON writer caps it.
    pub misses: Vec<Miss>,
    /// Cycles actually executed.
    pub executed_cycles: u64,
    /// Accepted sensor frames.
    pub accepted: u32,
    /// Rejected sensor frames.
    pub rejected: u32,
    /// Rejections by reason, indexed by `Reject::index`.
    pub rejected_by_reason: [u32; REJECT_KINDS],
    /// The latched safe state at the end of the run, if any.
    pub safe_state: Option<SafeState>,
    /// Cycle at which the safe state latched.
    pub trip_cycle: Option<u64>,
    /// Whether the commanded actuator was 0 on every cycle from the trip to
    /// the end of the run. `true` when nothing tripped.
    pub actuator_zero_since_trip: bool,
    /// The last commanded actuator value.
    pub last_actuator: f32,
    /// Whether the elevated scheduling policy was granted.
    pub scheduling: SchedulingReport,
    /// Wall-clock length of the run in microseconds.
    pub wall_us: u64,
    /// For a `cpu-hog` run: how many spinners were spawned and how many had
    /// the elevated policy granted. `None` for every other run.
    pub hog: Option<HogOutcome>,
}

/// What the deliberate contention actually consisted of.
#[derive(Debug, Clone, Copy)]
pub struct HogOutcome {
    /// Spinners spawned.
    pub threads: usize,
    /// Spinners that had the elevated scheduling policy granted.
    pub policy_granted: usize,
    /// First cycle they ran on.
    pub start_cycle: u64,
    /// First cycle after they were stopped.
    pub end_cycle: u64,
}

/// Runs the periodic loop.
///
/// The per-cycle order is fixed: wait for the scheduled start, receive,
/// step the node, transmit. Receive precedes the step so a frame that arrived
/// during the previous period's sleep counts toward this period's freshness,
/// and transmit follows it so the command leaves in the same period as the
/// reading that produced it.
pub fn run(bus: &mut dyn CanBus, cfg: RunConfig, hog_window: Option<HogWindow>) -> RunOutcome {
    let scheduling = match cfg.mode {
        Mode::RealTime => request_realtime_policy(cfg.period_us),
        // Asking for a real-time policy in virtual time would be theatre: the
        // loop never sleeps and never competes for a core on a deadline.
        Mode::Virtual => SchedulingReport {
            requested: "none".to_string(),
            granted: false,
            detail: "virtual-time mode does not sleep, so no scheduling policy is requested"
                .to_string(),
        },
    };

    let mut hog = hog_window.map(|w| (w, CpuHog::new(w.threads)));
    let mut node = ControlNode::new(0);
    let mut jitter = JitterStats::new();
    let mut misses: Vec<Miss> = Vec::new();
    let mut trip_cycle: Option<u64> = None;
    let mut safe_state: Option<SafeState> = None;
    let mut actuator_zero_since_trip = true;
    let mut last_actuator = 0.0f32;
    let mut accepted = 0u32;
    let mut rejected = 0u32;

    let origin = Instant::now();
    for cycle in 0..cfg.cycles {
        let scheduled_us = cycle * cfg.period_us;

        if let Some((w, h)) = hog.as_mut() {
            if cycle == w.start_cycle {
                h.start(cfg.period_us);
            }
            if cycle == w.end_cycle {
                h.stop();
            }
        }

        let start_us = match cfg.mode {
            Mode::RealTime => {
                let target = origin + Duration::from_micros(scheduled_us);
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
                origin.elapsed().as_micros() as u64
            }
            // Virtual time is exactly on schedule by definition, which is why
            // its jitter is not evidence of anything.
            Mode::Virtual => scheduled_us,
        };

        let jitter_us = start_us.saturating_sub(scheduled_us);
        jitter.record(jitter_us);

        // Whole periods that went by before this cycle even started. Each one
        // is a period in which the actuator received no command.
        let skipped = jitter_us / cfg.period_us;
        for k in 0..skipped {
            jitter.record_miss();
            misses.push(Miss {
                cycle,
                kind: MissKind::SkippedPeriod,
                jitter_us,
                work_us: 0,
                overrun_us: jitter_us - k * cfg.period_us,
            });
        }

        let frames = bus.recv_until(scheduled_us + cfg.period_us);
        let out = node.step(start_us, &frames);
        if let Some(f) = out.command_frame {
            let _ = bus.send(f);
        }

        accepted = out.accepted;
        rejected = out.rejected;
        last_actuator = out.actuator;
        if let Some(s) = out.safe_state {
            if trip_cycle.is_none() {
                trip_cycle = Some(cycle);
                safe_state = Some(s);
            }
        }
        if trip_cycle.is_some() && out.actuator != 0.0 {
            actuator_zero_since_trip = false;
        }

        let end_us = match cfg.mode {
            Mode::RealTime => origin.elapsed().as_micros() as u64,
            // Virtual time charges no work duration: there is no wall clock to
            // charge it against, and pretending otherwise would invent a
            // number.
            Mode::Virtual => scheduled_us,
        };
        let work_us = end_us.saturating_sub(start_us);
        let deadline_us = scheduled_us + cfg.period_us;
        if end_us > deadline_us {
            jitter.record_miss();
            misses.push(Miss {
                cycle,
                kind: MissKind::WorkOverran,
                jitter_us,
                work_us,
                overrun_us: end_us - deadline_us,
            });
        }
    }

    let hog_outcome = hog.as_mut().map(|(w, h)| {
        h.stop();
        HogOutcome {
            threads: h.threads(),
            policy_granted: h.granted(),
            start_cycle: w.start_cycle,
            end_cycle: w.end_cycle,
        }
    });

    RunOutcome {
        jitter,
        misses,
        executed_cycles: cfg.cycles,
        accepted,
        rejected,
        rejected_by_reason: *node.rejected_by_reason(),
        safe_state,
        trip_cycle,
        actuator_zero_since_trip,
        last_actuator,
        scheduling,
        wall_us: origin.elapsed().as_micros() as u64,
        hog: hog_outcome,
    }
}
