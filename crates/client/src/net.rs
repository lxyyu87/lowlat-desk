use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rd_codec::open_decoder;
use rd_proto::{kind, Control, Fragmenter, Hello, Reassembler, CODEC_H264};
use tracing::{debug, info, warn};
use winit::event_loop::EventLoopProxy;

use crate::Presented;

pub struct Shared {
    pub running: AtomicBool,
    pub latest: Mutex<Option<Presented>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            latest: Mutex::new(None),
        }
    }
}

#[derive(Debug)]
pub enum UserEvent {
    Frame,
}

pub(crate) struct LatencyMeter {
    present_us: Vec<u64>,
    host_us: Vec<u64>,
    started: Instant,
}

impl LatencyMeter {
    fn new() -> Self {
        Self {
            present_us: Vec::new(),
            host_us: Vec::new(),
            started: Instant::now(),
        }
    }

    fn record(&mut self, frame_id: u32, receive_to_present_us: u64, host_capture_to_send_us: u64) {
        debug!(
            frame_id,
            receive_to_present_us, host_capture_to_send_us, "receive->present"
        );
        self.present_us.push(receive_to_present_us);
        self.host_us.push(host_capture_to_send_us);
        if self.started.elapsed() < Duration::from_secs(1) || self.present_us.is_empty() {
            return;
        }
        let summarize = |samples: &[u64]| {
            let n = samples.len() as u64;
            let sum: u64 = samples.iter().sum();
            let max = samples.iter().copied().max().unwrap_or(0);
            (n, sum / n.max(1), max)
        };
        let (n, present_avg, present_max) = summarize(&self.present_us);
        let (_, host_avg, host_max) = summarize(&self.host_us);
        info!(
            frames = n,
            receive_to_present_avg_us = present_avg,
            receive_to_present_max_us = present_max,
            host_capture_to_send_avg_us = host_avg,
            host_capture_to_send_max_us = host_max,
            "receive->present"
        );
        self.present_us.clear();
        self.host_us.clear();
        self.started = Instant::now();
    }
}

pub fn run(
    addr: std::net::SocketAddr,
    shared: Arc<Shared>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("bind client socket")?;
    socket
        .connect(addr)
        .with_context(|| format!("connect {addr}"))?;
    socket.set_read_timeout(Some(Duration::from_millis(20)))?;
    info!(addr = %addr, local = %socket.local_addr()?, "connecting");

    let mut decoder = open_decoder()?;
    let mut reassembler = Reassembler::new();
    let mut fragmenter = Fragmenter::new();
    let mut got_ack = false;
    let mut video_size = (0u16, 0u16);
    let mut last_hello = Instant::now() - Duration::from_secs(1);
    let mut last_ping = Instant::now();
    let mut ping_token = 1u64;
    let mut last_keyframe = Instant::now() - Duration::from_secs(1);
    let mut buf = [0u8; 2048];

    while shared.running.load(Ordering::Relaxed) {
        if !got_ack && last_hello.elapsed() >= Duration::from_millis(500) {
            let hello = Hello {
                name: "rd-client".into(),
            };
            socket.send(&fragmenter.control(kind::HELLO, &hello.encode())?)?;
            last_hello = Instant::now();
            debug!("sent hello");
        }
        if got_ack && last_ping.elapsed() >= Duration::from_secs(1) {
            socket.send(&fragmenter.control(kind::PING, &ping_token.to_le_bytes())?)?;
            ping_token = ping_token.wrapping_add(1);
            last_ping = Instant::now();
        }

        let received = match socket.recv(&mut buf) {
            Ok(n) => Some(n),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                None
            }
            Err(err) => return Err(err).context("recv"),
        };

        if received.is_none() {
            let outcome = reassembler.poll(Instant::now())?;
            maybe_request_keyframe(
                &socket,
                &mut fragmenter,
                &mut last_keyframe,
                outcome.request_keyframe,
            )?;
            continue;
        }

        // Drain every datagram already queued so we decode only the newest frame.
        socket.set_nonblocking(true)?;
        let mut request_keyframe = false;
        let mut pending = Some(received.unwrap());
        while let Some(n) = pending.take().or_else(|| socket.recv(&mut buf).ok()) {
            let now = Instant::now();
            let outcome = reassembler.push(&buf[..n], now)?;
            request_keyframe |= outcome.request_keyframe;
            for control in outcome.controls {
                match control {
                    Control::HelloAck(ack) => {
                        if ack.codec != CODEC_H264 {
                            warn!(codec = ack.codec, "host offered an unsupported codec");
                        }
                        info!(
                            width = ack.width,
                            height = ack.height,
                            fps = ack.fps,
                            "hello ack"
                        );
                        if video_size != (ack.width, ack.height) {
                            video_size = (ack.width, ack.height);
                            decoder.set_stream_info(
                                ack.width as u32,
                                ack.height as u32,
                                ack.fps as u32,
                            );
                        }
                        got_ack = true;
                    }
                    Control::Pong(_) => {}
                    other => debug!(?other, "control"),
                }
            }
        }
        socket.set_nonblocking(false)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;

        maybe_request_keyframe(
            &socket,
            &mut fragmenter,
            &mut last_keyframe,
            request_keyframe,
        )?;

        if let Some(frame) = reassembler.take_latest() {
            let t_recv = Instant::now();
            match decoder.decode(&frame.data) {
                Ok(Some(picture)) => {
                    let host_us = frame.send_us.saturating_sub(frame.capture_us);
                    {
                        let mut slot = shared.latest.lock().expect("frame slot");
                        *slot = Some(Presented {
                            rgba: picture.rgba,
                            width: picture.width,
                            height: picture.height,
                            t_recv,
                            frame_id: frame.frame_id,
                            host_capture_to_send_us: host_us,
                        });
                    }
                    let _ = proxy.send_event(UserEvent::Frame);
                }
                Ok(None) => debug!(
                    frame_id = frame.frame_id,
                    "decoder buffered the access unit"
                ),
                Err(err) => {
                    warn!(error = %err, frame_id = frame.frame_id, "decode failed");
                    maybe_request_keyframe(&socket, &mut fragmenter, &mut last_keyframe, true)?;
                }
            }
        }
    }

    let _ = socket.send(&fragmenter.control(kind::GOODBYE, &[])?);
    Ok(())
}

fn maybe_request_keyframe(
    socket: &UdpSocket,
    fragmenter: &mut Fragmenter,
    last: &mut Instant,
    needed: bool,
) -> Result<()> {
    if needed && last.elapsed() >= Duration::from_millis(200) {
        socket.send(&fragmenter.control(kind::KEYFRAME_REQUEST, &[])?)?;
        *last = Instant::now();
        info!("requested a keyframe");
    }
    Ok(())
}

pub fn note_presented(
    meter: &mut Option<LatencyMeter>,
    presented: &Presented,
    presented_at: Instant,
) {
    let meter = meter.get_or_insert_with(LatencyMeter::new);
    let receive_to_present_us = presented_at.duration_since(presented.t_recv).as_micros() as u64;
    meter.record(
        presented.frame_id,
        receive_to_present_us,
        presented.host_capture_to_send_us,
    );
}
