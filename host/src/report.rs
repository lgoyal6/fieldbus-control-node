//! The JSON result files and the gate evaluation that decides pass or fail.
//!
//! Every field here is written so a reader can tell what was measured from
//! what was assumed. In particular the environment block states, on every
//! single run, that the bus and sensor are simulated, that no CAN hardware is
//! attached, that this is one host, that there is no RTOS and that nothing was
//! hardware in the loop. Those are not decorations: a timing number without
//! them is a claim about a machine the reader cannot identify.
//!
//! Gate evaluation is a list of named checks, each carrying what was expected,
//! what was observed and whether it held. A single boolean would be smaller
//! and would make a failure impossible to diagnose or to audit against the
//! frozen manifest.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use fieldbus_core::j1939::{Reject, REJECT_KINDS};
use fieldbus_core::telemetry::BUCKET_WIDTH_US;

use fieldbus_core::telemetry::JitterStats;

use crate::manifest::{Manifest, NegativeControl};
use crate::runner::{HogOutcome, Mode, RunOutcome, SchedulingReport, ThreadPolicy};

/// How many explained misses are written to JSON before the list is capped.
///
/// The count is always exact and never capped; only the per-miss detail is.
/// Under the `cpu-hog` control the miss list can run to thousands of entries,
/// and a result file nobody can open is not evidence.
pub const MAX_EXPLAINED_MISSES: usize = 200;

/// What the numbers were produced on, and what they were not produced on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    /// Operating system, from the compiler's own target constants.
    pub os: String,
    /// Target architecture.
    pub arch: String,
    /// CPU model string as the kernel reports it.
    pub cpu_model: String,
    /// Logical core count.
    pub core_count: usize,
    /// The elevated scheduling policy request and its outcome.
    pub scheduling_policy: String,
    /// Always `"simulated"`: the bus and the sensor are in-process models.
    pub mode: String,
    /// `"real-time"` or `"virtual-time"`.
    pub clock_mode: String,
    /// Number of machines involved. Always 1.
    pub host_count: u32,
    /// Whether a CAN transceiver or adapter was involved. Always false here.
    pub physical_can_hardware: bool,
    /// Whether any physical device was in the loop. Always false here.
    pub hardware_in_the_loop: bool,
    /// Whether a real-time operating system was used. Always false here.
    pub rtos: bool,
    /// Plain-language statement of what this run is, carried in the data so
    /// it survives being quoted out of context.
    pub statement: String,
}

impl Environment {
    /// Describes the current process and the given clock mode.
    pub fn detect(clock_mode: Mode, scheduling_policy: String) -> Environment {
        Environment {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_model: cpu_model(),
            core_count: crate::hog::logical_cores(),
            scheduling_policy,
            mode: "simulated".to_string(),
            clock_mode: clock_mode.as_str().to_string(),
            host_count: 1,
            physical_can_hardware: false,
            hardware_in_the_loop: false,
            rtos: false,
            statement: "Simulated in-process CAN bus and simulated first-order sensor plant, \
                        single host, no physical CAN hardware, not hardware in the loop, no \
                        RTOS. A POSIX-hosted periodic loop with deadline telemetry, not a hard \
                        real-time system."
                .to_string(),
        }
    }
}

/// CPU model string, or `"unknown"` if the kernel will not say.
#[cfg(target_os = "macos")]
fn cpu_model() -> String {
    let name = c"machdep.cpu.brand_string";
    let mut len: libc::size_t = 0;
    // Two calls: the first asks how long the answer is, the second reads it.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return "unknown".to_string();
    }
    let mut buf = vec![0u8; len];
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return "unknown".to_string();
    }
    buf.truncate(len);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// CPU model string from `/proc/cpuinfo`, or `"unknown"`.
#[cfg(not(target_os = "macos"))]
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name") || l.starts_with("Model"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Which manifest this run was gated against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRef {
    /// Path as given on the command line.
    pub path: String,
    /// SHA-256 of the manifest bytes, computed by this binary at run time.
    /// This is what ties a number to the thresholds that were frozen before
    /// it existed.
    pub sha256: String,
}

