use crate::messages::InputEvent;
use crate::Error;

/// One length-prefixed message on the LAN input TCP connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMessage {
    Events(Vec<InputEvent>),
    Loss(u16),
}

pub fn encode_input_message(message: &InputMessage) -> Vec<u8> {
    let mut body = Vec::new();
    match message {
        InputMessage::Events(events) => {
            body.push(1);
            let count = events.len().min(u16::MAX as usize);
            body.extend_from_slice(&(count as u16).to_le_bytes());
            for event in events.iter().take(count) {
                let encoded = event.encode();
                let len = encoded.len().min(u16::MAX as usize);
                body.extend_from_slice(&(len as u16).to_le_bytes());
                body.extend_from_slice(&encoded[..len]);
            }
        }
        InputMessage::Loss(permille) => {
            body.push(2);
            body.extend_from_slice(&permille.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// `Ok(None)` means the buffer does not contain a whole message yet.
pub fn read_input_message(buffer: &[u8]) -> Result<Option<(InputMessage, usize)>, Error> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
    if len > 1024 * 1024 {
        return Err(Error::BadPayload);
    }
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let body = &buffer[4..4 + len];
    if body.is_empty() {
        return Err(Error::BadPayload);
    }
    let message = match body[0] {
        1 => {
            if body.len() < 3 {
                return Err(Error::BadPayload);
            }
            let count = u16::from_le_bytes(body[1..3].try_into().unwrap()) as usize;
            let mut rest = &body[3..];
            let mut events = Vec::with_capacity(count);
            for _ in 0..count {
                if rest.len() < 2 {
                    return Err(Error::BadPayload);
                }
                let n = u16::from_le_bytes(rest[..2].try_into().unwrap()) as usize;
                if rest.len() < 2 + n {
                    return Err(Error::BadPayload);
                }
                events.push(InputEvent::decode(&rest[2..2 + n])?);
                rest = &rest[2 + n..];
            }
            InputMessage::Events(events)
        }
        2 => {
            if body.len() < 3 {
                return Err(Error::BadPayload);
            }
            InputMessage::Loss(u16::from_le_bytes(body[1..3].try_into().unwrap()))
        }
        _ => return Err(Error::BadPayload),
    };
    Ok(Some((message, 4 + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_batches_survive_a_split_stream() {
        let message = InputMessage::Events(vec![
            InputEvent::MouseMove { x: 15, y: 25 },
            InputEvent::Key {
                vk: 0x41,
                scan: 0,
                pressed: true,
            },
        ]);
        let bytes = encode_input_message(&message);
        assert!(read_input_message(&bytes[..3]).unwrap().is_none());
        let (decoded, n) = read_input_message(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(decoded, message);

        let loss = encode_input_message(&InputMessage::Loss(40));
        let (decoded, _) = read_input_message(&loss).unwrap().unwrap();
        assert_eq!(decoded, InputMessage::Loss(40));
    }
}
