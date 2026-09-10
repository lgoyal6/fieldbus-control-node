//! Regression tests for the deadline accounting.
//!
//! These exist because of a specific defect the `cpu-hog` negative control
//! exposed. In the first full gate run the control reported 170 missed
//! deadlines and was recorded as caught, which looked like the gate working.
//! Grouping those misses by cycle showed every one of them at cycles 7000
//! through 7016 and none at all inside the contended window, cycles 3000
//! through 6999. Two separate bugs produced that:
//!
//! 1. The control joined its 22 spinning threads from inside the timed loop
//!    at the end of its window. The join blocked the loop for 173 ms, which
//!    the runner charged to wake jitter. The control was firing on its own
//!    teardown rather than on contention.
//! 2. Skipped periods were counted as `jitter / period` on every late cycle,
//!    so the same wall-clock periods were counted again by each cycle still
//!    catching up. One 173 ms stall was reported as 153 skipped periods when
//!    only 17 periods had actually gone by with no cycle in them.
//!
//! The second bug is the one that is unit-testable, and it is the one that
//! would have misreported any real stall, not just this control's.

use fieldbus_host::runner::{Mode, PeriodLedger, RunConfig, ThreadPolicy};

const PERIOD: u64 = 10_000;
/// The frozen staleness budget, eight periods. The runner takes it as a
/// parameter because `manifest/frozen.json` is where it is set.
const WATCHDOG_US: u64 = 80_000;

#[test]
fn an_on_time_loop_never_reports_a_skipped_period() {
    let mut ledger = PeriodLedger::new(PERIOD);
    let mut cycle = 0u64;
    while cycle < 1_000 {
        // A little wake jitter, well inside the period.
        let start = cycle * PERIOD + (cycle % 7) * 10;
        assert_eq!(
            ledger.account_cycle_start(start),
            0,
            "cycle {cycle} was on time and must not report a skip"
        );
        cycle += 1;
    }
}

#[test]
fn a_stall_counts_each_lost_period_exactly_once() {
    let mut ledger = PeriodLedger::new(PERIOD);
    // Cycles 0 through 6999 run on time.
    let mut cycle = 0u64;
    while cycle < 7_000 {
        assert_eq!(ledger.account_cycle_start(cycle * PERIOD), 0);
        cycle += 1;
    }

    // Cycle 7000 is 172_955 us late, so it actually starts inside period
    // 7017. Periods 7000 through 7016 went by with no cycle in them: 17.
    let stalled_start = 7_000 * PERIOD + 172_955;
    assert_eq!(stalled_start / PERIOD, 7_017);
    assert_eq!(
        ledger.account_cycle_start(stalled_start),
        17,
        "a 173 ms stall loses 17 whole periods, not 153"
    );

    // Cycles 7001 through 7017 now run back to back inside period 7017,
    // catching up. They are late against their own deadlines, but they lose
    // no further periods and must not re-count the ones already counted.
    let mut catch_up = 7_001u64;
    let mut extra = 0u64;
    while catch_up <= 7_017 {
        extra += ledger.account_cycle_start(stalled_start + (catch_up - 7_000));
        catch_up += 1;
    }
    assert_eq!(extra, 0, "catch-up cycles re-counted periods already lost");

    // And once the loop is back on schedule, accounting resumes normally.
    assert_eq!(ledger.account_cycle_start(7_018 * PERIOD), 0);
    assert_eq!(ledger.account_cycle_start(7_019 * PERIOD), 0);
}

#[test]
fn one_lost_period_is_counted_once() {
    let mut ledger = PeriodLedger::new(PERIOD);
    ledger.account_cycle_start(0);
    // Cycle 1 starts inside period 2, so period 1 was lost.
    assert_eq!(ledger.account_cycle_start(2 * PERIOD + 5), 1);
    // Cycle 2 also starts inside period 2. Nothing further is lost.
    assert_eq!(ledger.account_cycle_start(2 * PERIOD + 60), 0);
    assert_eq!(ledger.account_cycle_start(3 * PERIOD), 0);
}

#[test]
fn a_degenerate_period_does_not_divide_by_zero() {
    // The runner should never be handed a zero period, but a ledger that
    // panics is a worse failure than one that treats it as the smallest
    // representable period.
    let mut ledger = PeriodLedger::new(0);
    assert_eq!(ledger.account_cycle_start(0), 0);
    assert_eq!(ledger.account_cycle_start(5), 4);
}