impl ManifestRef {
    /// Hashes the manifest file at `path`.
    pub fn of(path: &Path) -> Result<ManifestRef, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut h = Sha256::new();
        h.update(&bytes);
        Ok(ManifestRef {
            path: path.display().to_string(),
            sha256: format!("{:x}", h.finalize()),
        })
    }
}

/// Jitter summary, with its own resolution stated inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitterReport {
    /// p50, as an upper-edge bound.
    pub p50: u64,
    /// p95, as an upper-edge bound.
    pub p95: u64,
    /// p99, as an upper-edge bound.
    pub p99: u64,
    /// Exact largest sample.
    pub max: u64,
    /// Exact smallest sample.
    pub min: u64,
    /// Arithmetic mean, truncated.
    pub mean: u64,
    /// Number of samples.
    pub samples: u64,
    /// Histogram bucket width in microseconds.
    pub histogram_bucket_width_us: u64,
    /// Whether any sample exceeded the histogram range, which would make the
    /// percentiles lower bounds.
    pub histogram_overflowed: bool,
    /// How to read the percentile figures.
    pub percentile_reporting: String,
}

/// One explained deadline miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissReport {
    /// Cycle index.
    pub cycle: u64,
    /// `work_overran` or `skipped_period`.
    pub kind: String,
    /// Wake jitter of that cycle.
    pub jitter_us: u64,
    /// Work duration of that cycle.
    pub work_us: u64,
    /// How far past the deadline it finished.
    pub overrun_us: u64,
}

/// Deadline accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadlineReport {
    /// Exact total, never capped.
    pub count: u64,
    /// Per-miss detail, capped at [`MAX_EXPLAINED_MISSES`].
    pub explained: Vec<MissReport>,
    /// Whether `explained` was capped.
    pub explained_truncated: bool,
    /// What counts as a miss.
    pub definition: String,
}

/// Frame accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanReport {
    /// Frames the validator accepted.
    pub accepted: u32,
    /// Frames the validator refused.
    pub rejected: u32,
    /// Refusals by reason.
    pub rejected_by_reason: std::collections::BTreeMap<String, u32>,
}

/// Watchdog outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogReport {
    /// Whether a safe state latched during the run.
    pub tripped: bool,
    /// The reason name, if it tripped.
    pub reason: Option<String>,
    /// `tripped_at_us - last_seen_us`, if it tripped: the whole path from the
    /// last accepted sensor frame to the latched safe state.
    pub reaction_time_us: Option<u64>,
    /// What that number means, carried with it so it cannot be read as the v1
    /// quantity of the same name.
    pub reaction_time_definition: String,
    /// The frozen bound on that path, from the `sensor-freeze` control.
    pub reaction_bound_us: Option<u64>,
    /// `reaction_bound_us - reaction_time_us`. Negative means the bound was
    /// missed.
    pub reaction_margin_us: Option<i64>,
    /// `tripped_at_us - (last_seen_us + timeout_us)`: how far past its own
    /// internal budget the trip landed. A diagnostic on how often the watchdog
    /// is evaluated, not the reported reaction. This is the quantity v1 called
    /// `reaction_time_us`.
    pub overshoot_past_budget_us: Option<u64>,
    /// Cycle at which it latched.
    pub trip_cycle: Option<u64>,
    /// Whether the commanded actuator was 0 for every cycle after the trip.
    pub actuator_zero_since_trip: bool,
    /// The staleness budget in force, read from the frozen manifest.
    pub timeout_us: u64,
}

/// What one thread asked the scheduler for and what it got.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadPolicyReport {
    /// The manifest's name for the policy: `mach-time-constraint` or
    /// `default-timeshare`.
    pub policy: String,
    /// The request as the operating system names it.
    pub requested: String,
    /// Whether it was granted. A `default-timeshare` thread requests nothing,
    /// so this is false for it by construction and not a failure.
    pub granted: bool,
    /// Return code or reason, verbatim.
    pub detail: String,
    /// The QoS class the thread ended up in, read back from the kernel.
    pub qos_class: String,
}

