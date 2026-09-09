//! J1939-style identifier layout and the 8-byte payload codec.
//!
//! # Identifier layout
//!
//! A 29-bit extended CAN identifier is split exactly as J1939 splits it:
//!
//! ```text
//!  bit 28                                                          bit 0
//!  +-----+-----+-----+-----------------+-----------------+-----------------+
//!  | PRI | RSV |  DP |   PDU format    |  PDU specific   | source address  |
//!  |  3  |  1  |  1  |        8        |        8        |        8        |
//!  +-----+-----+-----+-----------------+-----------------+-----------------+
//! ```
//!
//! The parameter group number (PGN) used here is `DP << 16 | PF << 8 | PS`,
//! which is the proprietary-B form: PDU format `0xFF` means the PDU-specific
//! byte is a group extension rather than a destination address, so both
//! streams are broadcast and every node on the bus sees them.
//!
//! Two PGNs are defined:
//!
//! - [`PGN_SENSOR`] (`0xFF00`), sent by the sensor at source address
//!   [`SA_SENSOR`] (`0x80`).
//! - [`PGN_COMMAND`] (`0xFF01`), sent by this controller at source address
//!   [`SA_CONTROLLER`] (`0x28`).
//!
//! The actuator's own address is [`SA_ACTUATOR`] (`0x81`). Because
//! proprietary B is broadcast, that address is the actuator's identity on the
//! bus, not a destination encoded in the command identifier.
//!
//! # Payload layout
//!
//! Exactly 8 bytes, no variable-length encoding and no transport protocol:
//!
//! ```text
//!  byte 0 | 1        2 | 3      | 4  5  6           | 7
//!  seq u8 | value i16  | status | reserved, all 0x00| crc8
//!         | little-end | u8     |                   |
//! ```
//!
//! The CRC is CRC-8 with the SAE J1850 polynomial `0x1D`, initial value
//! `0xFF`, no reflection and no final XOR, computed over bytes 0 through 6
//! inclusive. Byte 7 carries it.

/// Priority field of the sensor and command identifiers.
pub const PRIORITY: u8 = 3;
/// Parameter group number of the sensor stream (proprietary B).
pub const PGN_SENSOR: u32 = 0xFF00;
/// Parameter group number of the command stream (proprietary B).
pub const PGN_COMMAND: u32 = 0xFF01;
/// Source address of the sensor node.
pub const SA_SENSOR: u8 = 0x80;
/// Source address of this controller.
pub const SA_CONTROLLER: u8 = 0x28;
/// Source address of the actuator node. It listens to the broadcast command.
pub const SA_ACTUATOR: u8 = 0x81;

/// Every frame on both streams carries exactly this many payload bytes.
pub const PAYLOAD_LEN: u8 = 8;

/// Status byte: the sensor reading is valid and the controller is in normal
/// closed-loop operation.
pub const STATUS_OK: u8 = 0x00;
/// Status byte: the controller has latched a safe state and this command
/// carries the safe actuator value rather than a control output.
pub const STATUS_SAFE: u8 = 0x01;

/// Inclusive bounds a sensor value must fall inside to be accepted. Frozen in
/// `manifest/frozen.json`; a reading outside them is rejected as
/// [`Reject::OutOfRange`] rather than fed to the controller.
pub const VALUE_MIN: i16 = -10_000;
/// Upper inclusive bound of the accepted sensor range.
pub const VALUE_MAX: i16 = 10_000;

/// A classic CAN frame with an extended (29-bit) identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// 29-bit extended identifier. Bits above bit 28 are always zero.
    pub id: u32,
    /// Payload length in bytes. Both streams here always use 8.
    pub dlc: u8,
    /// Payload. Only the first `dlc` bytes are meaningful.
    pub data: [u8; 8],
}

/// The decoded fields of a 29-bit J1939-style identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Id {
    /// 3-bit priority, 0 is highest.
    pub priority: u8,
    /// 1-bit reserved ("extended data page" in later J1939 revisions).
    pub reserved: u8,
    /// 1-bit data page.
    pub data_page: u8,
    /// PDU format. `0xFF` selects proprietary B, making the frame broadcast.
    pub pdu_format: u8,
    /// PDU specific: a group extension when `pdu_format` is `0xFF`.
    pub pdu_specific: u8,
    /// Source address of the transmitting node.
    pub source_address: u8,
}

