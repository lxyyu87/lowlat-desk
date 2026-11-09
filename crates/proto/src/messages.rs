use super::FLAG_KEYFRAME;
use crate::{kind, Error, HEADER_LEN, MAGIC, VERSION};

/// Fixed 40-byte header. Multi-byte fields are little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub flags: u16,
    pub seq: u32,
    pub frame_id: u32,
    pub frag_index: u16,
    pub frag_count: u16,
    pub capture_us: u64,
    pub send_us: u64,
}

impl Header {
    pub fn is_keyframe(self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

pub fn write_packet(header: &Header, payload: &[u8]) -> Result<Vec<u8>, Error> {
    if payload.len() > u16::MAX as usize {
        return Err(Error::FrameTooLarge);
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(VERSION);
    out.push(header.kind);
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.frame_id.to_le_bytes());
    out.extend_from_slice(&header.frag_index.to_le_bytes());
    out.extend_from_slice(&header.frag_count.to_le_bytes());
    out.extend_from_slice(&header.capture_us.to_le_bytes());
    out.extend_from_slice(&header.send_us.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(payload);
    debug_assert_eq!(HEADER_LEN, out.len() - payload.len());
    Ok(out)
}

pub fn parse_packet(datagram: &[u8]) -> Result<(Header, &[u8]), Error> {
    if datagram.len() < HEADER_LEN {
        return Err(Error::TooShort);
    }
    let magic = u32::from_le_bytes(datagram[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = datagram[4];
    if version != VERSION {
        return Err(Error::BadVersion(version));
    }
    let payload_len = u16::from_le_bytes(datagram[36..38].try_into().unwrap()) as usize;
    if HEADER_LEN + payload_len != datagram.len() {
        return Err(Error::BadLength);
    }
    let header = Header {
        kind: datagram[5],
        flags: u16::from_le_bytes(datagram[6..8].try_into().unwrap()),
        seq: u32::from_le_bytes(datagram[8..12].try_into().unwrap()),
        frame_id: u32::from_le_bytes(datagram[12..16].try_into().unwrap()),
        frag_index: u16::from_le_bytes(datagram[16..18].try_into().unwrap()),
        frag_count: u16::from_le_bytes(datagram[18..20].try_into().unwrap()),
        capture_us: u64::from_le_bytes(datagram[20..28].try_into().unwrap()),
        send_us: u64::from_le_bytes(datagram[28..36].try_into().unwrap()),
    };
    Ok((header, &datagram[HEADER_LEN..]))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub name: String,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let name = if name.len() > 64 { &name[..64] } else { name };
        let mut out = Vec::with_capacity(2 + name.len());
        out.push(VERSION);
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        if payload.len() < 2 {
            return Err(Error::BadPayload);
        }
        let len = payload[1] as usize;
        if payload.len() < 2 + len {
            return Err(Error::BadPayload);
        }
        let name = String::from_utf8_lossy(&payload[2..2 + len]).into_owned();
        Ok(Self { name })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloAck {
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub codec: u8,
}

impl HelloAck {
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.push(self.fps);
        out.push(self.codec);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        if payload.len() < 6 {
            return Err(Error::BadPayload);
        }
        Ok(Self {
            width: u16::from_le_bytes(payload[0..2].try_into().unwrap()),
            height: u16::from_le_bytes(payload[2..4].try_into().unwrap()),
            fps: payload[4],
            codec: payload[5],
        })
    }
}

/// Phase 2 input slot. The phase 1 host does not inject these; the phase 1
/// client does not send them. Absolute mouse coordinates are in the host
/// capture pixel space. `vk` / `scan` match the Windows `SendInput` fields
/// the host will use later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, pressed: bool },
    Wheel { dx: i16, dy: i16 },
    Key { vk: u16, scan: u16, pressed: bool },
}

impl InputEvent {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Self::MouseMove { x, y } => {
                let mut out = vec![1];
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out
            }
            Self::MouseButton { button, pressed } => {
                vec![2, button, u8::from(pressed)]
            }
            Self::Wheel { dx, dy } => {
                let mut out = vec![3];
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out
            }
            Self::Key { vk, scan, pressed } => {
                let mut out = vec![4];
                out.extend_from_slice(&vk.to_le_bytes());
                out.extend_from_slice(&scan.to_le_bytes());
                out.push(u8::from(pressed));
                out
            }
        }
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        if payload.is_empty() {
            return Err(Error::BadPayload);
        }
        match payload[0] {
            1 => {
                if payload.len() < 5 {
                    return Err(Error::BadPayload);
                }
                Ok(Self::MouseMove {
                    x: u16::from_le_bytes(payload[1..3].try_into().unwrap()),
                    y: u16::from_le_bytes(payload[3..5].try_into().unwrap()),
                })
            }
            2 => {
                if payload.len() < 3 {
                    return Err(Error::BadPayload);
                }
                Ok(Self::MouseButton {
                    button: payload[1],
                    pressed: payload[2] != 0,
                })
            }
            3 => {
                if payload.len() < 5 {
                    return Err(Error::BadPayload);
                }
                Ok(Self::Wheel {
                    dx: i16::from_le_bytes(payload[1..3].try_into().unwrap()),
                    dy: i16::from_le_bytes(payload[3..5].try_into().unwrap()),
                })
            }
            4 => {
                if payload.len() < 6 {
                    return Err(Error::BadPayload);
                }
                Ok(Self::Key {
                    vk: u16::from_le_bytes(payload[1..3].try_into().unwrap()),
                    scan: u16::from_le_bytes(payload[3..5].try_into().unwrap()),
                    pressed: payload[5] != 0,
                })
            }
            _ => Err(Error::BadPayload),
        }
    }
}

#[cfg(test)]
pub fn encode_token(token: u64) -> Vec<u8> {
    token.to_le_bytes().to_vec()
}

pub fn decode_token(payload: &[u8]) -> Result<u64, Error> {
    if payload.len() < 8 {
        return Err(Error::BadPayload);
    }
    Ok(u64::from_le_bytes(payload[..8].try_into().unwrap()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Hello(Hello),
    HelloAck(HelloAck),
    KeyframeRequest,
    Ping(u64),
    Pong(u64),
    Goodbye,
    Input(InputEvent),
}

pub fn decode_control(kind_id: u8, payload: &[u8]) -> Result<Option<Control>, Error> {
    match kind_id {
        kind::HELLO => Ok(Some(Control::Hello(Hello::decode(payload)?))),
        kind::HELLO_ACK => Ok(Some(Control::HelloAck(HelloAck::decode(payload)?))),
        kind::KEYFRAME_REQUEST => Ok(Some(Control::KeyframeRequest)),
        kind::PING => Ok(Some(Control::Ping(decode_token(payload)?))),
        kind::PONG => Ok(Some(Control::Pong(decode_token(payload)?))),
        kind::GOODBYE => Ok(Some(Control::Goodbye)),
        kind::INPUT => Ok(Some(Control::Input(InputEvent::decode(payload)?))),
        kind::VIDEO => Ok(None),
        // Unknown kinds are ignored so a newer peer can add packets.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind;

    #[test]
    fn header_roundtrip_and_layout() {
        let header = Header {
            kind: kind::PING,
            flags: FLAG_KEYFRAME,
            seq: 0x0102_0304,
            frame_id: 7,
            frag_index: 1,
            frag_count: 3,
            capture_us: 99,
            send_us: 120,
        };
        let packet = write_packet(&header, &encode_token(42)).unwrap();
        assert_eq!(&packet[0..4], b"RD01");
        assert_eq!(packet.len(), HEADER_LEN + 8);
        let (decoded, payload) = parse_packet(&packet).unwrap();
        assert_eq!(decoded, header);
        assert!(decoded.is_keyframe());
        assert_eq!(decode_token(payload).unwrap(), 42);
    }

    #[test]
    fn rejects_bad_magic_version_and_length() {
        let header = Header {
            kind: kind::GOODBYE,
            flags: 0,
            seq: 0,
            frame_id: 0,
            frag_index: 0,
            frag_count: 1,
            capture_us: 0,
            send_us: 0,
        };
        let mut packet = write_packet(&header, &[]).unwrap();
        packet[0] = 0;
        assert_eq!(parse_packet(&packet), Err(Error::BadMagic));
        packet[0] = b'R';
        packet[4] = 9;
        assert_eq!(parse_packet(&packet), Err(Error::BadVersion(9)));
        packet[4] = VERSION;
        packet.push(1);
        assert_eq!(parse_packet(&packet), Err(Error::BadLength));
    }

    #[test]
    fn hello_and_input_roundtrip() {
        let hello = Hello {
            name: "rd-client".into(),
        };
        assert_eq!(Hello::decode(&hello.encode()).unwrap(), hello);

        let events = [
            InputEvent::MouseMove { x: 10, y: 20 },
            InputEvent::MouseButton {
                button: 0,
                pressed: true,
            },
            InputEvent::Wheel { dx: -1, dy: 3 },
            InputEvent::Key {
                vk: 0x41,
                scan: 0x1e,
                pressed: false,
            },
        ];
        for event in events {
            assert_eq!(InputEvent::decode(&event.encode()).unwrap(), event);
        }
    }

    #[test]
    fn hello_ack_roundtrip() {
        let ack = HelloAck {
            width: 1280,
            height: 720,
            fps: 60,
            codec: crate::CODEC_H264,
        };
        assert_eq!(HelloAck::decode(&ack.encode()).unwrap(), ack);
    }
}
