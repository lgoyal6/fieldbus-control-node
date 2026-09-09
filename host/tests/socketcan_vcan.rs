//! SocketCAN integration test against a virtual `vcan0` interface.
//!
//! Runs only on Linux, only with the `socketcan` feature, and only when asked
//! for by name with `--ignored`. It needs an interface that has to be created
//! with root privileges:
//!
//! ```text
//! sudo modprobe vcan
//! sudo ip link add dev vcan0 type vcan
//! sudo ip link set up vcan0
//! cargo test -p fieldbus-host --features socketcan -- --ignored --nocapture
//! ```
//!
//! What it proves: the same [`Validator`] that the simulator faces also faces
//! real `AF_CAN` sockets, and reaches the same verdicts on a valid frame, an
//! out-of-order frame and a bad-CRC frame. What it does not prove: anything
//! about a physical CAN bus. `vcan0` is a kernel loopback with no transceiver
//! and no bit timing, so this is not hardware in the loop.

#![cfg(all(target_os = "linux", feature = "socketcan"))]

use std::time::Duration;

use fieldbus_core::j1939::{crc8_j1850, sensor_frame, Reject, Validator, STATUS_OK};
use fieldbus_host::bus::CanBus;
use fieldbus_host::socketcan::SocketCanBus;

const IFACE: &str = "vcan0";

#[test]
#[ignore = "needs a vcan0 interface; created by the CI workflow with root privileges"]
fn validator_reaches_the_same_verdicts_over_real_af_can_sockets() {
    // Two sockets on the same interface: a CAN socket does not receive its
    // own transmissions by default, so the reader has to be a separate one.
    let mut rx = SocketCanBus::open(IFACE, Duration::from_millis(500))
        .expect("open vcan0 for reading; is the interface up?");
    let mut tx = SocketCanBus::open(IFACE, Duration::ZERO).expect("open vcan0 for writing");
    assert_eq!(rx.interface(), IFACE);

    // Drain anything a previous test left queued, so the sequence position
    // this test establishes is its own.
    let _ = rx.recv_until(0);

    let valid = sensor_frame(10, 500, STATUS_OK);
    let out_of_order = sensor_frame(40, 500, STATUS_OK);
    let mut bad_crc = sensor_frame(11, 500, STATUS_OK);
    bad_crc.data[7] ^= 0xFF;
    assert_ne!(
        bad_crc.data[7],
        crc8_j1850(&bad_crc.data[0..7]),
        "the bad-CRC frame must actually have a bad CRC"
    );

    tx.send(valid).expect("send valid frame");
    tx.send(out_of_order).expect("send out-of-order frame");
    tx.send(bad_crc).expect("send bad-CRC frame");

    // Collect all three back off the socket. vcan delivery is immediate, but
    // read in a bounded loop rather than assuming one call returns everything.
    let mut received = Vec::new();
    for _ in 0..20 {
        received.extend(rx.recv_until(0));
        if received.len() >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        received.len(),
        3,
        "expected three frames back from {IFACE}, got {}: {received:?}",
        received.len()
    );

    // Round trip through the kernel must be byte-exact, identifier included.
    assert_eq!(
        received[0], valid,
        "valid frame did not survive the round trip"
    );
    assert_eq!(received[1], out_of_order);
    assert_eq!(received[2], bad_crc);

    // And the verdicts must match what the simulator gets for the same bytes.
    let mut v = Validator::new();
    assert!(
        v.validate(&received[0]).is_ok(),
        "the first frame after reset is accepted at any seq"
    );
    assert_eq!(
        v.validate(&received[1]),
        Err(Reject::OutOfOrder {
            expected: 11,
            got: 40
        })
    );
    assert_eq!(v.validate(&received[2]), Err(Reject::BadCrc));
    assert_eq!(v.accepted(), 1);
    assert_eq!(v.rejected(), 2);
    assert_eq!(v.by_reason()[Reject::BadCrc.index()], 1);
    assert_eq!(
        v.by_reason()[Reject::OutOfOrder {
            expected: 0,
            got: 0
        }
        .index()],
        1
    );
}

#[test]
#[ignore = "needs a vcan0 interface; created by the CI workflow with root privileges"]
fn a_short_frame_arrives_short_and_is_rejected_for_it() {
    // BadLength has to be reachable over a real socket, not just in the
    // simulator, or the validator's first check is untested where it matters.
    let mut rx = SocketCanBus::open(IFACE, Duration::from_millis(500)).expect("open vcan0 rx");
    let mut tx = SocketCanBus::open(IFACE, Duration::ZERO).expect("open vcan0 tx");
    let _ = rx.recv_until(0);

    let mut short = sensor_frame(0, 500, STATUS_OK);
    short.dlc = 4;
    tx.send(short).expect("send short frame");

    let mut received = Vec::new();
    for _ in 0..20 {
        received.extend(rx.recv_until(0));
        if !received.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(received.len(), 1, "expected one frame, got {received:?}");
    assert_eq!(
        received[0].dlc, 4,
        "the kernel must report the short length"
    );

    let mut v = Validator::new();
    assert_eq!(v.validate(&received[0]), Err(Reject::BadLength));
}
