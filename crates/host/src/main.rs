//! Host: capture, encode, and send a screen.
//!
//! On Windows this captures the desktop with DXGI Desktop Duplication, encodes
//! H.264, and injects client input with `SendInput`. LAN viewers connect by
//! IP. Internet viewers meet this host through `rd-signal`. Other operating
//! systems can still run `--test-pattern`.

mod capture;
mod input;
mod transport;

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use capture::{prepare_frame, FrameSource, TestPattern};
use clap::Parser;
use rd_codec::{open_encoder, EncodeConfig, VideoEncoder};
use rd_proto::{
    kind, Fragmenter, HelloAck, ParameterSetCache, VideoFrame, CODEC_H264, MAX_DATAGRAM,
};
use rd_session::{Role, SessionParams, SESSION_WRAP};
use tracing::{debug, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "rd-host",
    version,
    about = "Send a low-latency H.264 view of this machine"
)]
struct Args {
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:47000")]
    listen: std::net::SocketAddr,
    /// Capture / encode rate.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Target bitrate in bits per second.
    #[arg(long, default_value_t = 8_000_000)]
    bitrate: u32,
    /// Scale the desktop down so the width does not exceed this. 0 keeps the desktop size.
    #[arg(long, default_value_t = 1920)]
    max_width: u32,
    /// DXGI output index. Ignored with `--test-pattern`.
    #[arg(long, default_value_t = 0)]
    display: u32,
    /// IDR interval, in frames. B-frames are not used.
    #[arg(long, default_value_t = 15)]
    gop: u32,
    /// Encode a moving test pattern instead of the desktop.
    #[arg(long)]
    test_pattern: bool,
    /// Test-pattern width.
    #[arg(long, default_value_t = 1280)]
    width: u32,
    /// Test-pattern height.
    #[arg(long, default_value_t = 720)]
    height: u32,
    /// Signal server, for example `203.0.113.10:47010`. Omit this to stay on the LAN.
    #[arg(long)]
    signal: Option<std::net::SocketAddr>,
    /// Device id the client types to find this host.
    #[arg(long)]
    id: Option<String>,
    /// Shared password. The signal server only sees a verifier, not this value.
    #[arg(long)]
    password: Option<String>,
    /// Skip UDP hole punching and relay through the signal server.
    #[arg(long)]
    force_relay: bool,
    /// STUN servers. Empty uses the built-in list. `none` disables STUN.
    #[arg(long, value_delimiter = ',')]
    stun: Vec<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(err) = run(Args::parse()) {
        eprintln!("rd-host: {err:#}");
        std::process::exit(1);
    }
}

struct Client {
    addr: std::net::SocketAddr,
    seen: Instant,
}

struct LatencyMeter {
    samples: Vec<u64>,
    started: Instant,
}

impl LatencyMeter {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
            started: Instant::now(),
        }
    }

    fn push(&mut self, capture_to_send_us: u64) {
        self.samples.push(capture_to_send_us);
        debug!(capture_to_send_us, "capture->send");
        if self.started.elapsed() >= Duration::from_secs(1) && !self.samples.is_empty() {
            let n = self.samples.len() as u64;
            let sum: u64 = self.samples.iter().sum();
            let max = self.samples.iter().copied().max().unwrap_or(0);
            info!(frames = n, avg_us = sum / n, max_us = max, "capture->send");
            self.samples.clear();
            self.started = Instant::now();
        }
    }
}

