use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::messages::{decode_control, parse_packet, write_packet, Control, Header};
use crate::{kind, Error, FLAG_KEYFRAME, HEADER_LEN, MAX_DATAGRAM};

/// How far a missing sequence may sit behind a newer packet before it is
/// declared lost. Small UDP reordering stays inside the window.
pub const REORDER_WINDOW: u32 = 32;
const PARTIAL_TIMEOUT: Duration = Duration::from_millis(30);

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub frame_id: u32,
    pub is_keyframe: bool,
    pub capture_us: u64,
    pub send_us: u64,
    pub annex_b: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub frame_id: u32,
    pub is_keyframe: bool,
    pub capture_us: u64,
    pub send_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct PushOutcome {
    pub controls: Vec<Control>,
    pub request_keyframe: bool,
}

/// Splits access units into MTU-sized datagrams and assigns stream sequence
/// numbers. Control datagrams share the same sequence space so the receiver
/// can see loss between a HelloAck and the first video packet.
#[derive(Debug)]
pub struct Fragmenter {
    next_seq: u32,
    max_datagram: usize,
}

impl Default for Fragmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragmenter {
    pub fn new() -> Self {
        Self {
            next_seq: 0,
            max_datagram: MAX_DATAGRAM,
        }
    }

    /// Smaller datagrams leave room for an encryption header on the internet path.
    pub fn with_max_datagram(mut self, max_datagram: usize) -> Self {
        self.max_datagram = max_datagram.max(HEADER_LEN + 1);
        self
    }

    pub fn control(&mut self, kind_id: u8, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let header = Header {
            kind: kind_id,
            flags: 0,
            seq: self.next_seq,
            frame_id: 0,
            frag_index: 0,
            frag_count: 1,
            capture_us: 0,
            send_us: 0,
        };
        self.next_seq = self.next_seq.wrapping_add(1);
        write_packet(&header, payload)
    }

    pub fn packetize(&mut self, frame: &VideoFrame) -> Result<Vec<Vec<u8>>, Error> {
        let max_payload = self.max_datagram.saturating_sub(HEADER_LEN).max(1);
        let payload = frame.annex_b.as_slice();
        let frag_count = if payload.is_empty() {
            1
        } else {
            payload.len().div_ceil(max_payload)
        };
        if frag_count > u16::MAX as usize {
            return Err(Error::FrameTooLarge);
        }
        let mut packets = Vec::with_capacity(frag_count);
        for index in 0..frag_count {
            let start = index * max_payload;
            let end = (start + max_payload).min(payload.len());
            let chunk = if payload.is_empty() {
                &[][..]
            } else {
                &payload[start..end]
            };
            let header = Header {
                kind: kind::VIDEO,
                flags: if frame.is_keyframe { FLAG_KEYFRAME } else { 0 },
                seq: self.next_seq,
                frame_id: frame.frame_id,
                frag_index: index as u16,
                frag_count: frag_count as u16,
                capture_us: frame.capture_us,
                send_us: frame.send_us,
            };
            self.next_seq = self.next_seq.wrapping_add(1);
            packets.push(write_packet(&header, chunk)?);
        }
        Ok(packets)
    }
}

#[derive(Debug)]
struct Partial {
    frame_id: u32,
    frag_count: u16,
    parts: Vec<Option<Vec<u8>>>,
    got: u16,
    is_keyframe: bool,
    capture_us: u64,
    send_us: u64,
    started: Instant,
}