impl Id {
    /// Builds an identifier for a broadcast proprietary-B PGN from `sa`.
    ///
    /// `pgn` is truncated to its low 17 bits, which is the width of
    /// `DP | PF | PS`; the caller supplies only the two PGNs defined above.
    pub const fn from_pgn(priority: u8, pgn: u32, sa: u8) -> Self {
        Id {
            priority: priority & 0x07,
            reserved: 0,
            data_page: ((pgn >> 16) & 0x01) as u8,
            pdu_format: ((pgn >> 8) & 0xFF) as u8,
            pdu_specific: (pgn & 0xFF) as u8,
            source_address: sa,
        }
    }

    /// Packs the fields into a 29-bit identifier. Every field is masked to its
    /// own width, so a caller cannot make one field bleed into the next.
    pub const fn pack(&self) -> u32 {
        ((self.priority as u32 & 0x07) << 26)
            | ((self.reserved as u32 & 0x01) << 25)
            | ((self.data_page as u32 & 0x01) << 24)
            | ((self.pdu_format as u32) << 16)
            | ((self.pdu_specific as u32) << 8)
            | (self.source_address as u32)
    }

    /// Unpacks a 29-bit identifier. Bits 29 and above are ignored.
    pub const fn unpack(raw: u32) -> Self {
        Id {
            priority: ((raw >> 26) & 0x07) as u8,
            reserved: ((raw >> 25) & 0x01) as u8,
            data_page: ((raw >> 24) & 0x01) as u8,
            pdu_format: ((raw >> 16) & 0xFF) as u8,
            pdu_specific: ((raw >> 8) & 0xFF) as u8,
            source_address: (raw & 0xFF) as u8,
        }
    }

    /// The parameter group number this identifier belongs to.
    pub const fn pgn(&self) -> u32 {
        ((self.data_page as u32) << 16)
            | ((self.pdu_format as u32) << 8)
            | (self.pdu_specific as u32)
    }
}

/// The decoded 8-byte payload of a sensor or command frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Payload {
    /// Sequence number, incremented by the sender and wrapping at 255.
    pub seq: u8,
    /// Signed engineering value: a reading on the sensor stream, a commanded
    /// actuator position on the command stream.
    pub value: i16,
    /// [`STATUS_OK`] or [`STATUS_SAFE`].
    pub status: u8,
}

impl Payload {
    /// Encodes the payload and appends the CRC in byte 7.
    pub fn encode(&self) -> [u8; 8] {
        let mut d = [0u8; 8];
        d[0] = self.seq;
        let v = self.value.to_le_bytes();
        d[1] = v[0];
        d[2] = v[1];
        d[3] = self.status;
        // Bytes 4..7 stay zero: they are the reserved field, and a non-zero
        // reserved byte is a rejection reason rather than something to ignore.
        d[7] = crc8_j1850(&d[0..7]);
        d
    }

    /// Decodes bytes 0 through 3. The caller is responsible for having already
    /// checked the CRC and the reserved bytes; [`Validator`] does both first.
    pub fn decode(d: &[u8; 8]) -> Self {
        Payload {
            seq: d[0],
            value: i16::from_le_bytes([d[1], d[2]]),
            status: d[3],
        }
    }
}

