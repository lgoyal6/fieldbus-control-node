//! `fieldbus-core`: the portable half of a periodic CAN control node.
//!
//! This crate is `#![no_std]`, allocation-free and `unsafe`-free. Everything it
//! does is a pure function of its inputs and its own state, so the same frame
//! sequence produces byte-identical output on a host and on a microcontroller.
//!
//! What this crate is not: it is not an implementation of the SAE J1939
//! transport protocol. It uses the J1939 *identifier layout* (see
//! [`j1939`]) with two proprietary-B parameter group numbers, and nothing
//! above that layer. It has never been executed on a microcontroller; the
//! `thumbv7em-none-eabihf` build in the completion gate is a cross-compile
//! check only.

#![no_std]

pub mod j1939;

pub use j1939::{Frame, Reject, Validator};
