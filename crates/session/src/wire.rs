use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, Result};

pub const UDP_MAGIC: &[u8; 4] = b"RD02";
pub const UDP_BIND: u8 = 1;
pub const UDP_RELAY: u8 = 2;
pub const UDP_PUNCH: u8 = 3;
pub const UDP_SECURE: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigMsg {
    Register {
        id: String,
        verifier: [u8; 32],
    },
    RegisterOk {
        token: [u8; 16],
    },
    Connect {
        id: String,
    },
    Intro {
        token: [u8; 16],
        peer_token: [u8; 16],
    },
    Noise(Vec<u8>),
    Candidate(SocketAddr),
    CandidatesDone,
    UseRelay,
    RelayOn,
    PunchOk,
    Error(String),
}

pub fn encode_message(msg: &SigMsg) -> Vec<u8> {
    let mut body = Vec::new();
    match msg {
        SigMsg::Register { id, verifier } => {
            body.push(1);
            push_str(&mut body, id);
            body.extend_from_slice(verifier);
        }
        SigMsg::RegisterOk { token } => {
            body.push(2);
            body.extend_from_slice(token);
        }
        SigMsg::Connect { id } => {
            body.push(3);
            push_str(&mut body, id);
        }
        SigMsg::Intro { token, peer_token } => {
            body.push(4);
            body.extend_from_slice(token);
            body.extend_from_slice(peer_token);
        }
        SigMsg::Noise(data) => {
            body.push(5);
            body.extend_from_slice(&(data.len() as u16).to_le_bytes());
            body.extend_from_slice(data);
        }
        SigMsg::Candidate(addr) => {
            body.push(6);
            push_addr(&mut body, *addr);
        }
        SigMsg::CandidatesDone => body.push(7),
        SigMsg::UseRelay => body.push(8),
        SigMsg::RelayOn => body.push(9),
        SigMsg::PunchOk => body.push(10),
        SigMsg::Error(text) => {
            body.push(11);
            push_str(&mut body, text);
        }
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn decode_message(bytes: &[u8]) -> Result<SigMsg> {
    if bytes.is_empty() {
        bail!("empty signal message");
    }
    match bytes[0] {
        1 => {
            let (id, rest) = take_str(&bytes[1..])?;
            if rest.len() < 32 {
                bail!("truncated verifier");
            }
            let mut verifier = [0u8; 32];
            verifier.copy_from_slice(&rest[..32]);
            Ok(SigMsg::Register { id, verifier })
        }
        2 => Ok(SigMsg::RegisterOk {
            token: take_token(&bytes[1..])?,
        }),
        3 => {
            let (id, _) = take_str(&bytes[1..])?;
            Ok(SigMsg::Connect { id })
        }
        4 => {
            if bytes.len() < 33 {
                bail!("truncated intro");
            }
            Ok(SigMsg::Intro {
                token: take_token(&bytes[1..17])?,
                peer_token: take_token(&bytes[17..33])?,
            })
        }
        5 => {
            if bytes.len() < 3 {
                bail!("truncated noise");
            }
            let len = u16::from_le_bytes(bytes[1..3].try_into().unwrap()) as usize;
            if bytes.len() < 3 + len {
                bail!("truncated noise");
            }
            Ok(SigMsg::Noise(bytes[3..3 + len].to_vec()))
        }
        6 => {
            let (addr, _) = take_addr(&bytes[1..])?;
            Ok(SigMsg::Candidate(addr))
        }
        7 => Ok(SigMsg::CandidatesDone),
        8 => Ok(SigMsg::UseRelay),
        9 => Ok(SigMsg::RelayOn),
        10 => Ok(SigMsg::PunchOk),
        11 => {
            let (text, _) = take_str(&bytes[1..])?;
            Ok(SigMsg::Error(text))
        }
        other => bail!("unknown signal message {other}"),
    }
}

pub fn read_message_from(buffer: &[u8]) -> Result<Option<(SigMsg, usize)>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
    if len > 1024 * 1024 {
        bail!("signal message is too large");
    }
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some((decode_message(&buffer[4..4 + len])?, 4 + len)))
}

pub fn udp_bind(token: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    out.extend_from_slice(UDP_MAGIC);
    out.push(UDP_BIND);
    out.extend_from_slice(token);
    out
}

pub fn udp_bind_ack(token: &[u8; 16], observed: SocketAddr) -> Vec<u8> {
    let mut out = udp_bind(token);
    push_addr(&mut out, observed);
    out
}

pub fn udp_punch(token: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    out.extend_from_slice(UDP_MAGIC);
    out.push(UDP_PUNCH);
    out.extend_from_slice(token);
    out
}