#[test]
fn virtual_time_runs_are_exactly_on_schedule_and_reproducible() {
    use fieldbus_host::bus::{FaultSchedule, SimBus};
    use fieldbus_host::runner;

    let cfg = RunConfig {
        cycles: 500,
        period_us: PERIOD,
        watchdog_timeout_us: WATCHDOG_US,
        control_thread_policy: ThreadPolicy::TimeConstraint,
        mode: Mode::Virtual,
    };
    let mut bus_a = SimBus::new(20260909, FaultSchedule::clean());
    let a = runner::run(&mut bus_a, cfg, None);
    let mut bus_b = SimBus::new(20260909, FaultSchedule::clean());
    let b = runner::run(&mut bus_b, cfg, None);

    assert_eq!(a.jitter.missed_deadlines(), 0);
    assert_eq!(a.jitter.max_us(), 0, "virtual time cannot jitter");
    assert_eq!(a.accepted, 500);
    assert_eq!(a.rejected, 0);
    assert!(a.safe_state.is_none());

    // Same seed, same schedule, same everything. If this ever diverges, no
    // number produced by the simulator means anything.
    assert_eq!(a.accepted, b.accepted);
    assert_eq!(a.rejected, b.rejected);
    assert_eq!(a.rejected_by_reason, b.rejected_by_reason);
    assert_eq!(a.jitter.missed_deadlines(), b.jitter.missed_deadlines());
    assert_eq!(a.last_actuator, b.last_actuator);
}

#[test]
fn the_can_corrupt_schedule_produces_one_rejection_per_injected_frame() {
    use fieldbus_core::j1939::Reject;
    use fieldbus_host::bus::{FaultSchedule, SimBus};
    use fieldbus_host::runner;

    // The claim the negative control rests on: an injected fault yields
    // exactly one rejection under exactly one reason, and never cascades
    // into a second rejection on the next good frame.
    let mut faults = FaultSchedule::clean();
    faults.bad_crc = [100, 110].into_iter().collect();
    faults.duplicate = [120].into_iter().collect();
    faults.out_of_order = [130].into_iter().collect();
    faults.out_of_range = [140].into_iter().collect();
    faults.bad_length = [150].into_iter().collect();

    let mut bus = SimBus::new(20260909, faults);
    let out = runner::run(
        &mut bus,
        RunConfig {
            cycles: 300,
            period_us: PERIOD,
            watchdog_timeout_us: WATCHDOG_US,
            control_thread_policy: ThreadPolicy::TimeConstraint,
            mode: Mode::Virtual,
        },
        None,
    );

    assert_eq!(out.accepted, 300, "every legitimate frame must still land");
    assert_eq!(out.rejected, 6);
    let by = out.rejected_by_reason;
    assert_eq!(by[Reject::BadCrc.index()], 2);
    assert_eq!(by[Reject::Duplicate.index()], 1);
    assert_eq!(
        by[Reject::OutOfOrder {
            expected: 0,
            got: 0
        }
        .index()],
        1
    );
    assert_eq!(by[Reject::OutOfRange.index()], 1);
    assert_eq!(by[Reject::BadLength.index()], 1);
}

#[test]
fn a_dropped_cycle_models_a_silent_sensor_and_produces_no_rejection() {
    use fieldbus_core::j1939::Reject;
    use fieldbus_host::bus::{FaultSchedule, SimBus};
    use fieldbus_host::runner;

    // What Drop models here is precise and worth stating: the simulated
    // sensor does not transmit this period. Its sequence counter therefore
    // does not advance, so the next frame is still in sequence and nothing
    // is rejected. The node simply goes one period without a fresh reading.
    //
    // What it does NOT model is a wire-level loss, where the sender did
    // transmit and its counter did advance, leaving a real gap. See the
    // README limitation on sequence resynchronisation for why that case is
    // documented rather than simulated.
    let mut faults = FaultSchedule::clean();
    faults.dropped = [100, 101, 102].into_iter().collect();

    let mut bus = SimBus::new(20260909, faults);
    let out = runner::run(
        &mut bus,
        RunConfig {
            cycles: 300,
            period_us: PERIOD,
            watchdog_timeout_us: WATCHDOG_US,
            control_thread_policy: ThreadPolicy::TimeConstraint,
            mode: Mode::Virtual,
        },
        None,
    );

    assert_eq!(out.accepted, 297, "three periods produced no frame at all");
    assert_eq!(out.rejected, 0, "a silent period is not a malformed frame");
    assert_eq!(
        out.rejected_by_reason[Reject::OutOfOrder {
            expected: 0,
            got: 0
        }
        .index()],
        0
    );
    // Three lost periods is 30 ms, far inside the 80 ms staleness budget,
    // so the watchdog must not trip on them.
    assert!(
        out.safe_state.is_none(),
        "three silent periods are not staleness at an 80 ms budget"
    );
}

