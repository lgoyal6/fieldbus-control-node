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

/// Counts whole periods that elapsed with no cycle starting in them.
///
/// The naive version of this counted `jitter / period` on every late cycle,
/// which counts the *same* wall-clock periods again on each cycle that is
/// still catching up: one 173 ms stall was reported as 153 skipped periods
/// instead of 17. This ledger records the last period a cycle actually
/// started in, so each period is counted at most once, no matter how many
/// cycles run late inside it afterwards.
#[derive(Debug, Clone, Copy)]
pub struct PeriodLedger {
    period_us: u64,
    next_unaccounted: u64,
}

impl PeriodLedger {
    /// A ledger for a loop of the given period, before any cycle has started.
    pub fn new(period_us: u64) -> Self {
        PeriodLedger {
            period_us: period_us.max(1),
            next_unaccounted: 0,
        }
    }

    /// Records that a cycle started at `start_us` and returns how many whole
    /// periods went by with no cycle starting in them.
    pub fn account_cycle_start(&mut self, start_us: u64) -> u64 {
        let period = start_us / self.period_us;
        let skipped = period.saturating_sub(self.next_unaccounted);
        self.next_unaccounted = period + 1;
        skipped
    }
}

/// Which scheduling policy a thread asks the operating system for.
///
/// Two values, because the `cpu-hog` control needs the two sides of the
/// machine to be in different scheduling bands and every other run needs them
/// in the same one. Which value each side uses is read from
/// `manifest/frozen.json`, never chosen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPolicy {
    /// Request the Mach time-constraint policy (`SCHED_FIFO` on Linux). This
    /// is what every positive run asks for and what v1 gave both sides.
    TimeConstraint,
    /// Request nothing at all and run in the platform's default timeshare
    /// band. Used by the control thread on the `cpu-hog` control run only, so
    /// that the hog is in a strictly higher band than the loop it is meant to
    /// disturb.
    DefaultTimeshare,
}

impl ThreadPolicy {
    /// The stable name used in `manifest/frozen.json` and in `results/`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ThreadPolicy::TimeConstraint => "mach-time-constraint",
            ThreadPolicy::DefaultTimeshare => "default-timeshare",
        }
    }

    /// Parses a policy name from the frozen manifest.
    ///
    /// An unknown name is an error rather than a fallback: a manifest that
    /// names a policy this binary does not implement is a manifest describing
    /// a different experiment, and running anyway would produce numbers
    /// labelled with the wrong one.
    pub fn from_manifest(name: &str) -> Result<ThreadPolicy, String> {
        match name {
            "mach-time-constraint" => Ok(ThreadPolicy::TimeConstraint),
            "default-timeshare" => Ok(ThreadPolicy::DefaultTimeshare),
            other => Err(format!(
                "manifest names an unknown thread policy {other:?}; this binary implements \
                 \"mach-time-constraint\" and \"default-timeshare\""
            )),
        }
    }

    /// Applies the policy to the calling thread and reports what happened.
    pub fn apply(&self, period_us: u64) -> SchedulingReport {
        match self {
            ThreadPolicy::TimeConstraint => request_realtime_policy(period_us),
            ThreadPolicy::DefaultTimeshare => SchedulingReport {
                requested: "none: the default timeshare policy".to_string(),
                granted: false,
                detail: "no thread_policy_set call was made on this thread".to_string(),
                qos_class: qos_class_of_current_thread(),
            },
        }
    }
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
    /// The QoS class the thread ended up in, read back from the operating
    /// system after the request. Recorded rather than assumed: it is the half
    /// of the priority relation that the request itself does not state, and
    /// on macOS a thread granted the time-constraint policy leaves the QoS
    /// bands entirely, which is exactly the fact the `cpu-hog` control rests
    /// on.
    pub qos_class: String,
}

impl SchedulingReport {
    /// The single-line form written into `results/` as `scheduling_policy`.
    pub fn summary(&self) -> String {
        format!(
            "requested={}, granted={}, detail={}, qos_class={}",
            self.requested, self.granted, self.detail, self.qos_class
        )
    }
}

