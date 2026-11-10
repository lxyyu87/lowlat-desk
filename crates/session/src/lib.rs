//! Internet session support for lowlat-desk.
//!
//! Video stays on a lossy datagram path. Mouse and keyboard use a reliable
//! stream with sequence numbers and retransmission. A Noise PSK handshake
//! authenticates the password and encrypts both paths, so the signaling
//! server cannot read the session even when it relays it.
//!
//! LAN mode does not use this crate: the host still accepts a direct UDP
//! video connection and a TCP input connection on the same port.

mod adapt;
mod crypto;
mod link;
mod pointer;
mod reliable;
mod stun;
mod wire;

pub use adapt::BitrateControl;
pub use crypto::{pass_psk, pass_verifier};
pub use link::{establish, Role, SecureLink, SessionParams};
pub use pointer::map_pointer;
pub use stun::{
    decode_binding_response, encode_binding_request, parse_server_list, stun_from_args,
    DEFAULT_STUN,
};

/// Bytes wrapped around one video datagram on the relay path:
/// relay header (21) + secure header (13) + Poly1305 tag (16) + channel byte (1).
pub const SESSION_WRAP: usize = 51;
pub use wire::{
    decode_message, encode_message, parse_udp, read_message_from, udp_bind_ack, SigMsg, UdpPacket,
};
