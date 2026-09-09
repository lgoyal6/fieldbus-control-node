//! Unit tests for `fieldbus-core`.
//!
//! The crate is `#![no_std]`, but these tests link against `std` because they
//! run on the host. Nothing here is compiled into the `thumbv7em-none-eabihf`
//! build, which is why the cross-compile check in the completion gate is a
//! compile check and not a test run: this crate has never executed on a
//! microcontroller.

use fieldbus_core::control::{LowPass, Pi, PERIOD_S};
use fieldbus_core::j1939::{
    command_frame, crc8_j1850, sensor_frame, Frame, Id, Payload, Reject, Validator, PGN_COMMAND,
    PGN_SENSOR, PRIORITY, SA_CONTROLLER, SA_SENSOR, STATUS_OK, VALUE_MAX, VALUE_MIN,
};
use fieldbus_core::node::{ControlNode, SETPOINT, WATCHDOG_TIMEOUT_US};
use fieldbus_core::telemetry::{JitterStats, BUCKET_WIDTH_US, NUM_BUCKETS, OVERFLOW_FLOOR_US};
use fieldbus_core::watchdog::{SafeReason, Watchdog};

// ---------------------------------------------------------------- identifiers

#[test]
fn id_pack_unpack_round_trips_over_every_field() {
    // Walk each field across its full width rather than spot-checking one
    // value: a mask error in `pack` shows up only when a neighbouring field is
    // non-zero at the same time.
    for priority in 0..8u8 {
        for dp in 0..2u8 {
            for &pf in &[0x00u8, 0x01, 0x7F, 0xEF, 0xFF] {
                for &ps in &[0x00u8, 0x01, 0x80, 0xFE, 0xFF] {
                    for &sa in &[0x00u8, 0x28, 0x80, 0x81, 0xFF] {
                        let id = Id {
                            priority,
                            reserved: 0,
                            data_page: dp,
                            pdu_format: pf,
                            pdu_specific: ps,
                            source_address: sa,
                        };
                        let raw = id.pack();
                        assert!(raw < (1 << 29), "identifier exceeded 29 bits: {raw:#x}");
                        assert_eq!(Id::unpack(raw), id);
                    }
                }
            }
        }
    }
}

#[test]
fn from_pgn_places_the_two_defined_pgns_where_the_layout_says() {
    let s = Id::from_pgn(PRIORITY, PGN_SENSOR, SA_SENSOR);
    assert_eq!(s.pgn(), PGN_SENSOR);
    assert_eq!(
        s.pdu_format, 0xFF,
        "proprietary B, so the frame is broadcast"
    );
    assert_eq!(s.pdu_specific, 0x00);
    assert_eq!(s.source_address, SA_SENSOR);
    assert_eq!(s.data_page, 0);
    assert_eq!(s.reserved, 0);

    let c = Id::from_pgn(PRIORITY, PGN_COMMAND, SA_CONTROLLER);
    assert_eq!(c.pgn(), PGN_COMMAND);
    assert_eq!(c.pdu_specific, 0x01);
    assert_eq!(c.source_address, SA_CONTROLLER);

    // The two streams must be distinguishable by identifier alone.
    assert_ne!(s.pack(), c.pack());
}

#[test]
fn packing_masks_oversized_fields_instead_of_corrupting_neighbours() {
    let id = Id {
        priority: 0xFF,
        reserved: 0xFF,
        data_page: 0xFF,
        pdu_format: 0xFF,
        pdu_specific: 0xFF,
        source_address: 0xFF,
    };
    assert_eq!(
        id.pack(),
        0x1FFF_FFFF,
        "all 29 bits set, nothing above them"
    );
}

// -------------------------------------------------------------------- payload

#[test]
fn payload_codec_round_trips_across_the_value_range() {
    for &value in &[
        VALUE_MIN,
        VALUE_MIN + 1,
        -1,
        0,
        1,
        499,
        500,
        VALUE_MAX - 1,
        VALUE_MAX,
        i16::MIN,
        i16::MAX,
    ] {
        for &seq in &[0u8, 1, 127, 254, 255] {
            let p = Payload {
                seq,
                value,
                status: STATUS_OK,
            };
            let d = p.encode();
            assert_eq!(Payload::decode(&d), p);
            assert_eq!(d[4], 0, "reserved byte 4 must be zero");
            assert_eq!(d[5], 0, "reserved byte 5 must be zero");
            assert_eq!(d[6], 0, "reserved byte 6 must be zero");
            assert_eq!(d[7], crc8_j1850(&d[0..7]));
        }
    }
}