/// The QoS class of the calling thread, read back from the kernel.
///
/// `pthread_get_qos_class_np` is not in the `libc` crate, so it is declared
/// here. It is a read of the calling thread's own state and cannot fail in a
/// way that matters; a failure is reported as unknown rather than fatal.
#[cfg(target_os = "macos")]
pub fn qos_class_of_current_thread() -> String {
    extern "C" {
        fn pthread_get_qos_class_np(
            thread: libc::pthread_t,
            qos_class: *mut u32,
            relative_priority: *mut i32,
        ) -> i32;
    }
    let mut class: u32 = 0;
    let mut relative: i32 = 0;
    let rc = unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative) };
    if rc != 0 {
        return format!("unknown (pthread_get_qos_class_np rc={rc})");
    }
    // Values from <sys/qos.h>.
    let name = match class {
        0x21 => "QOS_CLASS_USER_INTERACTIVE",
        0x19 => "QOS_CLASS_USER_INITIATED",
        0x15 => "QOS_CLASS_DEFAULT",
        0x11 => "QOS_CLASS_UTILITY",
        0x09 => "QOS_CLASS_BACKGROUND",
        0x00 => "QOS_CLASS_UNSPECIFIED",
        _ => "unrecognised",
    };
    format!("{name} (0x{class:02x}), relative_priority={relative}")
}

/// No QoS classes outside macOS.
#[cfg(not(target_os = "macos"))]
pub fn qos_class_of_current_thread() -> String {
    "not applicable on this platform".to_string()
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
                qos_class: qos_class_of_current_thread(),
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
        qos_class: qos_class_of_current_thread(),
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
        qos_class: qos_class_of_current_thread(),
    }
}

/// No elevated policy is attempted on other platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn request_realtime_policy(_period_us: u64) -> SchedulingReport {
    SchedulingReport {
        requested: "none".to_string(),
        granted: false,
        detail: "no elevated scheduling policy is implemented for this platform".to_string(),
        qos_class: qos_class_of_current_thread(),
    }
}

/// Frozen loop parameters for one run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Number of periods to execute.
    pub cycles: u64,
    /// Period length in microseconds.
    pub period_us: u64,
    /// What the control thread asks the scheduler for. Every positive run
    /// asks for the time-constraint policy; the `cpu-hog` control run is the
    /// only thing in this repository that does not.
    pub control_thread_policy: ThreadPolicy,
    /// Sensor staleness budget in microseconds, read from the frozen manifest
    /// and handed to the node rather than taken from a constant.
    pub watchdog_timeout_us: u64,
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
    /// What each spinner asks the scheduler for. For the control to mean
    /// what it claims, this must be a strictly higher band than
    /// [`RunConfig::control_thread_policy`].
    pub policy: ThreadPolicy,
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
    /// What the control thread asked the scheduler for.
    pub control_thread_policy: ThreadPolicy,
    /// Whether that request was granted.
    pub scheduling: SchedulingReport,
    /// Wall-clock length of the run in microseconds.
    pub wall_us: u64,
    /// For a `cpu-hog` run: what the contention was and what it did, split by
    /// whether a cycle fell inside the hog window. `None` for every other run.
    pub hog: Option<HogOutcome>,
}