/// The `cpu-hog` control's contention, and what separates the contended part
/// of the run from the rest of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HogReport {
    /// Spinners spawned.
    pub threads: usize,
    /// What they asked for and what they got.
    pub policy: ThreadPolicyReport,
    /// How many of them had it granted.
    pub policy_granted: usize,
    /// First contended cycle.
    pub window_start_cycle: u64,
    /// First cycle after the window.
    pub window_end_cycle: u64,
    /// Deadline misses at cycles inside the window.
    pub misses_inside_window: u64,
    /// Deadline misses at cycles outside it.
    pub misses_outside_window: u64,
    /// Wake jitter inside the window.
    pub jitter_inside_window: JitterReport,
    /// Wake jitter outside it, which is this run's own baseline: same thread,
    /// same policy, no hog.
    pub jitter_outside_window: JitterReport,
    /// How the two halves compare, in words, so the number a reader quotes
    /// carries its own caveat.
    pub attribution: String,
}

/// One named gate condition and whether it held.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// What is being checked.
    pub name: String,
    /// What the frozen manifest requires.
    pub expected: String,
    /// What this run produced.
    pub observed: String,
    /// Whether the condition held.
    pub passed: bool,
}

/// The positive gate's verdict on one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    /// Every condition, evaluated.
    pub checks: Vec<Check>,
    /// True only if every check passed.
    pub pass: bool,
}

impl GateReport {
    /// Builds a verdict from a list of checks.
    pub fn from_checks(checks: Vec<Check>) -> GateReport {
        let pass = checks.iter().all(|c| c.passed);
        GateReport { checks, pass }
    }
}

/// A negative control's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlReport {
    /// Control identifier, matching the manifest.
    pub id: String,
    /// What the manifest says should happen.
    pub expected: String,
    /// Every condition the manifest attaches to "caught", evaluated.
    pub caught_checks: Vec<Check>,
    /// True only if every caught-condition held.
    pub caught: bool,
    /// Stated plainly for a reader who only looks at one field.
    pub note: String,
}

/// One run's complete result file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Which run this is: `positive-1`, `cpu-hog`, and so on.
    pub run_id: String,
    /// What produced the numbers.
    pub environment: Environment,
    /// Which frozen manifest applied.
    pub manifest: ManifestRef,
    /// Period in microseconds.
    pub target_period_us: u64,
    /// Cycles executed.
    pub cycles: u64,
    /// Wall-clock length of the run.
    pub wall_us: u64,
    /// Wake jitter.
    pub jitter_us: JitterReport,
    /// Deadline misses.
    pub missed_deadlines: DeadlineReport,
    /// Frame accounting.
    pub can: CanReport,
    /// Watchdog outcome.
    pub watchdog: WatchdogReport,
    /// What the control thread asked the scheduler for and what it got.
    pub control_thread_policy: ThreadPolicyReport,
    /// Present only for the `cpu-hog` control: the contention and its
    /// attribution.
    pub hog: Option<HogReport>,
    /// The positive gate, evaluated against this run whether or not it is a
    /// positive run. For a negative control, a failing gate is the point.
    pub gate: GateReport,
    /// Present only for a negative control.
    pub control: Option<ControlReport>,
}

/// Summarises one jitter histogram.
pub fn jitter_report(j: &JitterStats) -> JitterReport {
    JitterReport {
        p50: j.p50_us(),
        p95: j.p95_us(),
        p99: j.p99_us(),
        max: j.max_us(),
        min: j.min_us(),
        mean: j.mean_us(),
        samples: j.count(),
        histogram_bucket_width_us: BUCKET_WIDTH_US,
        histogram_overflowed: j.overflowed(),
        percentile_reporting: format!(
            "nearest-rank, reported as the upper edge of the containing {BUCKET_WIDTH_US} us \
             bucket. Buckets are half-open, so a reported value r means the true value lies in \
             [r - {BUCKET_WIDTH_US}, r). An all-zero run reports every percentile as \
             {BUCKET_WIDTH_US} while max reports 0. min and max are exact."
        ),
    }
}