/// Reassembles video frames and reports when a gap should trigger an IDR.
///
/// Only the newest completed frame is kept. While waiting for a keyframe,
/// non-IDR frames are dropped so a loss cannot be displayed as a corrupt
/// reference chain.
#[derive(Debug)]
pub struct Reassembler {
    next_seq: u32,
    buffer: BTreeMap<u32, Vec<u8>>,
    partial: Option<Partial>,
    latest: Option<AssembledFrame>,
    wait_idr: bool,
    hole_since: Option<Instant>,
    received: u64,
    lost: u64,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            next_seq: 0,
            buffer: BTreeMap::new(),
            partial: None,
            latest: None,
            wait_idr: false,
            hole_since: None,
            received: 0,
            lost: 0,
        }
    }

    /// Packets delivered and packets skipped since the last call.
    pub fn take_loss(&mut self) -> (u64, u64) {
        let counters = (self.received, self.lost);
        self.received = 0;
        self.lost = 0;
        counters
    }

    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Result<PushOutcome, Error> {
        let (header, _) = parse_packet(datagram)?;
        let mut outcome = PushOutcome::default();
        let behind = self.next_seq.wrapping_sub(header.seq);
        if header.seq != self.next_seq && behind > 0 && behind < 0x8000_0000 {
            // Already delivered, or a duplicate sitting behind the cursor.
            return Ok(outcome);
        }
        self.buffer.insert(header.seq, datagram.to_vec());
        self.drain(now, &mut outcome)?;
        Ok(outcome)
    }

    pub fn poll(&mut self, now: Instant) -> Result<PushOutcome, Error> {
        let mut outcome = PushOutcome::default();
        if let Some(partial) = &self.partial {
            let stale = now
                .checked_duration_since(partial.started)
                .is_some_and(|age| age > PARTIAL_TIMEOUT);
            if partial.got < partial.frag_count && stale {
                self.partial = None;
                self.wait_idr = true;
                outcome.request_keyframe = true;
            }
        }
        if self.hole_since.is_some_and(|since| {
            now.checked_duration_since(since)
                .is_some_and(|age| age > PARTIAL_TIMEOUT)
        }) {
            if let Some((&seq, _)) = self.buffer.iter().next() {
                let gap = seq.wrapping_sub(self.next_seq) as u64;
                self.lost = self.lost.wrapping_add(gap);
                self.partial = None;
                self.wait_idr = true;
                outcome.request_keyframe = true;
                self.next_seq = seq;
                self.hole_since = None;
                self.drain(now, &mut outcome)?;
            }
        }
        Ok(outcome)
    }

    pub fn take_latest(&mut self) -> Option<AssembledFrame> {
        self.latest.take()
    }

    fn drain(&mut self, now: Instant, outcome: &mut PushOutcome) -> Result<(), Error> {
        loop {
            if self.buffer.contains_key(&self.next_seq) {
                let datagram = self.buffer.remove(&self.next_seq).expect("checked");
                self.next_seq = self.next_seq.wrapping_add(1);
                self.received = self.received.wrapping_add(1);
                self.hole_since = None;
                self.deliver(&datagram, now, outcome)?;
                continue;
            }
            let Some((&seq, _)) = self.buffer.iter().next() else {
                break;
            };
            let ahead = seq.wrapping_sub(self.next_seq);
            if ahead > REORDER_WINDOW && ahead < 0x8000_0000 {
                self.lost = self.lost.wrapping_add(u64::from(ahead));
                self.partial = None;
                self.wait_idr = true;
                outcome.request_keyframe = true;
                self.next_seq = seq;
                self.hole_since = None;
                continue;
            }
            if self.hole_since.is_none() {
                self.hole_since = Some(now);
            }
            break;
        }
        Ok(())
    }

    fn deliver(
        &mut self,
        datagram: &[u8],
        now: Instant,
        outcome: &mut PushOutcome,
    ) -> Result<(), Error> {
        let (header, payload) = parse_packet(datagram)?;
        if header.kind == kind::VIDEO {
            self.deliver_video(&header, payload, now, outcome);
            return Ok(());
        }
        if let Some(control) = decode_control(header.kind, payload)? {
            outcome.controls.push(control);
        }
        Ok(())
    }

    fn deliver_video(
        &mut self,
        header: &Header,
        payload: &[u8],
        now: Instant,
        outcome: &mut PushOutcome,
    ) {
        if header.frag_count == 0 {
            self.wait_idr = true;
            outcome.request_keyframe = true;
            return;
        }
        let (restart, lost_previous) = match &self.partial {
            None => (true, false),
            Some(partial) if partial.frame_id != header.frame_id => {
                (true, partial.got < partial.frag_count)
            }
            Some(_) => (false, false),
        };
        if lost_previous {
            self.wait_idr = true;
            outcome.request_keyframe = true;
        }
        if restart {
            self.partial = Some(Partial {
                frame_id: header.frame_id,
                frag_count: header.frag_count,
                parts: vec![None; header.frag_count as usize],
                got: 0,
                is_keyframe: false,
                capture_us: header.capture_us,
                send_us: header.send_us,
                started: now,
            });
        }
        let index = header.frag_index as usize;
        let stored = {
            let Some(partial) = self.partial.as_mut() else {
                return;
            };
            if index >= partial.parts.len() || header.frag_count != partial.frag_count {
                false
            } else {
                if partial.parts[index].is_none() {
                    partial.parts[index] = Some(payload.to_vec());
                    partial.got += 1;
                }
                partial.is_keyframe |= header.is_keyframe();
                true
            }
        };
        if !stored {
            self.partial = None;
            self.wait_idr = true;
            outcome.request_keyframe = true;
            return;
        }
        let complete = self
            .partial
            .as_ref()
            .is_some_and(|partial| partial.got == partial.frag_count);
        if !complete {
            return;
        }
        let partial = self.partial.take().expect("complete");
        if self.wait_idr && !partial.is_keyframe {
            return;
        }
        if partial.is_keyframe {
            self.wait_idr = false;
        }
        let mut data = Vec::new();
        for part in partial.parts.into_iter().flatten() {
            data.extend_from_slice(&part);
        }
        self.latest = Some(AssembledFrame {
            frame_id: partial.frame_id,
            is_keyframe: partial.is_keyframe,
            capture_us: partial.capture_us,
            send_us: partial.send_us,
            data,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Hello, HelloAck};
    use crate::{kind, CODEC_H264};

    fn frame(id: u32, key: bool, payload: Vec<u8>) -> VideoFrame {
        VideoFrame {
            frame_id: id,
            is_keyframe: key,
            capture_us: 10,
            send_us: 20,
            annex_b: payload,
        }
    }

    #[test]
    fn in_order_roundtrip_including_hello() {
        let mut tx = Fragmenter::new();
        let hello = tx
            .control(
                kind::HELLO_ACK,
                &HelloAck {
                    width: 64,
                    height: 64,
                    fps: 60,
                    codec: CODEC_H264,
                }
                .encode(),
            )
            .unwrap();
        let video = tx.packetize(&frame(1, true, vec![7u8; 2000])).unwrap();
        assert!(video.len() > 1);

        let mut rx = Reassembler::new();
        let now = Instant::now();
        let outcome = rx.push(&hello, now).unwrap();
        assert_eq!(
            outcome.controls,
            vec![Control::HelloAck(HelloAck {
                width: 64,
                height: 64,
                fps: 60,
                codec: CODEC_H264,
            })]
        );
        assert!(!outcome.request_keyframe);
        for packet in &video {
            let outcome = rx.push(packet, now).unwrap();
            assert!(!outcome.request_keyframe);
        }
        let assembled = rx.take_latest().unwrap();
        assert_eq!(assembled.data, vec![7u8; 2000]);
        assert!(assembled.is_keyframe);
        assert_eq!(assembled.capture_us, 10);
        assert_eq!(assembled.send_us, 20);
    }

    #[test]
    fn reorders_fragments_without_requesting_a_keyframe() {
        let mut tx = Fragmenter::new();
        let mut packets = tx.packetize(&frame(1, true, vec![1u8; 2000])).unwrap();
        packets.reverse();
        let mut rx = Reassembler::new();
        let now = Instant::now();
        let mut requested = false;
        for packet in &packets {
            requested |= rx.push(packet, now).unwrap().request_keyframe;
        }
        assert!(!requested);
        assert_eq!(rx.take_latest().unwrap().data, vec![1u8; 2000]);
    }

    #[test]
    fn loss_requests_keyframe_and_drops_delta_frames() {
        let mut tx = Fragmenter::new();
        let key = tx.packetize(&frame(1, true, vec![9u8; 2000])).unwrap();
        let delta = tx.packetize(&frame(2, false, vec![3u8; 50])).unwrap();
        let next_key = tx.packetize(&frame(3, true, vec![8u8; 40])).unwrap();

        let mut rx = Reassembler::new();
        let t0 = Instant::now();
        // Deliver only the first fragment of the first keyframe.
        rx.push(&key[0], t0).unwrap();
        // The rest of that keyframe is gone. The delta arrives next.
        let pushed = rx.push(&delta[0], t0).unwrap();
        assert!(
            !pushed.request_keyframe,
            "a one-packet gap stays inside the reorder window"
        );
        let polled = rx.poll(t0 + Duration::from_millis(40)).unwrap();
        assert!(polled.request_keyframe);
        assert!(
            rx.take_latest().is_none(),
            "delta frames are dropped until an IDR"
        );

        rx.push(&next_key[0], t0 + Duration::from_millis(50))
            .unwrap();
        let assembled = rx.take_latest().unwrap();
        assert!(assembled.is_keyframe);
        assert_eq!(assembled.frame_id, 3);
        assert_eq!(assembled.data, vec![8u8; 40]);
    }

    #[test]
    fn duplicates_do_not_corrupt_the_frame() {
        let mut tx = Fragmenter::new();
        let packets = tx
            .packetize(&frame(1, true, b"hello-frame".to_vec()))
            .unwrap();
        let mut rx = Reassembler::new();
        let now = Instant::now();
        rx.push(&packets[0], now).unwrap();
        rx.push(&packets[0], now).unwrap();
        assert_eq!(rx.take_latest().unwrap().data, b"hello-frame");
    }

    #[test]
    fn client_hello_is_a_control_packet() {
        let mut tx = Fragmenter::new();
        let packet = tx
            .control(
                kind::HELLO,
                &Hello {
                    name: "rd-client".into(),
                }
                .encode(),
            )
            .unwrap();
        let mut rx = Reassembler::new();
        let outcome = rx.push(&packet, Instant::now()).unwrap();
        assert_eq!(
            outcome.controls,
            vec![Control::Hello(Hello {
                name: "rd-client".into()
            })]
        );
    }
}
