//! Linux SocketCAN backend: real `AF_CAN` raw sockets.
//!
//! # What this is and is not
//!
//! The sockets here are real. The frames really go through the kernel's CAN
//! stack, and the identifiers really are 29-bit extended identifiers on the
//! wire format the kernel defines. What is not real, anywhere this code has
//! actually been executed, is the bus: there is no CAN adapter on the
//! development machine and none on a GitHub Actions runner, so this backend
//! is exercised against a **virtual** `vcan0` interface. A virtual CAN
//! interface is a kernel loopback. It has no transceiver, no bit timing, no
//! arbitration, no error counters and no bus-off state.
//!
//! So: real sockets, real kernel path, no bus, and not hardware in the loop.
//! No timing number in `results/` comes from this backend; the timing gate
//! runs on macOS against the in-process simulator, and this module exists so
//! that the same validator faces a real socket at least once, in CI, rather
//! than only ever facing a simulator written by the same hand.

use std::time::Duration;

use socketcan::{CanDataFrame, CanFrame, CanSocket, EmbeddedFrame, ExtendedId, Id, Socket};

use fieldbus_core::j1939::Frame as BusFrame;

use crate::bus::{BusError, CanBus};

/// A `CanBus` backed by one `AF_CAN` raw socket.
pub struct SocketCanBus {
    sock: CanSocket,
    iface: String,
}

impl SocketCanBus {
    /// Opens `iface` (for example `"vcan0"`).
    ///
    /// `read_timeout` bounds a single read. Zero means fully non-blocking,
    /// which is what a periodic node wants: drain whatever the kernel already
    /// has and get on with the period.
    pub fn open(iface: &str, read_timeout: Duration) -> Result<Self, BusError> {
        let sock = CanSocket::open(iface)
            .map_err(|e| BusError::Io(format!("cannot open CAN interface {iface}: {e}")))?;
        if read_timeout.is_zero() {
            sock.set_nonblocking(true)
                .map_err(|e| BusError::Io(format!("cannot set {iface} non-blocking: {e}")))?;
        } else {
            sock.set_read_timeout(read_timeout)
                .map_err(|e| BusError::Io(format!("cannot set {iface} read timeout: {e}")))?;
        }
        Ok(SocketCanBus {
            sock,
            iface: iface.to_string(),
        })
    }

    /// The interface name this socket is bound to.
    pub fn interface(&self) -> &str {
        &self.iface
    }
}

/// Converts a SocketCAN frame into the crate's own frame type.
///
/// Returns `None` for anything that is not a data frame with an extended
/// identifier. Remote and error frames are not part of either stream this
/// node speaks, and a standard 11-bit identifier cannot carry a J1939-style
/// PGN at all, so passing one to the validator would only produce a
/// misleading `UnknownPgn` for a frame that was never addressed to this node.
pub fn from_socketcan(frame: &CanFrame) -> Option<BusFrame> {
    let data_frame = match frame {
        CanFrame::Data(d) => d,
        CanFrame::Remote(_) | CanFrame::Error(_) => return None,
    };
    let id = match data_frame.id() {
        Id::Extended(e) => e.as_raw(),
        Id::Standard(_) => return None,
    };
    let payload = data_frame.data();
    let mut data = [0u8; 8];
    let n = payload.len().min(8);
    data[..n].copy_from_slice(&payload[..n]);
    Some(BusFrame {
        id,
        // The real length as the kernel reports it, not the length that was
        // copied. A short frame has to reach the validator as short or
        // `BadLength` could never fire on a real bus.
        dlc: payload.len() as u8,
        data,
    })
}

/// Converts the crate's own frame type into a SocketCAN data frame.
pub fn to_socketcan(frame: &BusFrame) -> Result<CanDataFrame, BusError> {
    let id = ExtendedId::new(frame.id)
        .ok_or_else(|| BusError::Io(format!("identifier {:#x} exceeds 29 bits", frame.id)))?;
    let n = (frame.dlc as usize).min(8);
    CanDataFrame::new(Id::Extended(id), &frame.data[..n])
        .ok_or_else(|| BusError::Io(format!("cannot build a CAN frame with dlc {}", frame.dlc)))
}

impl CanBus for SocketCanBus {
    fn send(&mut self, frame: BusFrame) -> Result<(), BusError> {
        let out = to_socketcan(&frame)?;
        self.sock
            .write_frame(&out)
            .map_err(|e| BusError::Io(format!("write on {} failed: {e}", self.iface)))
    }

    fn recv_until(&mut self, _deadline_us: u64) -> Vec<BusFrame> {
        // The deadline is not used to wait. The socket's own timeout bounds a
        // read, and the loop drains what is queued rather than sleeping for
        // frames that may not come; a periodic node that waited for a silent
        // sensor would convert a sensor fault into a deadline miss and hide
        // the real cause. Staleness is the watchdog's job, not the bus's.
        let mut out = Vec::new();
        loop {
            match self.sock.read_frame() {
                Ok(f) => {
                    if let Some(bf) = from_socketcan(&f) {
                        out.push(bf);
                    }
                }
                // Empty queue or timeout. Anything else is a real error, and
                // it is dropped here rather than propagated because
                // `recv_until` has no way to report one; the validator's
                // counters and the watchdog are what make a broken bus
                // visible.
                Err(_) => break,
            }
        }
        out
    }
}
