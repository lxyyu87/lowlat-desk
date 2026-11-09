//! Wire protocol shared by the host and the client.
//!
//! Phase 1 carries these datagrams over UDP on a LAN. The bytes do not
//! assume a socket, a broadcast address, or a particular NAT technique, so a
//! later signaling path can hand the same datagrams to a punched UDP socket
//! or a TURN relay without changing this crate.
//!
//! Phase 2 mouse and keyboard events already have a packet kind ([`InputEvent`])
//! but nothing sends them yet.

mod fragment;
mod h264;
mod messages;

pub use fragment::{
    AssembledFrame, Fragmenter, PushOutcome, Reassembler, VideoFrame, REORDER_WINDOW,
};
pub use h264::{
    avcc_record_to_annex_b, contains_idr, contains_parameter_sets, is_annex_b, iter_annex_b_nals,
    nal_type, normalize_access_unit, try_length_prefixed_to_annex_b, ParameterSetCache,
};
pub use messages::{parse_packet, write_packet, Control, Header, Hello, HelloAck, InputEvent};

/// ASCII `RD01`, little-endian on the wire.
pub const MAGIC: u32 = u32::from_le_bytes(*b"RD01");
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 40;
/// Safe UDP payload on Ethernet and on common tunnel MTUs.
pub const MAX_DATAGRAM: usize = 1200;
pub const FLAG_KEYFRAME: u16 = 1 << 0;

/// Codec id carried in [`HelloAck`].
pub const CODEC_H264: u8 = 1;

pub mod kind {
    pub const HELLO: u8 = 1;
    pub const HELLO_ACK: u8 = 2;
    pub const VIDEO: u8 = 3;
    pub const KEYFRAME_REQUEST: u8 = 4;
    pub const PING: u8 = 5;
    pub const PONG: u8 = 6;
    pub const GOODBYE: u8 = 7;
    /// Reserved for phase 2. Parsed, never sent by the phase 1 binaries.
    pub const INPUT: u8 = 8;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("datagram is shorter than the header")]
    TooShort,
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    BadVersion(u8),
    #[error("payload length does not match the datagram")]
    BadLength,
    #[error("control payload is truncated or invalid")]
    BadPayload,
    #[error("encoded frame does not fit in u16 fragments")]
    FrameTooLarge,
}