/// CRC-8 with the SAE J1850 polynomial `0x1D`, init `0xFF`, no reflection and
/// no final XOR. Bit-at-a-time on purpose: a 256-byte table would be faster
/// and would also be 256 bytes of flash on the target for a computation that
/// runs twice per 10 ms period.
pub fn crc8_j1850(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0xFF;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x1D;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Why a frame was not accepted.
///
/// One variant per distinct cause, and the validator checks them in a fixed
/// order (see [`Validator::validate`]) so that a given malformed frame always
/// produces the same reason. That fixed order is what makes the per-reason
/// counters comparable between runs, and what lets the `can-corrupt` negative
/// control assert an exact histogram instead of a total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// `dlc` was not 8.
    BadLength,
    /// The PGN was neither the sensor nor a recognised stream.
    UnknownPgn,
    /// Right PGN, wrong source address.
    UnexpectedSource,
    /// Byte 7 did not match the CRC over bytes 0 through 6.
    BadCrc,
    /// One of the reserved bytes 4, 5 or 6 was non-zero.
    BadReserved,
    /// `seq` equalled the last accepted `seq`: a retransmission or an echo.
    Duplicate,
    /// `seq` was neither the last accepted `seq` nor its successor.
    OutOfOrder {
        /// The `seq` that would have been accepted.
        expected: u8,
        /// The `seq` that arrived.
        got: u8,
    },
    /// The value fell outside [`VALUE_MIN`]..=[`VALUE_MAX`].
    OutOfRange,
}

/// Number of variants in [`Reject`]; the width of the counter array.
pub const REJECT_KINDS: usize = 8;

impl Reject {
    /// Stable index of this reason in the counter array. Stable across
    /// versions on purpose: the numbers in `results/` are keyed by it.
    pub const fn index(&self) -> usize {
        match self {
            Reject::BadLength => 0,
            Reject::UnknownPgn => 1,
            Reject::UnexpectedSource => 2,
            Reject::BadCrc => 3,
            Reject::BadReserved => 4,
            Reject::Duplicate => 5,
            Reject::OutOfOrder { .. } => 6,
            Reject::OutOfRange => 7,
        }
    }

    /// The reason name used as a JSON key in `results/`.
    pub const fn name(index: usize) -> &'static str {
        match index {
            0 => "bad_length",
            1 => "unknown_pgn",
            2 => "unexpected_source",
            3 => "bad_crc",
            4 => "bad_reserved",
            5 => "duplicate",
            6 => "out_of_order",
            _ => "out_of_range",
        }
    }
}

/// Accepts sensor frames and counts why the rest were refused.
///
/// The validator is the only thing that decides whether a reading reaches the
/// controller, and it is shared verbatim by the simulated bus and the
/// SocketCAN backend. A frame accepted here is the only thing that feeds the
/// watchdog, so a bus that goes quiet and a bus that goes loud with garbage
/// both end in the same latched safe state.
#[derive(Clone, Debug)]
pub struct Validator {
    last_seq: Option<u8>,
    accepted: u32,
    rejected: u32,
    by_reason: [u32; REJECT_KINDS],
}

impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}

impl Validator {
    /// A validator that has not yet seen a frame.
    pub const fn new() -> Self {
        Validator {
            last_seq: None,
            accepted: 0,
            rejected: 0,
            by_reason: [0; REJECT_KINDS],
        }
    }