/// What the deliberate contention actually consisted of, and what it did.
///
/// The attribution fields are the point. The run's own head and tail are
/// uncontended and run under exactly the same scheduling policy as the
/// contended middle, so they are a within-run baseline: whatever separates
/// inside from outside is the hog, and whatever they share is not.
#[derive(Debug, Clone)]
pub struct HogOutcome {
    /// Spinners spawned.
    pub threads: usize,
    /// What each spinner asked the scheduler for.
    pub policy: ThreadPolicy,
    /// The request, named the way the operating system names it.
    pub policy_requested: String,
    /// Spinners that had that policy granted.
    pub policy_granted: usize,
    /// The QoS class a spinner ended up in, read back from the kernel.
    pub qos_class: String,
    /// First cycle they ran on.
    pub start_cycle: u64,
    /// First cycle after they were stopped.
    pub end_cycle: u64,
    /// Deadline misses at cycles inside the window.
    pub misses_inside_window: u64,
    /// Deadline misses at cycles outside it.
    pub misses_outside_window: u64,
    /// Wake jitter of cycles inside the window.
    pub jitter_inside_window: JitterStats,
    /// Wake jitter of cycles outside it.
    pub jitter_outside_window: JitterStats,
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
        Mode::RealTime => cfg.control_thread_policy.apply(cfg.period_us),
        // Asking for a real-time policy in virtual time would be theatre: the
        // loop never sleeps and never competes for a core on a deadline.
        Mode::Virtual => SchedulingReport {
            requested: "none".to_string(),
            granted: false,
            detail: "virtual-time mode does not sleep, so no scheduling policy is requested"
                .to_string(),
            qos_class: qos_class_of_current_thread(),
        },
    };

    // Created and spawned here, before the clock origin is taken, so the
    // cost of making 22 threads is paid outside the measured loop. Inside the
    // loop the only operations on it are activate and deactivate, both of
    // which are a lock, a store and a notify.
    let mut hog = hog_window.map(|w| {
        let mut h = CpuHog::new(w.threads, w.policy);
        h.spawn(cfg.period_us);
        (w, h)
    });
    // Only allocated for a hog run, and only ever read for one.
    let mut inside_jitter = JitterStats::new();
    let mut outside_jitter = JitterStats::new();
    let mut misses_inside = 0u64;
    let mut misses_outside = 0u64;
    let mut node = ControlNode::with_watchdog_timeout(0, cfg.watchdog_timeout_us);
    let mut jitter = JitterStats::new();
    let mut ledger = PeriodLedger::new(cfg.period_us);
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
            // Flag flips only. Spawning and joining both happen outside this
            // loop, because doing either inside it blocks the loop long
            // enough to manufacture the very deadline misses this control is
            // supposed to detect. It did exactly that twice before this.
            if cycle == w.start_cycle {
                h.activate();
            }
            if cycle == w.end_cycle {
                h.deactivate();
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

        // Attribution. A cycle is inside the window if the spinners were
        // running when it started. Splitting the same measurement two ways is
        // what makes the run its own baseline, rather than something to be
        // compared against a different run on a differently loaded machine.
        let in_window = hog
            .as_ref()
            .is_some_and(|(w, _)| cycle >= w.start_cycle && cycle < w.end_cycle);
        if in_window {
            inside_jitter.record(jitter_us);
        } else {
            outside_jitter.record(jitter_us);
        }

        // Whole periods that went by with no cycle starting in them. Each one
        // is a period in which the actuator received no command. The ledger
        // makes sure a given period is counted once, not once per cycle that
        // is still running late inside it.
        let skipped = ledger.account_cycle_start(start_us);
        for k in 0..skipped {
            jitter.record_miss();
            if in_window {
                misses_inside += 1;
            } else {
                misses_outside += 1;
            }
            misses.push(Miss {
                cycle,
                kind: MissKind::SkippedPeriod,
                jitter_us,
                work_us: 0,
                overrun_us: jitter_us.saturating_sub(k * cfg.period_us),
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
            if in_window {
                misses_inside += 1;
            } else {
                misses_outside += 1;
            }
            misses.push(Miss {
                cycle,
                kind: MissKind::WorkOverran,
                jitter_us,
                work_us,
                overrun_us: end_us - deadline_us,
            });
        }
    }

    // Join here, outside the timed path, so the cost of tearing the hog down
    // lands on nobody's deadline.
    let hog_outcome = hog.as_mut().map(|(w, h)| {
        h.stop();
        HogOutcome {
            threads: h.threads(),
            policy: w.policy,
            policy_requested: h.policy_requested(),
            policy_granted: h.policy_granted(),
            qos_class: h.qos_class(),
            start_cycle: w.start_cycle,
            end_cycle: w.end_cycle,
            misses_inside_window: misses_inside,
            misses_outside_window: misses_outside,
            jitter_inside_window: inside_jitter,
            jitter_outside_window: outside_jitter,
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
        control_thread_policy: cfg.control_thread_policy,
        scheduling,
        wall_us: origin.elapsed().as_micros() as u64,
        hog: hog_outcome,
    }
}
