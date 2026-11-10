use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use rd_proto::InputEvent;

const RETRANSMIT: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReliableMsg {
    Inputs(Vec<InputEvent>),
    Stats { loss_permille: u16, rtt_ms: u16 },
    Ping(u64),
    Pong(u64),
}

impl ReliableMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Inputs(events) => {
                let mut out = vec![1];
                out.extend_from_slice(&(events.len() as u16).to_le_bytes());
                for event in events {
                    let encoded = event.encode();
                    out.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
                    out.extend_from_slice(&encoded);
                }
                out
            }
            Self::Stats {
                loss_permille,
                rtt_ms,
            } => {
                let mut out = vec![2];
                out.extend_from_slice(&loss_permille.to_le_bytes());
                out.extend_from_slice(&rtt_ms.to_le_bytes());
                out
            }
            Self::Ping(token) => {
                let mut out = vec![3];
                out.extend_from_slice(&token.to_le_bytes());
                out
            }
            Self::Pong(token) => {
                let mut out = vec![4];
                out.extend_from_slice(&token.to_le_bytes());
                out
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("empty reliable message");
        }
        match bytes[0] {
            1 => {
                if bytes.len() < 3 {
                    anyhow::bail!("truncated input batch");
                }
                let count = u16::from_le_bytes(bytes[1..3].try_into().unwrap()) as usize;
                let mut offset = 3;
                let mut events = Vec::with_capacity(count);
                for _ in 0..count {
                    if offset + 2 > bytes.len() {
                        anyhow::bail!("truncated input event");
                    }
                    let len =
                        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                    offset += 2;
                    if offset + len > bytes.len() {
                        anyhow::bail!("truncated input event");
                    }
                    events.push(InputEvent::decode(&bytes[offset..offset + len])?);
                    offset += len;
                }
                Ok(Self::Inputs(events))
            }
            2 => {
                if bytes.len() < 5 {
                    anyhow::bail!("truncated stats");
                }
                Ok(Self::Stats {
                    loss_permille: u16::from_le_bytes(bytes[1..3].try_into().unwrap()),
                    rtt_ms: u16::from_le_bytes(bytes[3..5].try_into().unwrap()),
                })
            }
            3 | 4 => {
                if bytes.len() < 9 {
                    anyhow::bail!("truncated ping");
                }
                let token = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
                if bytes[0] == 3 {
                    Ok(Self::Ping(token))
                } else {
                    Ok(Self::Pong(token))
                }
            }
            _ => anyhow::bail!("unknown reliable message"),
        }
    }
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    packet: Vec<u8>,
    last_sent: Instant,
}

/// Sequence numbers and retransmission for input and stats.
/// Video datagrams never enter this queue.
#[derive(Debug)]
pub struct ReliableEndpoint {
    next_out: u64,
    next_in: u64,
    pending: VecDeque<Pending>,
    held: BTreeMap<u64, Vec<u8>>,
    ack: u64,
}

impl Default for ReliableEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl ReliableEndpoint {
    pub fn new() -> Self {
        Self {
            next_out: 1,
            next_in: 1,
            pending: VecDeque::new(),
            held: BTreeMap::new(),
            ack: 0,
        }
    }

    pub fn send(&mut self, msg: &ReliableMsg, now: Instant) -> Vec<u8> {
        let seq = self.next_out;
        self.next_out = self.next_out.wrapping_add(1);
        let packet = encode_frame(1, seq, self.ack, &msg.encode());
        self.pending.push_back(Pending {
            seq,
            packet: packet.clone(),
            last_sent: now,
        });
        packet
    }

    pub fn retransmit(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        for pending in &mut self.pending {
            if now.saturating_duration_since(pending.last_sent) >= RETRANSMIT {
                pending.last_sent = now;
                packets.push(pending.packet.clone());
            }
        }
        packets
    }

    /// Returns messages that are now in order, plus an ack packet when the
    /// peer should be told what we have received.
    pub fn handle(&mut self, packet: &[u8]) -> Result<(Vec<ReliableMsg>, Option<Vec<u8>>)> {
        let (kind, seq, ack, body) = decode_frame(packet)?;
        self.pending.retain(|pending| pending.seq > ack);
        if kind == 2 {
            return Ok((Vec::new(), None));
        }
        if seq < self.next_in {
            return Ok((Vec::new(), Some(encode_frame(2, 0, self.ack, &[]))));
        }
        self.held.insert(seq, body);
        let mut delivered = Vec::new();
        while let Some(body) = self.held.remove(&self.next_in) {
            delivered.push(ReliableMsg::decode(&body)?);
            self.ack = self.next_in;
            self.next_in = self.next_in.wrapping_add(1);
        }
        Ok((delivered, Some(encode_frame(2, 0, self.ack, &[]))))
    }
}

fn encode_frame(kind: u8, seq: u64, ack: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(19 + body.len());
    out.push(kind);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&ack.to_le_bytes());
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn decode_frame(packet: &[u8]) -> Result<(u8, u64, u64, Vec<u8>)> {
    if packet.len() < 19 {
        anyhow::bail!("reliable frame is too short");
    }
    let kind = packet[0];
    let seq = u64::from_le_bytes(packet[1..9].try_into().unwrap());
    let ack = u64::from_le_bytes(packet[9..17].try_into().unwrap());
    let len = u16::from_le_bytes(packet[17..19].try_into().unwrap()) as usize;
    if packet.len() < 19 + len {
        anyhow::bail!("reliable frame is truncated");
    }
    Ok((kind, seq, ack, packet[19..19 + len].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retransmit_until_acked_and_deliver_in_order() {
        let mut sender = ReliableEndpoint::new();
        let mut receiver = ReliableEndpoint::new();
        let t0 = Instant::now();
        let first = sender.send(
            &ReliableMsg::Inputs(vec![InputEvent::MouseMove { x: 1, y: 2 }]),
            t0,
        );
        let second = sender.send(
            &ReliableMsg::Inputs(vec![InputEvent::MouseButton {
                button: 0,
                pressed: true,
            }]),
            t0,
        );
        let (msgs, _) = receiver.handle(&second).unwrap();
        assert!(msgs.is_empty(), "a hole holds the later event");
        let (msgs, ack) = receiver.handle(&first).unwrap();
        assert_eq!(msgs.len(), 2);
        sender.handle(ack.as_deref().unwrap()).unwrap();
        let (late, _) = receiver.handle(&second).unwrap();
        assert!(late.is_empty());
        assert!(sender.retransmit(t0 + RETRANSMIT).is_empty());
        let _ = first;
    }
}