#[test]
fn crc8_j1850_detects_every_single_bit_flip_in_the_covered_bytes() {
    let base = Payload {
        seq: 42,
        value: -1234,
        status: STATUS_OK,
    }
    .encode();
    let good = crc8_j1850(&base[0..7]);
    for byte in 0..7usize {
        for bit in 0..8u32 {
            let mut d = base;
            d[byte] ^= 1 << bit;
            assert_ne!(
                crc8_j1850(&d[0..7]),
                good,
                "single-bit flip at byte {byte} bit {bit} went undetected"
            );
        }
    }
}

// ------------------------------------------------------------------ validator

fn valid_sensor(seq: u8, value: i16) -> Frame {
    sensor_frame(seq, value, STATUS_OK)
}

#[test]
fn first_frame_after_reset_is_accepted_at_any_seq() {
    let mut v = Validator::new();
    // A node that boots mid-stream cannot know the sender's counter, so it
    // synchronises to whatever arrives first.
    assert!(v.validate(&valid_sensor(200, 10)).is_ok());
    assert!(v.validate(&valid_sensor(201, 10)).is_ok());
    v.reset_sequence();
    assert!(v.validate(&valid_sensor(7, 10)).is_ok());
    assert_eq!(v.accepted(), 3);
    assert_eq!(v.rejected(), 0);
}

#[test]
fn reject_bad_length() {
    let mut v = Validator::new();
    let mut f = valid_sensor(0, 100);
    f.dlc = 4;
    assert_eq!(v.validate(&f), Err(Reject::BadLength));
    assert_eq!(v.by_reason()[Reject::BadLength.index()], 1);
}

#[test]
fn reject_unknown_pgn() {
    let mut v = Validator::new();
    let mut f = valid_sensor(0, 100);
    // A command frame on the sensor validator: right bus, wrong stream. This
    // is what a node hearing its own transmissions looks like.
    f.id = Id::from_pgn(PRIORITY, PGN_COMMAND, SA_SENSOR).pack();
    assert_eq!(v.validate(&f), Err(Reject::UnknownPgn));
    assert_eq!(v.by_reason()[Reject::UnknownPgn.index()], 1);
}

#[test]
fn reject_unexpected_source() {
    let mut v = Validator::new();
    let mut f = valid_sensor(0, 100);
    f.id = Id::from_pgn(PRIORITY, PGN_SENSOR, 0x42).pack();
    assert_eq!(v.validate(&f), Err(Reject::UnexpectedSource));
    assert_eq!(v.by_reason()[Reject::UnexpectedSource.index()], 1);
}

#[test]
fn reject_bad_crc() {
    let mut v = Validator::new();
    let mut f = valid_sensor(0, 100);
    f.data[7] ^= 0xFF;
    assert_eq!(v.validate(&f), Err(Reject::BadCrc));
    assert_eq!(v.by_reason()[Reject::BadCrc.index()], 1);
}

#[test]
fn bad_crc_is_checked_before_any_content_check() {
    let mut v = Validator::new();
    assert!(v.validate(&valid_sensor(0, 100)).is_ok());
    // This frame is simultaneously a duplicate, out of range, and has a bad
    // CRC. Bytes that fail their checksum cannot be trusted to say what they
    // are, so the CRC reason must win.
    let mut f = valid_sensor(0, i16::MAX);
    f.data[6] = 0x55;
    assert_eq!(v.validate(&f), Err(Reject::BadCrc));
}

#[test]
fn reject_bad_reserved() {
    let mut v = Validator::new();
    let mut f = valid_sensor(0, 100);
    f.data[5] = 0x01;
    f.data[7] = crc8_j1850(&f.data[0..7]); // recompute so the CRC passes
    assert_eq!(v.validate(&f), Err(Reject::BadReserved));
    assert_eq!(v.by_reason()[Reject::BadReserved.index()], 1);
}