/// Describes one thread's scheduling request and its outcome.
pub fn thread_policy_report(policy: ThreadPolicy, s: &SchedulingReport) -> ThreadPolicyReport {
    ThreadPolicyReport {
        policy: policy.as_str().to_string(),
        requested: s.requested.clone(),
        granted: s.granted,
        detail: s.detail.clone(),
        qos_class: s.qos_class.clone(),
    }
}

/// Summarises the contention and, more importantly, what separates the
/// contended cycles from the rest of the same run.
pub fn hog_report(h: &HogOutcome) -> HogReport {
    let inside = jitter_report(&h.jitter_inside_window);
    let outside = jitter_report(&h.jitter_outside_window);
    let attribution = format!(
        "Cycles {}..{} ran with {} spinners at the {} policy ({} of them granted it, QoS class \
         {}). The rest of the run is the same thread at the same policy with no hog, so it is \
         this run's own baseline. Missed deadlines: {} inside the window, {} outside it. Wake \
         jitter p99: {} us inside, {} us outside; max {} us inside, {} us outside. Read the two \
         together: a p99 that is already above the gate threshold outside the window is the \
         control thread's own policy, not the contention, and only what separates inside from \
         outside is the hog.",
        h.start_cycle,
        h.end_cycle,
        h.threads,
        h.policy.as_str(),
        h.policy_granted,
        h.qos_class,
        h.misses_inside_window,
        h.misses_outside_window,
        inside.p99,
        outside.p99,
        inside.max,
        outside.max
    );
    HogReport {
        threads: h.threads,
        policy: ThreadPolicyReport {
            policy: h.policy.as_str().to_string(),
            requested: h.policy_requested.clone(),
            granted: h.policy_granted == h.threads && h.threads > 0,
            detail: format!(
                "{} of {} spinners were granted it",
                h.policy_granted, h.threads
            ),
            qos_class: h.qos_class.clone(),
        },
        policy_granted: h.policy_granted,
        window_start_cycle: h.start_cycle,
        window_end_cycle: h.end_cycle,
        misses_inside_window: h.misses_inside_window,
        misses_outside_window: h.misses_outside_window,
        jitter_inside_window: inside,
        jitter_outside_window: outside,
        attribution,
    }
}

/// Builds a run report from an outcome and the frozen manifest.
pub fn build_run_report(
    run_id: &str,
    outcome: &RunOutcome,
    cfg_mode: Mode,
    period_us: u64,
    manifest: &Manifest,
    manifest_ref: ManifestRef,
) -> RunReport {
    let j = &outcome.jitter;
    let mut by_reason = std::collections::BTreeMap::new();
    for i in 0..REJECT_KINDS {
        by_reason.insert(Reject::name(i).to_string(), outcome.rejected_by_reason[i]);
    }

    let explained: Vec<MissReport> = outcome
        .misses
        .iter()
        .take(MAX_EXPLAINED_MISSES)
        .map(|m| MissReport {
            cycle: m.cycle,
            kind: m.kind.as_str().to_string(),
            jitter_us: m.jitter_us,
            work_us: m.work_us,
            overrun_us: m.overrun_us,
        })
        .collect();

    let gate = evaluate_positive_gate(outcome, manifest);

    let reaction = outcome.safe_state.map(|s| s.time_since_last_accepted_us());
    // The bound belongs to the sensor-freeze control, but it is reported on
    // every run: a positive run that trips at all has already failed, and a
    // reader should not have to open a different file to see by how much.
    let bound = manifest
        .control("sensor-freeze")
        .ok()
        .and_then(|c| c.max_reaction_time_us);

    RunReport {
        run_id: run_id.to_string(),
        environment: Environment::detect(cfg_mode, outcome.scheduling.summary()),
        manifest: manifest_ref,
        target_period_us: period_us,
        cycles: outcome.executed_cycles,
        wall_us: outcome.wall_us,
        jitter_us: jitter_report(j),
        missed_deadlines: DeadlineReport {
            count: j.missed_deadlines(),
            explained_truncated: outcome.misses.len() > explained.len(),
            explained,
            definition: "A cycle whose work finished after its own scheduled start plus one \
                         period, plus every whole period that elapsed with no cycle executing \
                         in it."
                .to_string(),
        },
        can: CanReport {
            accepted: outcome.accepted,
            rejected: outcome.rejected,
            rejected_by_reason: by_reason,
        },
        watchdog: WatchdogReport {
            tripped: outcome.safe_state.is_some(),
            reason: outcome.safe_state.map(|s| s.reason_name().to_string()),
            reaction_time_us: reaction,
            reaction_time_definition: "tripped_at_us - last_seen_us: the whole path from the \
                                       timestamp of the last accepted sensor frame to the check \
                                       that latched the safe state. Bounded by the sensor-freeze \
                                       control's max_reaction_time_us in the frozen manifest."
                .to_string(),
            reaction_bound_us: bound,
            reaction_margin_us: match (bound, reaction) {
                (Some(b), Some(r)) => Some(b as i64 - r as i64),
                _ => None,
            },
            overshoot_past_budget_us: outcome.safe_state.map(|s| s.overshoot_past_budget_us()),
            trip_cycle: outcome.trip_cycle,
            actuator_zero_since_trip: outcome.actuator_zero_since_trip,
            timeout_us: manifest.watchdog.timeout_us,
        },
        control_thread_policy: thread_policy_report(
            outcome.control_thread_policy,
            &outcome.scheduling,
        ),
        hog: outcome.hog.as_ref().map(hog_report),
        gate,
        control: None,
    }
}