#[test]
fn enough_consecutive_silent_periods_do_trip_the_watchdog() {
    use fieldbus_host::bus::{FaultSchedule, SimBus};
    use fieldbus_host::runner;

    // The complement of the test above: silence is tolerated up to the
    // budget and not past it. Nine consecutive silent periods is 90 ms
    // against an 80 ms budget.
    let mut faults = FaultSchedule::clean();
    faults.dropped = (100..=115).collect();

    let mut bus = SimBus::new(20260909, faults);
    let out = runner::run(
        &mut bus,
        RunConfig {
            cycles: 300,
            period_us: PERIOD,
            watchdog_timeout_us: WATCHDOG_US,
            control_thread_policy: ThreadPolicy::TimeConstraint,
            mode: Mode::Virtual,
        },
        None,
    );

    assert_eq!(out.rejected, 0);
    let state = out
        .safe_state
        .expect("16 silent periods must trip an 80 ms watchdog");
    assert_eq!(state.reason_name(), "sensor_stale");
    // And it latches: the sensor comes back at cycle 116 and the node stays
    // in its safe state regardless.
    assert!(out.actuator_zero_since_trip);
    assert_eq!(out.last_actuator, 0.0);
}

#[test]
fn a_thread_policy_name_round_trips_and_an_unknown_one_is_refused() {
    // The manifest names both sides' scheduling policies, so an unrecognised
    // name has to stop the run. Falling back to a default would produce a
    // result file labelled with an experiment the binary did not perform.
    assert_eq!(
        ThreadPolicy::from_manifest("mach-time-constraint"),
        Ok(ThreadPolicy::TimeConstraint)
    );
    assert_eq!(
        ThreadPolicy::from_manifest("default-timeshare"),
        Ok(ThreadPolicy::DefaultTimeshare)
    );
    assert_eq!(
        ThreadPolicy::TimeConstraint.as_str(),
        "mach-time-constraint"
    );
    assert_eq!(ThreadPolicy::DefaultTimeshare.as_str(), "default-timeshare");
    assert!(ThreadPolicy::from_manifest("sched-deadline").is_err());
    assert!(ThreadPolicy::from_manifest("").is_err());
}

#[test]
fn the_hog_window_split_accounts_for_every_cycle_exactly_once() {
    use fieldbus_host::bus::{FaultSchedule, SimBus};
    use fieldbus_host::runner::{self, HogWindow};

    // The attribution bookkeeping, checked where the clock is exact. Virtual
    // time cannot jitter and cannot miss, so what this asserts is only that
    // every cycle is counted on exactly one side of the window boundary, which
    // is the thing that would silently ruin an attribution claim if it were
    // off by one.
    let mut bus = SimBus::new(20260909, FaultSchedule::clean());
    let out = runner::run(
        &mut bus,
        RunConfig {
            cycles: 100,
            period_us: PERIOD,
            watchdog_timeout_us: WATCHDOG_US,
            control_thread_policy: ThreadPolicy::DefaultTimeshare,
            mode: Mode::Virtual,
        },
        Some(HogWindow {
            start_cycle: 30,
            end_cycle: 70,
            threads: 2,
            policy: ThreadPolicy::TimeConstraint,
        }),
    );

    let hog = out.hog.expect("a hog run must report its hog");
    assert_eq!(hog.threads, 2);
    assert_eq!(hog.start_cycle, 30);
    assert_eq!(hog.end_cycle, 70);
    assert_eq!(hog.jitter_inside_window.count(), 40);
    assert_eq!(hog.jitter_outside_window.count(), 60);
    assert_eq!(
        hog.jitter_inside_window.count() + hog.jitter_outside_window.count(),
        out.jitter.count(),
        "every cycle must be counted on exactly one side of the window"
    );
    assert_eq!(
        hog.misses_inside_window + hog.misses_outside_window,
        out.jitter.missed_deadlines()
    );
    assert_eq!(out.jitter.missed_deadlines(), 0, "virtual time cannot miss");

    // The two sides of the priority relation the control rests on are recorded
    // rather than assumed, so a reader can check it held.
    assert_eq!(out.control_thread_policy, ThreadPolicy::DefaultTimeshare);
    assert_eq!(hog.policy, ThreadPolicy::TimeConstraint);
}