fn run(args: Args) -> Result<()> {
    if !(1..=240).contains(&args.fps) {
        bail!("--fps must be 1..=240");
    }
    let mut source = open_source(&args)?;
    let (source_w, source_h) = source.size();
    info!(width = source_w, height = source_h, "capture size");
    if args.signal.is_some() {
        loop {
            let transport = match connect_signal(&args) {
                Ok(transport) => transport,
                Err(err) => {
                    warn!(error = %err, "signal registration failed");
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            if let Err(err) = stream(&args, &mut source, transport) {
                warn!(error = %err, "internet session ended");
                thread::sleep(Duration::from_secs(1));
            }
        }
    } else {
        let transport = transport::Transport::lan(args.listen, args.bitrate)?;
        stream(&args, &mut source, transport)
    }
}

fn connect_signal(args: &Args) -> Result<transport::Transport> {
    let signal = args.signal.context("missing --signal")?;
    let id = args.id.clone().context("--id is required with --signal")?;
    let password = args
        .password
        .clone()
        .context("--password is required with --signal")?;
    info!(%signal, %id, "registering with the signal server");
    let params = SessionParams {
        signal,
        id,
        password,
        role: Role::Host,
        force_relay: args.force_relay,
        stun: rd_session::stun_from_args(&args.stun),
    };
    transport::Transport::internet(&params, args.bitrate)
}

fn stream(
    args: &Args,
    source: &mut Box<dyn FrameSource>,
    mut link: transport::Transport,
) -> Result<()> {
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut encoded_size = (0u32, 0u32);
    let mut fragmenter = Fragmenter::new();
    let mut parameters = ParameterSetCache::default();
    let mut client: Option<Client> = None;
    let mut frame_id = 1u32;
    let mut force_idr = true;
    let mut cached: Option<Vec<u8>> = None;
    let mut latency = LatencyMeter::new();
    let epoch = Instant::now();
    let frame_interval = Duration::from_micros(1_000_000 / args.fps as u64);
    let mut next_tick = Instant::now();
    let max_datagram = if link.is_internet() {
        MAX_DATAGRAM - SESSION_WRAP
    } else {
        MAX_DATAGRAM
    };
    fragmenter = fragmenter.with_max_datagram(max_datagram);
    let mut applied_bitrate = args.bitrate;

    loop {
        for (packet, src) in link.pump()? {
            if let Some(next) =
                handle_control(&packet, src, &mut client, &mut fragmenter, &mut link)?
            {
                force_idr = true;
                if next {
                    // A new viewer always gets a fresh IDR and a HelloAck.
                    encoded_size = (0, 0);
                }
            }
        }
        let events = link.take_inputs();
        if !events.is_empty() && encoded_size.0 > 0 {
            let (desktop_w, desktop_h) = source.size();
            let (origin_x, origin_y) = source.origin();
            input::inject(
                &events,
                input::DesktopTarget {
                    origin_x,
                    origin_y,
                    desktop_w,
                    desktop_h,
                    encoded_w: encoded_size.0,
                    encoded_h: encoded_size.1,
                },
            );
        }
        let adapted = link.bitrate();
        if applied_bitrate > 0 && adapted.abs_diff(applied_bitrate) * 100 / applied_bitrate >= 15 {
            info!(from = applied_bitrate, to = adapted, "adapted bitrate");
            applied_bitrate = adapted;
            encoder = None;
            force_idr = true;
        }

        let Some(viewer) = client.as_ref() else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        if viewer.seen.elapsed() > Duration::from_secs(5) {
            info!(addr = %viewer.addr, "client timed out");
            client = None;
            continue;
        }
        let viewer_addr = viewer.addr;

        let now = Instant::now();
        if now < next_tick && !force_idr {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        next_tick = Instant::now() + frame_interval;

        let t_capture = Instant::now();
        let captured = source.poll(0)?;
        let prepared = if let Some(frame) = captured.as_ref() {
            Some(prepare_frame(
                frame,
                if args.test_pattern { 0 } else { args.max_width },
            )?)
        } else if force_idr {
            cached.as_ref().map(|bgra| {
                let (w, h) = encoded_size;
                (bgra.clone(), w, h)
            })
        } else {
            None
        };
        let Some((bgra, width, height)) = prepared.filter(|(_, w, h)| *w > 0 && *h > 0) else {
            continue;
        };
        if encoded_size != (width, height) || encoder.is_none() {
            let config = EncodeConfig {
                width,
                height,
                fps: args.fps,
                bitrate_bps: applied_bitrate,
                gop: args.gop.max(1),
            };
            encoder = Some(open_encoder(config)?);
            encoded_size = (width, height);
            force_idr = true;
            parameters = ParameterSetCache::default();
            let ack = HelloAck {
                width: width as u16,
                height: height as u16,
                fps: args.fps.min(255) as u8,
                codec: CODEC_H264,
            };
            let packet = fragmenter.control(kind::HELLO_ACK, &ack.encode())?;
            link.send(&packet, viewer_addr)?;
            info!(width, height, "sent HelloAck");
        }
        cached = Some(bgra.clone());
        let encoder = encoder.as_mut().expect("encoder");
        if force_idr {
            encoder.force_keyframe();
            force_idr = false;
        }
        let encoded = match encoder.encode(&bgra) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(err) => {
                warn!(error = %err, "encode failed; requesting a fresh IDR next tick");
                force_idr = true;
                continue;
            }
        };
        parameters.observe(&encoded.annex_b);
        let annex_b = parameters.ensure(&encoded.annex_b, encoded.is_keyframe);
        let capture_us = t_capture.duration_since(epoch).as_micros() as u64;
        let send_us = Instant::now().duration_since(epoch).as_micros() as u64;
        let packets = fragmenter.packetize(&VideoFrame {
            frame_id,
            is_keyframe: encoded.is_keyframe,
            capture_us,
            send_us,
            annex_b,
        })?;
        frame_id = frame_id.wrapping_add(1);
        for packet in &packets {
            if let Err(err) = link.send(packet, viewer_addr) {
                if link.is_internet() {
                    return Err(err);
                }
                warn!(error = %err, "send failed");
                break;
            }
        }
        latency.push(send_us.saturating_sub(capture_us));
    }
}

fn open_source(args: &Args) -> Result<Box<dyn FrameSource>> {
    if args.test_pattern {
        info!("streaming a test pattern");
        return Ok(Box::new(TestPattern::new(args.width, args.height)));
    }
    #[cfg(windows)]
    {
        return Ok(Box::new(capture::DxgiCapture::open(args.display)?));
    }
    #[cfg(not(windows))]
    {
        bail!(
            "desktop capture requires Windows (DXGI Desktop Duplication). \
             Re-run with --test-pattern to exercise the encoder and the UDP stream."
        );
    }
}

/// Returns `Some(true)` when the viewer changed and the stream should restart.
fn handle_control(
    datagram: &[u8],
    src: std::net::SocketAddr,
    client: &mut Option<Client>,
    fragmenter: &mut Fragmenter,
    link: &mut transport::Transport,
) -> Result<Option<bool>> {
    let max_datagram = if link.is_internet() {
        MAX_DATAGRAM - SESSION_WRAP
    } else {
        MAX_DATAGRAM
    };
    let (header, payload) = match rd_proto::parse_packet(datagram) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    match header.kind {
        kind::HELLO => {
            let hello = rd_proto::Hello::decode(payload).unwrap_or(rd_proto::Hello {
                name: "client".into(),
            });
            let changed = client.as_ref().map(|c| c.addr) != Some(src);
            info!(addr = %src, name = %hello.name, "client hello");
            *client = Some(Client {
                addr: src,
                seen: Instant::now(),
            });
            *fragmenter = Fragmenter::new().with_max_datagram(max_datagram);
            Ok(Some(changed || true))
        }
        kind::KEYFRAME_REQUEST => {
            info!(addr = %src, "keyframe requested");
            touch(client, src);
            Ok(Some(false))
        }
        kind::PING => {
            touch(client, src);
            if payload.len() >= 8 {
                let pong = fragmenter.control(kind::PONG, payload)?;
                let _ = link.send(&pong, src);
            }
            Ok(None)
        }
        kind::GOODBYE => {
            info!(addr = %src, "client goodbye");
            if client.as_ref().map(|c| c.addr) == Some(src) {
                *client = None;
            }
            Ok(None)
        }
        kind::INPUT => {
            // Phase 2 will turn this into SendInput. Phase 1 ignores it.
            touch(client, src);
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn touch(client: &mut Option<Client>, src: std::net::SocketAddr) {
    if let Some(client) = client.as_mut() {
        if client.addr == src {
            client.seen = Instant::now();
        }
    }
}