#[test]
fn reject_duplicate() {
    let mut v = Validator::new();
    assert!(v.validate(&valid_sensor(9, 100)).is_ok());
    assert_eq!(v.validate(&valid_sensor(9, 100)), Err(Reject::Duplicate));
    assert_eq!(v.by_reason()[Reject::Duplicate.index()], 1);
    // A rejected frame must not move the sequence position, or one fault
    // would cascade into a second rejection on the next good frame.
    assert!(v.validate(&valid_sensor(10, 100)).is_ok());
}

#[test]
fn reject_out_of_order_reports_expected_and_got() {
    let mut v = Validator::new();
    assert!(v.validate(&valid_sensor(9, 100)).is_ok());
    assert_eq!(
        v.validate(&valid_sensor(14, 100)),
        Err(Reject::OutOfOrder {
            expected: 10,
            got: 14
        })
    );
    // Backwards is out of order too, not a duplicate.
    assert_eq!(
        v.validate(&valid_sensor(3, 100)),
        Err(Reject::OutOfOrder {
            expected: 10,
            got: 3
        })
    );
    assert_eq!(
        v.by_reason()[Reject::OutOfOrder {
            expected: 0,
            got: 0
        }
        .index()],
        2
    );
}

#[test]
fn sequence_numbers_wrap_at_255_without_a_rejection() {
    let mut v = Validator::new();
    assert!(v.validate(&valid_sensor(254, 100)).is_ok());
    assert!(v.validate(&valid_sensor(255, 100)).is_ok());
    assert!(v.validate(&valid_sensor(0, 100)).is_ok());
    assert!(v.validate(&valid_sensor(1, 100)).is_ok());
    assert_eq!(v.rejected(), 0);
}

#[test]
fn reject_out_of_range_at_both_ends() {
    let mut v = Validator::new();
    assert!(v.validate(&valid_sensor(0, VALUE_MAX)).is_ok());
    assert_eq!(
        v.validate(&valid_sensor(1, VALUE_MAX + 1)),
        Err(Reject::OutOfRange)
    );
    v.reset_sequence();
    assert!(v.validate(&valid_sensor(0, VALUE_MIN)).is_ok());
    assert_eq!(
        v.validate(&valid_sensor(1, VALUE_MIN - 1)),
        Err(Reject::OutOfRange)
    );
    assert_eq!(v.by_reason()[Reject::OutOfRange.index()], 2);
}

#[test]
fn reject_reason_indices_and_names_are_distinct_and_stable() {
    let all = [
        Reject::BadLength,
        Reject::UnknownPgn,
        Reject::UnexpectedSource,
        Reject::BadCrc,
        Reject::BadReserved,
        Reject::Duplicate,
        Reject::OutOfOrder {
            expected: 0,
            got: 0,
        },
        Reject::OutOfRange,
    ];
    for (i, r) in all.iter().enumerate() {
        assert_eq!(
            r.index(),
            i,
            "index of {r:?} moved; results/ keys are stale"
        );
    }
    assert_eq!(Reject::name(0), "bad_length");
    assert_eq!(Reject::name(7), "out_of_range");
}

// -------------------------------------------------------------------- control

#[test]
fn low_pass_adopts_its_first_sample_then_converges_monotonically() {
    let mut f = LowPass::new(0.30);
    assert_eq!(f.update(100.0), 100.0, "first sample is adopted outright");
    let mut prev = 100.0f32;
    for i in 0..200 {
        let y = f.update(200.0);
        // Non-decreasing, not strictly increasing: in f32 the step eventually
        // rounds to nothing and the response parks one ulp short of the
        // input. That is convergence, not a stall, and an epsilon smaller
        // than the f32 spacing at 200 (about 1.5e-5) cannot express it.
        assert!(y >= prev, "step {i} went backwards: {prev} then {y}");
        assert!(y <= 200.0, "step {i} overshot the input: {y}");
        prev = y;
    }
    assert!((prev - 200.0).abs() < 0.01, "did not converge: {prev}");
}

#[test]
fn low_pass_at_alpha_one_is_a_pass_through() {
    let mut f = LowPass::new(1.0);
    f.update(0.0);
    assert_eq!(f.update(123.0), 123.0);
    assert_eq!(f.update(-45.0), -45.0);
}