/// Evaluates the four frozen positive-gate conditions against a run.
///
/// Applied to every run, including the negative controls. For a control, the
/// gate is expected to fail, and the failure is what "caught" means.
pub fn evaluate_positive_gate(outcome: &RunOutcome, manifest: &Manifest) -> GateReport {
    let g = &manifest.positive_gate;
    let misses = outcome.jitter.missed_deadlines();
    let p99 = outcome.jitter.p99_us();
    let tripped = outcome.safe_state.is_some();
    GateReport::from_checks(vec![
        Check {
            name: "missed_deadlines".to_string(),
            expected: format!("<= {}", g.missed_deadlines_max),
            observed: misses.to_string(),
            passed: misses <= g.missed_deadlines_max,
        },
        Check {
            name: "p99_jitter_us".to_string(),
            expected: format!("<= {}", g.p99_jitter_us_max),
            observed: p99.to_string(),
            passed: p99 <= g.p99_jitter_us_max,
        },
        Check {
            name: "rejected_frames".to_string(),
            expected: format!("<= {}", g.rejected_frames_max),
            observed: outcome.rejected.to_string(),
            passed: outcome.rejected <= g.rejected_frames_max,
        },
        Check {
            name: "watchdog_tripped".to_string(),
            expected: format!("{}", g.watchdog_may_trip),
            observed: tripped.to_string(),
            passed: g.watchdog_may_trip || !tripped,
        },
    ])
}