pub fn udp_secure(nonce: u64, ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + ciphertext.len());
    out.extend_from_slice(UDP_MAGIC);
    out.push(UDP_SECURE);
    out.extend_from_slice(&nonce.to_le_bytes());
    out.extend_from_slice(ciphertext);
    out
}

pub fn udp_relay(token: &[u8; 16], inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(21 + inner.len());
    out.extend_from_slice(UDP_MAGIC);
    out.push(UDP_RELAY);
    out.extend_from_slice(token);
    out.extend_from_slice(inner);
    out
}

#[derive(Debug)]
pub enum UdpPacket {
    Bind {
        token: [u8; 16],
    },
    BindAck {
        token: [u8; 16],
        observed: SocketAddr,
    },
    Relay {
        token: [u8; 16],
        inner: Vec<u8>,
    },
    Punch {
        token: [u8; 16],
    },
    Secure {
        nonce: u64,
        ciphertext: Vec<u8>,
    },
}

pub fn parse_udp(packet: &[u8]) -> Option<UdpPacket> {
    if packet.len() < 5 || packet[..4] != *UDP_MAGIC {
        return None;
    }
    match packet[4] {
        UDP_BIND if packet.len() >= 21 => {
            let token = token_at(packet, 5)?;
            if packet.len() == 21 {
                Some(UdpPacket::Bind { token })
            } else {
                let (observed, _) = take_addr(&packet[21..]).ok()?;
                Some(UdpPacket::BindAck { token, observed })
            }
        }
        UDP_RELAY if packet.len() >= 21 => Some(UdpPacket::Relay {
            token: token_at(packet, 5)?,
            inner: packet[21..].to_vec(),
        }),
        UDP_PUNCH if packet.len() >= 21 => Some(UdpPacket::Punch {
            token: token_at(packet, 5)?,
        }),
        UDP_SECURE if packet.len() >= 13 => Some(UdpPacket::Secure {
            nonce: u64::from_le_bytes(packet[5..13].try_into().ok()?),
            ciphertext: packet[13..].to_vec(),
        }),
        _ => None,
    }
}

fn token_at(packet: &[u8], offset: usize) -> Option<[u8; 16]> {
    let mut token = [0u8; 16];
    token.copy_from_slice(packet.get(offset..offset + 16)?);
    Some(token)
}

fn push_str(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
}

fn take_str(bytes: &[u8]) -> Result<(String, &[u8])> {
    if bytes.len() < 2 {
        bail!("truncated string");
    }
    let len = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as usize;
    if bytes.len() < 2 + len {
        bail!("truncated string");
    }
    let text = String::from_utf8_lossy(&bytes[2..2 + len]).into_owned();
    Ok((text, &bytes[2 + len..]))
}

fn take_token(bytes: &[u8]) -> Result<[u8; 16]> {
    if bytes.len() < 16 {
        bail!("truncated token");
    }
    let mut token = [0u8; 16];
    token.copy_from_slice(&bytes[..16]);
    Ok(token)
}

fn push_addr(out: &mut Vec<u8>, addr: SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            out.push(4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_le_bytes());
        }
        SocketAddr::V6(v6) => {
            out.push(6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_le_bytes());
        }
    }
}

fn take_addr(bytes: &[u8]) -> Result<(SocketAddr, &[u8])> {
    if bytes.is_empty() {
        bail!("truncated address");
    }
    match bytes[0] {
        4 => {
            if bytes.len() < 7 {
                bail!("truncated ipv4");
            }
            let ip = Ipv4Addr::new(bytes[1], bytes[2], bytes[3], bytes[4]);
            let port = u16::from_le_bytes(bytes[5..7].try_into().unwrap());
            Ok((SocketAddr::new(IpAddr::V4(ip), port), &bytes[7..]))
        }
        6 => {
            if bytes.len() < 19 {
                bail!("truncated ipv6");
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&bytes[1..17]);
            let port = u16::from_le_bytes(bytes[17..19].try_into().unwrap());
            Ok((
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port),
                &bytes[19..],
            ))
        }
        _ => bail!("unknown address family"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_messages_roundtrip_inside_a_stream() {
        let messages = [
            SigMsg::Register {
                id: "desk".into(),
                verifier: [7; 32],
            },
            SigMsg::Candidate("203.0.113.5:47000".parse().unwrap()),
            SigMsg::Noise(vec![1, 2, 3, 4]),
            SigMsg::UseRelay,
        ];
        let mut stream = Vec::new();
        for message in &messages {
            stream.extend_from_slice(&encode_message(message));
        }
        let mut offset = 0;
        for expected in &messages {
            let (decoded, n) = read_message_from(&stream[offset..]).unwrap().unwrap();
            assert_eq!(&decoded, expected);
            offset += n;
        }
        assert!(read_message_from(&stream[offset..]).unwrap().is_none());
    }
}