#[test]
fn pi_output_never_leaves_its_limits() {
    let mut pi = Pi::new(10.0, 100.0, -50.0, 50.0);
    for _ in 0..10_000 {
        let out = pi.update(1_000.0, 0.0);
        assert!(
            (-50.0..=50.0).contains(&out),
            "output escaped its limits: {out}"
        );
    }
}

#[test]
fn pi_anti_windup_stops_the_integral_growing_while_saturated() {
    // Pure integral action, so the integral is the only thing that can wind
    // up and the test cannot pass vacuously by never accumulating at all.
    let mut pi = Pi::new(0.0, 10.0, -20.0, 20.0);
    for _ in 0..500 {
        pi.update(100.0, 0.0);
    }
    let wound = pi.integral();
    assert!(
        wound > 0.0,
        "the integral never accumulated, so this test proves nothing"
    );
    // With ki = 10 and a 20-unit limit, the integral has authority up to 2.0
    // error-seconds and must stop exactly there.
    assert!(
        (wound - 2.0).abs() < 1e-4,
        "integral froze at {wound}, expected 2.0"
    );

    for _ in 0..500 {
        pi.update(100.0, 0.0);
    }
    assert_eq!(
        pi.integral(),
        wound,
        "integral kept accumulating while the output had no authority"
    );
    assert_eq!(pi.update(100.0, 0.0), 20.0, "still pinned to the limit");

    // The payoff: with the integral frozen, reversing the error sign brings
    // the output off the limit on the very next period. A wound-up controller
    // would stay pinned for as long as it took to unwind.
    let out = pi.update(0.0, 100.0);
    assert!(
        out < 20.0,
        "controller stayed pinned at {out} after the error reversed"
    );
}

#[test]
fn pi_integral_accumulates_at_the_fixed_period() {
    let mut pi = Pi::new(0.0, 1.0, -1_000.0, 1_000.0);
    pi.update(10.0, 0.0);
    assert!(
        (pi.integral() - 10.0 * PERIOD_S).abs() < 1e-6,
        "integral step was not error * PERIOD_S: {}",
        pi.integral()
    );
}

#[test]
fn pi_reaches_its_setpoint_on_a_first_order_plant() {
    let mut pi = Pi::new(0.60, 4.00, -1_000.0, 1_000.0);
    let mut plant = 0.0f32;
    for _ in 0..4_000 {
        let u = pi.update(500.0, plant);
        plant += 0.05 * (u - plant);
    }
    assert!(
        (plant - 500.0).abs() < 1.0,
        "closed loop settled at {plant}, not 500"
    );
}

#[test]
fn pi_new_orders_swapped_limits_instead_of_panicking_in_clamp() {
    let mut pi = Pi::new(1.0, 0.0, 50.0, -50.0);
    let out = pi.update(1_000.0, 0.0);
    assert!((-50.0..=50.0).contains(&out));
}

// ------------------------------------------------------------------- watchdog

#[test]
fn watchdog_is_armed_at_construction_so_a_sensor_that_never_starts_trips() {
    let mut w = Watchdog::new(WATCHDOG_TIMEOUT_US, 0);
    assert!(
        w.check(WATCHDOG_TIMEOUT_US).is_none(),
        "must not trip at the boundary"
    );
    let s = w
        .check(WATCHDOG_TIMEOUT_US + 1)
        .expect("should have tripped");
    match s.reason {
        SafeReason::SensorStale {
            last_seen_us,
            tripped_at_us,
            timeout_us,
        } => {
            assert_eq!(last_seen_us, 0);
            assert_eq!(tripped_at_us, WATCHDOG_TIMEOUT_US + 1);
            assert_eq!(timeout_us, WATCHDOG_TIMEOUT_US);
        }
    }
    assert_eq!(s.reaction_time_us(), 1);
    assert_eq!(s.reason_name(), "sensor_stale");
}

#[test]
fn watchdog_does_not_trip_while_it_is_fed() {
    let mut w = Watchdog::new(WATCHDOG_TIMEOUT_US, 0);
    let mut t = 0u64;
    for _ in 0..1_000 {
        t += 10_000;
        w.feed(t);
        assert!(w.check(t).is_none());
    }
}