/// Evaluates whether the `cpu-hog` control was caught.
///
/// Caught means the positive gate failed. That criterion is unchanged from v1
/// and is deliberately the only one: the control creates contention and either
/// the gate notices or it does not.
///
/// What is new is that "caught" is no longer the whole story, and the manifest
/// says so before the run rather than after it. The control thread is demoted
/// out of the time-constraint band for this run so the hog can be strictly
/// above it, and that demotion alone costs about 2 ms of wake latency on this
/// host. So the p99 condition can fail without the hog contributing anything.
/// The diagnostics below carry the split the manifest pre-registered: misses
/// inside the window against misses outside it, and the same for jitter. They
/// do not change the verdict, because a diagnostic that can flip a verdict is
/// a threshold, and thresholds are frozen.
pub fn evaluate_cpu_hog(
    gate: &GateReport,
    hog: Option<&crate::runner::HogOutcome>,
) -> ControlReport {
    let caught = !gate.pass;
    let threads = hog.map(|h| h.threads).unwrap_or(0);
    let granted = hog.map(|h| h.policy_granted).unwrap_or(0);
    let inside = hog.map(|h| h.misses_inside_window).unwrap_or(0);
    let outside = hog.map(|h| h.misses_outside_window).unwrap_or(0);
    let p99_in = hog.map(|h| h.jitter_inside_window.p99_us()).unwrap_or(0);
    let p99_out = hog.map(|h| h.jitter_outside_window.p99_us()).unwrap_or(0);
    let policy = hog
        .map(|h| h.policy.as_str().to_string())
        .unwrap_or_else(|| "none".to_string());

    let attributable = inside > 0 && inside > outside;
    ControlReport {
        id: "cpu-hog".to_string(),
        expected: "the positive gate is violated: missed_deadlines > 0 OR p99 jitter > 1000 us"
            .to_string(),
        caught_checks: vec![
            Check {
                name: "positive_gate_fails".to_string(),
                expected: "false (the gate must not pass under contention)".to_string(),
                observed: format!("gate pass = {}", gate.pass),
                passed: caught,
            },
            // Diagnostics from here down: recorded, never part of the verdict,
            // and so deliberately always passing. A control whose threads were
            // refused the policy they asked for, or whose misses landed outside
            // its own window, is a different experiment from the frozen one,
            // and a reader has to be able to see that from the result file.
            Check {
                name: "hog_threads_with_policy_granted".to_string(),
                expected: format!("diagnostic only, {threads} spawned at policy {policy}"),
                observed: format!("{granted} of {threads} granted"),
                passed: true,
            },
            Check {
                name: "misses_inside_versus_outside_the_hog_window".to_string(),
                expected: "diagnostic only: misses attributable to the hog land inside its window"
                    .to_string(),
                observed: format!("{inside} inside, {outside} outside"),
                passed: true,
            },
            Check {
                name: "p99_jitter_inside_versus_outside_the_hog_window".to_string(),
                expected: "diagnostic only: the out-of-window figure is this run's own baseline \
                           for a control thread at the same policy with no hog"
                    .to_string(),
                observed: format!("{p99_in} us inside, {p99_out} us outside"),
                passed: true,
            },
        ],
        caught,
        note: match (caught, attributable) {
            (true, true) => format!(
                "Caught, and attributable. {threads} spinning threads at the {policy} policy \
                 ({granted} granted it) produced {inside} missed deadlines inside the hog window \
                 against {outside} outside it, with p99 wake jitter of {p99_in} us inside \
                 against {p99_out} us outside. The out-of-window figures are the same thread at \
                 the same policy with no hog, so what separates them is the contention."
            ),
            (true, false) => format!(
                "Caught, but NOT attributable to the contention. {threads} spinning threads at \
                 the {policy} policy ({granted} granted it) produced {inside} missed deadlines \
                 inside the hog window against {outside} outside it, with p99 wake jitter of \
                 {p99_in} us inside against {p99_out} us outside. The gate failed, but the \
                 out-of-window baseline in this same run already fails it, which means what the \
                 gate caught is the control thread's own scheduling policy and not the hog. The \
                 manifest pre-registered this reading before the run; it is not a reinterpretation \
                 after one."
            ),
            (false, _) => format!(
                "NOT caught. {threads} spinning threads at the {policy} policy ({granted} \
                 granted it) did not push the loop past the frozen gate: {inside} missed \
                 deadlines inside the window, {outside} outside, p99 {p99_in} us inside against \
                 {p99_out} us outside. The gate has deliberately not been weakened to make this \
                 control fire and the hog has not been made more aggressive than the frozen \
                 mechanism specifies."
            ),
        },
    }
}

