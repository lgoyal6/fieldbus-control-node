//! `fieldbus-host`: the parts of the node that need an operating system.
//!
//! Everything here is deliberately outside [`fieldbus_core`]: clocks, threads,
//! sockets, scheduling policy, allocation and JSON. The split is what lets the
//! control logic be replayed deterministically and cross-compiled to a
//! microcontroller target, and it is also where the honest labelling lives.
//! A run driven by [`bus::SimBus`] is a **simulation**: an in-process bus with
//! no wire, no transceiver and no physical sensor. A run driven by
//! [`socketcan::SocketCanBus`] uses real `AF_CAN` sockets, but on this machine
//! there is no CAN adapter, so that backend is exercised only against a
//! virtual `vcan0` interface in CI. Neither is hardware in the loop.


pub mod bus;
pub mod hog;
pub mod manifest;
pub mod runner;

#[cfg(all(target_os = "linux", feature = "socketcan"))]
pub mod socketcan;