#[test]
fn watchdog_latches_and_keeps_the_original_trip_timestamps() {
    let mut w = Watchdog::new(100_000, 0);
    w.feed(50_000);
    assert!(w.check(150_000).is_none(), "boundary is not yet stale");
    let first = w.check(151_000).expect("should trip");
    assert_eq!(first.reaction_time_us(), 1_000);

    // Later checks, and even a sensor that comes back, must not change or
    // clear the latch.
    let later = w.check(900_000).expect("still latched");
    assert_eq!(later, first, "latched state was recomputed");
    w.feed(1_000_000);
    let after_feed = w
        .check(1_000_000)
        .expect("a working sensor must not clear a latch");
    assert_eq!(after_feed, first);
}

#[test]
fn watchdog_reset_is_the_only_way_out() {
    let mut w = Watchdog::new(100_000, 0);
    assert!(w.check(200_000).is_some());
    w.reset(200_000);
    assert!(w.latched().is_none());
    assert!(w.check(250_000).is_none(), "reset must re-arm from now");
    assert!(
        w.check(300_001).is_some(),
        "and the budget must apply again"
    );
}

#[test]
fn watchdog_ignores_a_feed_that_goes_backwards() {
    let mut w = Watchdog::new(100_000, 0);
    w.feed(500_000);
    w.feed(10_000);
    assert_eq!(w.last_seen_us(), 500_000);
    assert!(w.check(590_000).is_none());
}

// ------------------------------------------------------------------ telemetry

fn reference_percentile(samples: &mut [u64], permille: u32) -> u64 {
    samples.sort_unstable();
    let n = samples.len() as u64;
    let scaled = n * permille as u64;
    let mut rank = scaled / 1000;
    if !scaled.is_multiple_of(1000) {
        rank += 1;
    }
    if rank == 0 {
        rank = 1;
    }
    samples[(rank - 1) as usize]
}

#[test]
fn histogram_percentiles_track_a_sorted_slice_within_one_bucket_width() {
    // A deterministic spread with a deliberate tail, so p99 is not just p50
    // with a different name. Linear congruential generator inline: the point
    // is a reproducible sample set, not randomness.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut samples: Vec<u64> = Vec::new();
    let mut hist = JitterStats::new();
    for i in 0..20_000u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let base = (state >> 33) % 400; // bulk: 0..399 us
        let v = if i % 137 == 0 { base + 2_500 } else { base };
        samples.push(v);
        hist.record(v);
    }
    for &permille in &[500u32, 950, 990] {
        let want = reference_percentile(&mut samples, permille);
        let got = hist.percentile_permille(permille);
        assert!(
            got >= want && got - want <= BUCKET_WIDTH_US,
            "p{permille} permille: histogram said {got}, sorted slice said {want}"
        );
    }
    assert_eq!(hist.count(), 20_000);
    assert_eq!(hist.min_us(), *samples.iter().min().unwrap());
    assert_eq!(hist.max_us(), *samples.iter().max().unwrap());
    assert_eq!(hist.sum_us(), samples.iter().sum::<u64>());
    assert!(!hist.overflowed());
}

#[test]
fn histogram_reports_the_upper_edge_of_the_containing_bucket() {
    let mut h = JitterStats::new();
    h.record(0);
    assert_eq!(h.p50_us(), BUCKET_WIDTH_US, "0 lands in bucket 0, edge 10");
    let mut h = JitterStats::new();
    h.record(BUCKET_WIDTH_US * 7 + 3);
    assert_eq!(h.p50_us(), BUCKET_WIDTH_US * 8);
}

#[test]
fn histogram_overflow_bucket_bounds_the_percentile_but_max_stays_exact() {
    let mut h = JitterStats::new();
    for _ in 0..100 {
        h.record(OVERFLOW_FLOOR_US + 12_345);
    }
    assert!(h.overflowed());
    assert_eq!(
        h.p99_us(),
        OVERFLOW_FLOOR_US,
        "a lower bound, as documented"
    );
    assert_eq!(
        h.max_us(),
        OVERFLOW_FLOOR_US + 12_345,
        "max is not bucketed"
    );
}

#[test]
fn empty_histogram_reports_zeroes_rather_than_a_sentinel() {
    let h = JitterStats::new();
    assert_eq!(h.count(), 0);
    assert_eq!(h.min_us(), 0);
    assert_eq!(h.max_us(), 0);
    assert_eq!(h.mean_us(), 0);
    assert_eq!(h.p99_us(), 0);
    assert_eq!(h.missed_deadlines(), 0);
}