/// Evaluates whether the `can-corrupt` control was caught.
///
/// Caught means every injected fault was detected under its intended reason,
/// no legitimate frame was refused, and the run is therefore correctly
/// reported as not clean. A control that merely raised the total rejection
/// count would not distinguish a working validator from one that refuses
/// frames at random.
pub fn evaluate_can_corrupt(
    gate: &GateReport,
    outcome: &RunOutcome,
    spec: &NegativeControl,
) -> ControlReport {
    let mut checks = Vec::new();

    if let Some(total) = spec.expected_rejected_total {
        checks.push(Check {
            name: "rejected_total".to_string(),
            expected: format!("== {total}"),
            observed: outcome.rejected.to_string(),
            passed: outcome.rejected == total,
        });
    }
    if let Some(expected) = &spec.expected_rejected_by_reason {
        for (name, want) in expected {
            let idx = (0..REJECT_KINDS).find(|&i| Reject::name(i) == name.as_str());
            let got = idx.map(|i| outcome.rejected_by_reason[i]);
            checks.push(Check {
                name: format!("rejected_by_reason.{name}"),
                expected: format!("== {want}"),
                observed: match got {
                    Some(v) => v.to_string(),
                    None => "no such reason in this build".to_string(),
                },
                passed: got == Some(*want),
            });
        }
    }
    if let Some(acc) = spec.expected_accepted {
        checks.push(Check {
            name: "accepted".to_string(),
            expected: format!("== {acc}"),
            observed: outcome.accepted.to_string(),
            passed: outcome.accepted == acc,
        });
    }
    checks.push(Check {
        name: "positive_gate_fails".to_string(),
        expected: "false (rejected == 0 must not hold)".to_string(),
        observed: format!("gate pass = {}", gate.pass),
        passed: !gate.pass,
    });

    let caught = checks.iter().all(|c| c.passed);
    ControlReport {
        id: "can-corrupt".to_string(),
        expected: "every injected fault is rejected under its intended reason, no legitimate \
                   frame is rejected, and the positive gate's rejected == 0 condition therefore \
                   fails"
            .to_string(),
        caught,
        note: if caught {
            "Caught. The exact per-reason histogram matched, which shows the validator \
             attributed each fault to the right cause rather than merely counting more \
             refusals."
                .to_string()
        } else {
            "NOT caught. The observed rejection histogram does not match the frozen \
             expectation; see the failing checks."
                .to_string()
        },
        caught_checks: checks,
    }
}

/// Evaluates whether the `sensor-freeze` control was caught.
pub fn evaluate_sensor_freeze(
    gate: &GateReport,
    outcome: &RunOutcome,
    spec: &NegativeControl,
) -> ControlReport {
    let tripped = outcome.safe_state.is_some();
    let reason = outcome
        .safe_state
        .map(|s| s.reason_name().to_string())
        .unwrap_or_else(|| "none".to_string());
    let reaction = outcome.safe_state.map(|s| s.time_since_last_accepted_us());
    let max_reaction = spec.max_reaction_time_us.unwrap_or(u64::MAX);

    let mut checks = vec![
        Check {
            name: "watchdog_tripped".to_string(),
            expected: "true".to_string(),
            observed: tripped.to_string(),
            passed: tripped,
        },
        Check {
            name: "watchdog_reason".to_string(),
            expected: "sensor_stale".to_string(),
            observed: reason.clone(),
            passed: reason == "sensor_stale",
        },
        Check {
            name: "reaction_time_us".to_string(),
            expected: format!(
                "<= {max_reaction} (from the timestamp of the last accepted sensor frame to the \
                 latched safe state)"
            ),
            observed: match reaction {
                Some(v) => format!("{v}, margin {} us", max_reaction as i64 - v as i64),
                None => "never tripped".to_string(),
            },
            passed: reaction.map(|v| v <= max_reaction).unwrap_or(false),
        },
        Check {
            name: "actuator_zero_since_trip".to_string(),
            expected: "true".to_string(),
            observed: outcome.actuator_zero_since_trip.to_string(),
            passed: tripped && outcome.actuator_zero_since_trip,
        },
        Check {
            name: "safe_state_persists_to_end_of_run".to_string(),
            expected: "true".to_string(),
            observed: tripped.to_string(),
            passed: tripped,
        },
        Check {
            name: "positive_gate_fails".to_string(),
            expected: "false (the watchdog must not be allowed to trip)".to_string(),
            observed: format!("gate pass = {}", gate.pass),
            passed: !gate.pass,
        },
    ];
    if let Some(c) = spec.freeze_at_cycle {
        checks.push(Check {
            name: "freeze_cycle_from_manifest".to_string(),
            expected: format!("sensor stops at cycle {c}"),
            observed: format!("trip cycle {:?}", outcome.trip_cycle),
            passed: outcome.trip_cycle.map(|t| t > c).unwrap_or(false),
        });
    }

    let caught = checks.iter().all(|c| c.passed);
    ControlReport {
        id: "sensor-freeze".to_string(),
        expected: "the safe state is entered within max_reaction_time_us of the last accepted \
                   sensor frame, the reason persists to the end of the run, the actuator is \
                   commanded to 0 from the trip onward, and the positive gate's watchdog \
                   condition therefore fails"
            .to_string(),
        caught,
        note: if caught {
            match reaction {
                Some(v) => format!(
                    "Caught. The safe state was entered {v} us after the last accepted sensor \
                     frame, {} us inside the {max_reaction} us bound, and the latch held to the \
                     end of the run with the actuator commanded to the safe value throughout, so \
                     the safe state is a state and not a momentary log line.",
                    max_reaction as i64 - v as i64
                ),
                None => "Caught.".to_string(),
            }
        } else {
            "NOT caught. See the failing checks.".to_string()
        },
        caught_checks: checks,
    }
}