    /// Validates one frame against the sensor stream.
    ///
    /// The checks run in this fixed order, cheapest and most structural
    /// first, so that a frame with several faults is always attributed to the
    /// same one:
    ///
    /// 1. [`Reject::BadLength`] - the payload is not 8 bytes, so nothing below
    ///    can be read safely.
    /// 2. [`Reject::UnknownPgn`] - not the sensor PGN.
    /// 3. [`Reject::UnexpectedSource`] - sensor PGN from the wrong address.
    /// 4. [`Reject::BadCrc`] - the bytes are not trustworthy, so the sequence
    ///    number and value in them are not either. This must precede every
    ///    content check.
    /// 5. [`Reject::BadReserved`] - a reserved byte carries data, which means
    ///    the sender is not the protocol version this node implements.
    /// 6. [`Reject::Duplicate`] - `seq` repeats the last accepted one.
    /// 7. [`Reject::OutOfOrder`] - `seq` skipped or went backwards.
    /// 8. [`Reject::OutOfRange`] - a well-formed frame carrying an
    ///    implausible reading.
    ///
    /// The first frame after construction or [`Validator::reset_sequence`] is
    /// accepted at any `seq`, because a node that boots mid-stream has no way
    /// to know where the sender's counter is and refusing to synchronise would
    /// be a worse failure than trusting the first frame.
    ///
    /// # Known limitation: no resynchronisation after a real gap
    ///
    /// A rejected frame never advances the sequence position. That is what
    /// makes fault attribution exact, and it is why an injected bad frame
    /// produces one rejection rather than cascading into a second on the
    /// next good frame. The cost is that a *genuine* gap, where the sender
    /// really did transmit a frame that never arrived, desynchronises this
    /// validator permanently: every later frame is one ahead of what it
    /// expects, so every later frame is refused as [`Reject::OutOfOrder`]
    /// and the watchdog eventually trips on starvation.
    ///
    /// A production node would resynchronise, most simply by accepting a
    /// forward gap and counting it. That is not done here because the
    /// rejection semantics are frozen in `manifest/frozen.json` and the
    /// `can-corrupt` negative control asserts an exact per-reason histogram
    /// against them; changing the rule after freezing it would invalidate
    /// the control it is measured by. The limitation is stated rather than
    /// quietly fixed, and [`crate::Watchdog`] is what keeps the failure
    /// safe rather than silent.
    pub fn validate(&mut self, frame: &Frame) -> Result<Payload, Reject> {
        let outcome = self.classify(frame);
        match outcome {
            Ok(p) => {
                self.last_seq = Some(p.seq);
                self.accepted += 1;
                Ok(p)
            }
            Err(r) => {
                self.rejected += 1;
                self.by_reason[r.index()] += 1;
                Err(r)
            }
        }
    }

    fn classify(&self, frame: &Frame) -> Result<Payload, Reject> {
        if frame.dlc != PAYLOAD_LEN {
            return Err(Reject::BadLength);
        }
        let id = Id::unpack(frame.id);
        if id.pgn() != PGN_SENSOR {
            return Err(Reject::UnknownPgn);
        }
        if id.source_address != SA_SENSOR {
            return Err(Reject::UnexpectedSource);
        }
        if frame.data[7] != crc8_j1850(&frame.data[0..7]) {
            return Err(Reject::BadCrc);
        }
        if frame.data[4] != 0 || frame.data[5] != 0 || frame.data[6] != 0 {
            return Err(Reject::BadReserved);
        }
        let p = Payload::decode(&frame.data);
        if let Some(last) = self.last_seq {
            if p.seq == last {
                return Err(Reject::Duplicate);
            }
            let expected = last.wrapping_add(1);
            if p.seq != expected {
                return Err(Reject::OutOfOrder {
                    expected,
                    got: p.seq,
                });
            }
        }
        if p.value < VALUE_MIN || p.value > VALUE_MAX {
            return Err(Reject::OutOfRange);
        }
        Ok(p)
    }

    /// Total frames accepted since construction.
    pub const fn accepted(&self) -> u32 {
        self.accepted
    }

    /// Total frames rejected since construction, for any reason.
    pub const fn rejected(&self) -> u32 {
        self.rejected
    }

    /// Per-reason rejection counts, indexed by [`Reject::index`].
    pub const fn by_reason(&self) -> &[u32; REJECT_KINDS] {
        &self.by_reason
    }

    /// Forgets the sequence position without clearing the counters, so the
    /// next frame is accepted at any `seq`. Used after a latched safe state is
    /// cleared, when the sender has been restarted and its counter with it.
    pub fn reset_sequence(&mut self) {
        self.last_seq = None;
    }
}

/// Builds a sensor frame. Used by the simulator and by the tests; the real
/// sensor is another node on the bus.
pub fn sensor_frame(seq: u8, value: i16, status: u8) -> Frame {
    Frame {
        id: Id::from_pgn(PRIORITY, PGN_SENSOR, SA_SENSOR).pack(),
        dlc: PAYLOAD_LEN,
        data: Payload { seq, value, status }.encode(),
    }
}

/// Builds a command frame from this controller.
pub fn command_frame(seq: u8, value: i16, status: u8) -> Frame {
    Frame {
        id: Id::from_pgn(PRIORITY, PGN_COMMAND, SA_CONTROLLER).pack(),
        dlc: PAYLOAD_LEN,
        data: Payload { seq, value, status }.encode(),
    }
}