#[test]
fn histogram_boundary_sample_lands_in_the_last_exact_bucket() {
    let mut h = JitterStats::new();
    h.record(OVERFLOW_FLOOR_US - 1);
    assert!(!h.overflowed());
    assert_eq!(h.p50_us(), NUM_BUCKETS as u64 * BUCKET_WIDTH_US);
    let mut h = JitterStats::new();
    h.record(OVERFLOW_FLOOR_US);
    assert!(
        h.overflowed(),
        "the floor itself belongs to the overflow bucket"
    );
}

#[test]
fn missed_deadlines_are_counted_apart_from_jitter() {
    let mut h = JitterStats::new();
    h.record(5);
    h.record_miss();
    h.record_miss();
    assert_eq!(h.count(), 1);
    assert_eq!(h.missed_deadlines(), 2);
}

// ----------------------------------------------------------------------- node

/// One deterministic scripted run: a first-order plant driven by the node's
/// own command, with the sensor stream fed back in.
fn scripted_run(cycles: u64, freeze_at: Option<u64>) -> Vec<fieldbus_core::StepOutput> {
    let mut node = ControlNode::new(0);
    let mut plant = 0.0f32;
    let mut seq = 0u8;
    let mut out = Vec::new();
    for i in 0..cycles {
        let now = i * 10_000;
        let frozen = freeze_at.map(|f| i >= f).unwrap_or(false);
        let frames: Vec<Frame> = if frozen {
            Vec::new()
        } else {
            vec![sensor_frame(seq, plant as i16, STATUS_OK)]
        };
        if !frozen {
            seq = seq.wrapping_add(1);
        }
        let step = node.step(now, &frames);
        plant += 0.05 * (step.actuator - plant);
        out.push(step);
    }
    out
}

#[test]
fn node_is_deterministic_across_identical_input_sequences() {
    let a = scripted_run(2_000, None);
    let b = scripted_run(2_000, None);
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x, y, "outputs diverged at cycle {i}");
    }
}

#[test]
fn node_closes_the_loop_on_the_setpoint_with_nothing_rejected() {
    let out = scripted_run(3_000, None);
    let last = out.last().unwrap();
    assert_eq!(last.rejected, 0, "a clean stream must reject nothing");
    assert_eq!(last.accepted, 3_000);
    assert!(last.safe_state.is_none());
    assert!(
        (last.actuator - 500.0).abs() < 5.0,
        "actuator settled at {}, not near the setpoint",
        last.actuator
    );
    // Every period emits a command, including the first.
    assert!(out.iter().all(|s| s.command_frame.is_some()));
}

#[test]
fn node_emits_a_command_frame_this_controller_can_be_identified_by() {
    let out = scripted_run(3, None);
    let f = out[0].command_frame.unwrap();
    let id = Id::unpack(f.id);
    assert_eq!(id.pgn(), PGN_COMMAND);
    assert_eq!(id.source_address, SA_CONTROLLER);
    assert_eq!(f.dlc, 8);
    assert_eq!(f.data[7], crc8_j1850(&f.data[0..7]));
    // Command sequence numbers advance once per period.
    assert_eq!(Payload::decode(&out[0].command_frame.unwrap().data).seq, 0);
    assert_eq!(Payload::decode(&out[1].command_frame.unwrap().data).seq, 1);
}

#[test]
fn node_latches_safe_and_commands_zero_when_the_sensor_freezes() {
    let freeze_at = 1_000u64;
    let out = scripted_run(1_100, Some(freeze_at));

    // Last accepted frame was at cycle 999, timestamp 9_990_000 us. With a
    // 100_000 us budget the deadline is 10_090_000 us, so the first check
    // strictly past it is cycle 1010.
    let trip_index = out
        .iter()
        .position(|s| s.safe_state.is_some())
        .expect("watchdog never tripped");
    assert_eq!(trip_index, 1_010, "tripped at the wrong period");

    let state = out[trip_index].safe_state.unwrap();
    assert!(
        state.reaction_time_us() <= 10_000,
        "reaction time {} exceeded one period",
        state.reaction_time_us()
    );
    assert_eq!(state.reason_name(), "sensor_stale");

    // Latched to the end, commanding the safe value, with the safe status.
    for s in &out[trip_index..] {
        assert_eq!(s.safe_state, Some(state), "latch was not stable");
        assert_eq!(s.actuator, 0.0);
        let d = s.command_frame.unwrap().data;
        assert_eq!(Payload::decode(&d).value, 0);
        assert_eq!(
            Payload::decode(&d).status,
            fieldbus_core::j1939::STATUS_SAFE
        );
    }
    // And nothing was rejected: silence is not a malformed frame.
    assert_eq!(out.last().unwrap().rejected, 0);
}