/// The assembled evidence file, `results/completion.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionReport {
    /// What this file is.
    pub what: String,
    /// The frozen manifest and its hash.
    pub manifest: ManifestRef,
    /// The environment, taken from the first positive run.
    pub environment: Environment,
    /// Period in microseconds.
    pub target_period_us: u64,
    /// Cycles per run.
    pub cycles: u64,
    /// Both positive runs, in order.
    pub positive_runs: Vec<RunReport>,
    /// All three negative controls, in order.
    pub negative_controls: Vec<RunReport>,
    /// The verdict.
    pub summary: Summary,
    /// What these numbers do not show.
    pub limitations: Vec<String>,
}

/// The overall verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    /// How many of the two positive runs passed every frozen gate.
    pub positive_runs_passed: usize,
    /// How many positive runs there were.
    pub positive_runs_total: usize,
    /// How many of the three controls were caught.
    pub controls_caught: usize,
    /// How many controls there were.
    pub controls_total: usize,
    /// True only if every positive run passed and every control was caught.
    pub overall_pass: bool,
}

/// The limitations carried inside every completion file, so they travel with
/// the numbers rather than living only in a README nobody quotes.
pub fn limitations() -> Vec<String> {
    vec![
        "The CAN bus is simulated in-process. No CAN adapter, transceiver or physical bus was \
         involved, and no frame left this machine."
            .to_string(),
        "The sensor is a simulated first-order plant driven by the node's own command. There is \
         no physical sensor and no physical actuator."
            .to_string(),
        "This is not hardware in the loop. No device, board, QEMU instance or virtual interface \
         stands in for hardware in these numbers."
            .to_string(),
        "Single host. Every number was produced on one Apple Silicon Mac under macOS with other \
         processes running; there is no cross-machine or cross-platform comparison here."
            .to_string(),
        "Not a hard real-time system. This is a POSIX-hosted periodic loop with deadline \
         telemetry. The Mach time-constraint policy is requested as a best effort and the grant \
         result is recorded; it is not a guarantee, and macOS may demote the thread."
            .to_string(),
        "No RTOS, no ISO 26262 process, no ASIL classification, no automotive qualification and \
         no certification of any kind."
            .to_string(),
        "fieldbus-core is compiled for thumbv7em-none-eabihf as a cross-compile check only. It \
         has never been flashed to or executed on a microcontroller, and no timing figure here \
         describes a microcontroller."
            .to_string(),
        "The SocketCAN backend uses real AF_CAN sockets but is exercised only against a virtual \
         vcan0 interface on a GitHub Actions ubuntu runner. A virtual interface is a kernel \
         loopback, not a bus."
            .to_string(),
        "Jitter percentiles are reported as the upper edge of a 10 us histogram bucket. They are \
         bounds, not exact samples; min and max are exact."
            .to_string(),
    ]
}