#[test]
fn node_reset_clears_the_latch_and_the_control_state() {
    let mut node = ControlNode::new(0);
    let s = node.step(200_000, &[]);
    assert!(s.safe_state.is_some(), "no frames for 200 ms must trip");
    node.reset(200_000);
    assert!(node.safe_state().is_none());
    let s = node.step(210_000, &[sensor_frame(0, 500, STATUS_OK)]);
    assert!(s.safe_state.is_none());
    assert_eq!(s.accepted, 1);
}

#[test]
fn node_counts_rejections_per_reason_without_letting_them_reach_control() {
    let mut node = ControlNode::new(0);
    // Prime the sequence.
    node.step(0, &[sensor_frame(0, 500, STATUS_OK)]);

    let mut bad_crc = sensor_frame(1, 500, STATUS_OK);
    bad_crc.data[7] ^= 0xFF;
    let mut bad_len = sensor_frame(1, 500, STATUS_OK);
    bad_len.dlc = 2;
    let out_of_range = sensor_frame(1, VALUE_MAX + 1, STATUS_OK);
    let duplicate = sensor_frame(0, 500, STATUS_OK);
    let out_of_order = sensor_frame(50, 500, STATUS_OK);

    let s = node.step(
        10_000,
        &[bad_crc, bad_len, out_of_range, duplicate, out_of_order],
    );
    assert_eq!(s.accepted, 1, "none of the five may be accepted");
    assert_eq!(s.rejected, 5);
    let by = node.rejected_by_reason();
    assert_eq!(by[Reject::BadCrc.index()], 1);
    assert_eq!(by[Reject::BadLength.index()], 1);
    assert_eq!(by[Reject::OutOfRange.index()], 1);
    assert_eq!(by[Reject::Duplicate.index()], 1);
    assert_eq!(
        by[Reject::OutOfOrder {
            expected: 0,
            got: 0
        }
        .index()],
        1
    );

    // The next in-order frame is still accepted: rejections did not advance
    // the sequence position.
    let s = node.step(20_000, &[sensor_frame(1, 500, STATUS_OK)]);
    assert_eq!(s.accepted, 2);
    assert_eq!(s.rejected, 5);
}

#[test]
fn rejected_frames_alone_do_not_hold_off_the_watchdog() {
    // The design claim: the watchdog is fed by acceptance, not arrival. A
    // sensor babbling corrupt frames must still trip it.
    let mut node = ControlNode::new(0);
    node.step(0, &[sensor_frame(0, 500, STATUS_OK)]);
    let mut i = 1u64;
    while i <= 20 {
        let mut bad = sensor_frame(i as u8, 500, STATUS_OK);
        bad.data[7] ^= 0xA5;
        let s = node.step(i * 10_000, &[bad]);
        if i * 10_000 > 100_000 {
            assert!(
                s.safe_state.is_some(),
                "should be latched by {} us",
                i * 10_000
            );
        }
        i += 1;
    }
    assert_eq!(node.rejected_by_reason()[Reject::BadCrc.index()], 20);
}

#[test]
fn setpoint_and_timeout_are_the_frozen_values() {
    // A guard on the manifest: if these constants drift, every number in
    // results/ is describing a different experiment.
    assert_eq!(SETPOINT, 500.0);
    assert_eq!(WATCHDOG_TIMEOUT_US, 100_000);
    assert_eq!(fieldbus_core::control::PERIOD_US, 10_000);
    let f = command_frame(0, 0, STATUS_OK);
    assert_eq!(Id::unpack(f.id).source_address, SA_CONTROLLER);
}
