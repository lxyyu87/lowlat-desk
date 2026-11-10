use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_proto::InputEvent;
use snow::StatelessTransportState;

use crate::adapt::BitrateControl;
use crate::crypto::{self, NoiseSide};
use crate::reliable::{ReliableEndpoint, ReliableMsg};
use crate::stun::first_mapped;
use crate::wire::{
    encode_message, parse_udp, read_message_from, udp_bind, udp_punch, udp_relay, udp_secure,
    SigMsg, UdpPacket, UDP_MAGIC,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

pub struct SessionParams {
    pub signal: SocketAddr,
    pub id: String,
    pub password: String,
    pub role: Role,
    pub force_relay: bool,
    pub stun: Vec<String>,
}

pub struct PollResult {
    pub video: Vec<Vec<u8>>,
    pub inputs: Vec<InputEvent>,
    pub bitrate: u32,
}

/// An encrypted peer path. Video datagrams are not retransmitted. Input and
/// stats are.
pub struct SecureLink {
    udp: UdpSocket,
    /// Kept open so the signal server does not drop the registration.
    tcp: TcpStream,
    tcp_buf: Vec<u8>,
    signal: SocketAddr,
    dest: SocketAddr,
    relay: bool,
    token: [u8; 16],
    crypto: StatelessTransportState,
    nonce: u64,
    seen: BTreeSet<u64>,
    reliable: ReliableEndpoint,
    bitrate: BitrateControl,
    last_ping: Instant,
    ping_token: u64,
    ping_at: Option<(u64, Instant)>,
    rtt_ms: u32,
}

pub fn establish(params: &SessionParams) -> Result<SecureLink> {
    let started = Instant::now();
    loop {
        match establish_once(params) {
            Ok(link) => return Ok(link),
            Err(err)
                if params.role == Role::Client
                    && started.elapsed() < Duration::from_secs(20)
                    && format!("{err:#}").contains("offline") =>
            {
                tracing::info!("host is offline, retrying");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(err) => return Err(err),
        }
    }
}

fn establish_once(params: &SessionParams) -> Result<SecureLink> {
    let tcp = TcpStream::connect(params.signal).context("connect signal server")?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut conn = Conn {
        tcp,
        buf: Vec::new(),
    };
    match params.role {
        Role::Host => {
            conn.send(&SigMsg::Register {
                id: params.id.clone(),
                verifier: crypto::pass_verifier(&params.password),
            })?;
            match conn.recv()? {
                SigMsg::RegisterOk { .. } => {}
                SigMsg::Error(text) => bail!(text),
                other => bail!("unexpected register reply {other:?}"),
            }
        }
        Role::Client => {
            conn.send(&SigMsg::Connect {
                id: params.id.clone(),
            })?;
        }
    };
    let (token, peer_token) = match params.role {
        Role::Host => match conn.recv()? {
            SigMsg::Intro { token, peer_token } => (token, peer_token),
            SigMsg::Error(text) => bail!(text),
            other => bail!("unexpected intro {other:?}"),
        },
        Role::Client => match conn.recv()? {
            SigMsg::Intro { token, peer_token } => (token, peer_token),
            SigMsg::Error(text) => bail!(text),
            other => bail!("unexpected intro {other:?}"),
        },
    };
    let crypto = noise_over_tcp(&mut conn, &params.password, params.role)?;
    let udp = UdpSocket::bind("0.0.0.0:0").context("bind session udp")?;
    udp.set_read_timeout(Some(Duration::from_millis(300)))?;
    let observed = exchange_bind(&udp, params.signal, &token)?;
    let mut local_candidates = Vec::new();
    if let Ok(local) = udp.local_addr() {
        if !local.ip().is_unspecified() {
            local_candidates.push(local);
        }
    }
    if let Some(addr) = observed {
        local_candidates.push(addr);
        tracing::info!(%addr, "signal server sees our udp address");
    }
    if let Some(addr) = first_mapped(&udp, &params.stun) {
        local_candidates.push(addr);
    }
    for addr in &local_candidates {
        conn.send(&SigMsg::Candidate(*addr))?;
    }
    conn.send(&SigMsg::CandidatesDone)?;
    if params.force_relay {
        conn.send(&SigMsg::UseRelay)?;
    }

    let mut remote = Vec::new();
    let mut saw_relay = params.force_relay;
    let mut relay_ready = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    conn.tcp
        .set_read_timeout(Some(Duration::from_millis(200)))?;
    while Instant::now() < deadline && !relay_ready {
        match conn.recv() {
            Ok(SigMsg::Candidate(addr)) => remote.push(addr),
            Ok(SigMsg::RelayOn) => {
                saw_relay = true;
                relay_ready = true;
            }
            Ok(SigMsg::UseRelay) => saw_relay = true,
            Ok(_) => {}
            Err(_) => {
                if !params.force_relay && !remote.is_empty() {
                    break;
                }
            }
        }
    }

    let (dest, relay) = if saw_relay {
        if !relay_ready {
            conn.send(&SigMsg::UseRelay)?;
            let ack_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < ack_deadline && !relay_ready {
                match conn.recv() {
                    Ok(SigMsg::RelayOn) => relay_ready = true,
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
        (params.signal, true)
    } else if let Some(peer) = punch(&udp, &remote, &peer_token, &token) {
        tracing::info!(%peer, "udp hole punch succeeded");
        let _ = conn.send(&SigMsg::PunchOk);
        (peer, false)
    } else {
        tracing::info!("hole punch failed, relaying through the signal server");
        conn.send(&SigMsg::UseRelay)?;
        (params.signal, true)
    };
    udp.set_nonblocking(true)?;
    conn.tcp.set_nonblocking(true)?;
    Ok(SecureLink {
        udp,
        tcp: conn.tcp,
        tcp_buf: conn.buf,
        signal: params.signal,
        dest,
        relay,
        token,
        crypto,
        nonce: 1,
        seen: BTreeSet::new(),
        reliable: ReliableEndpoint::new(),
        bitrate: BitrateControl::new(8_000_000),
        last_ping: Instant::now(),
        ping_token: 1,
        ping_at: None,
        rtt_ms: 0,
    })
}

fn noise_over_tcp(conn: &mut Conn, password: &str, role: Role) -> Result<StatelessTransportState> {
    match role {
        Role::Client => {
            let mut side = NoiseSide::initiator(password)?;
            conn.send(&SigMsg::Noise(side.write()?))?;
            match conn.recv()? {
                SigMsg::Noise(reply) => side.read(&reply)?,
                SigMsg::Error(text) => bail!(text),
                other => bail!("expected noise reply, got {other:?}"),
            }
            side.finish()
        }
        Role::Host => {
            let msg = match conn.recv()? {
                SigMsg::Noise(msg) => msg,
                SigMsg::Error(text) => bail!(text),
                other => bail!("expected noise message, got {other:?}"),
            };
            let mut side = NoiseSide::responder(password)?;
            side.read(&msg)?;
            conn.send(&SigMsg::Noise(side.write()?))?;
            side.finish()
        }
    }
}

fn exchange_bind(
    udp: &UdpSocket,
    signal: SocketAddr,
    token: &[u8; 16],
) -> Result<Option<SocketAddr>> {
    let mut buf = [0u8; 256];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        udp.send_to(&udp_bind(token), signal)?;
        if let Ok((n, _)) = udp.recv_from(&mut buf) {
            if let Some(UdpPacket::BindAck { observed, .. }) = parse_udp(&buf[..n]) {
                tracing::info!(%observed, "received udp bind ack");
                return Ok(Some(observed));
            }
        }
    }
    Ok(None)
}

fn punch(
    udp: &UdpSocket,
    candidates: &[SocketAddr],
    peer_token: &[u8; 16],
    my_token: &[u8; 16],
) -> Option<SocketAddr> {
    if candidates.is_empty() {
        return None;
    }
    let packet = udp_punch(peer_token);
    let start = Instant::now();
    let mut buf = [0u8; 1500];
    while start.elapsed() < Duration::from_millis(1200) {
        for candidate in candidates {
            let _ = udp.send_to(&packet, candidate);
        }
        if let Ok((n, from)) = udp.recv_from(&mut buf) {
            match parse_udp(&buf[..n]) {
                Some(UdpPacket::Punch { token }) if token == *my_token => {
                    let _ = udp.send_to(&packet, from);
                    return Some(from);
                }
                Some(UdpPacket::Secure { .. }) => return Some(from),
                _ => {}
            }
        }
    }
    None
}

impl SecureLink {
    pub fn set_max_bitrate(&mut self, bps: u32) {
        self.bitrate = BitrateControl::new(bps);
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate.target()
    }

    pub fn send_video(&mut self, plain: &[u8]) -> Result<()> {
        self.send_channel(0, plain)
    }

    pub fn send_inputs(&mut self, events: &[InputEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let packet = self
            .reliable
            .send(&ReliableMsg::Inputs(events.to_vec()), Instant::now());
        self.send_channel(1, &packet)
    }

    pub fn send_stats(&mut self, loss_permille: u16, rtt_ms: u16) -> Result<()> {
        let packet = self.reliable.send(
            &ReliableMsg::Stats {
                loss_permille,
                rtt_ms,
            },
            Instant::now(),
        );
        self.send_channel(1, &packet)
    }

    pub fn poll(&mut self) -> Result<PollResult> {
        self.poll_signal()?;
        let now = Instant::now();
        for packet in self.reliable.retransmit(now) {
            self.send_channel(1, &packet)?;
        }
        if self.ping_at.is_none() && self.last_ping.elapsed() >= Duration::from_secs(1) {
            let token = self.ping_token;
            self.ping_token = self.ping_token.wrapping_add(1);
            let packet = self.reliable.send(&ReliableMsg::Ping(token), now);
            self.send_channel(1, &packet)?;
            self.ping_at = Some((token, now));
            self.last_ping = now;
        }
        let mut video = Vec::new();
        let mut inputs = Vec::new();
        let mut buf = [0u8; 2048];
        for _ in 0..64 {
            let n = match self.udp.recv_from(&mut buf) {
                Ok((n, _)) => n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err).context("session recv"),
            };
            if let Some(plain) = self.unwrap_datagram(&buf[..n])? {
                self.dispatch_plain(&plain, &mut video, &mut inputs)?;
            }
        }
        Ok(PollResult {
            video,
            inputs,
            bitrate: self.bitrate.target(),
        })
    }

    fn poll_signal(&mut self) -> Result<()> {
        let mut tmp = [0u8; 2048];
        loop {
            match self.tcp.read(&mut tmp) {
                Ok(0) => bail!("signal server closed the connection"),
                Ok(n) => self.tcp_buf.extend_from_slice(&tmp[..n]),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(err) => return Err(err).context("read signal socket"),
            }
        }
        if self.tcp_buf.len() > 1024 * 1024 {
            bail!("signal connection overflowed");
        }
        while let Some((msg, n)) = read_message_from(&self.tcp_buf)? {
            self.tcp_buf.drain(..n);
            if let SigMsg::Error(text) = msg {
                bail!(text);
            }
        }
        Ok(())
    }

    fn dispatch_plain(
        &mut self,
        plain: &[u8],
        video: &mut Vec<Vec<u8>>,
        inputs: &mut Vec<InputEvent>,
    ) -> Result<()> {
        if plain.is_empty() {
            return Ok(());
        }
        match plain[0] {
            0 => video.push(plain[1..].to_vec()),
            1 => {
                let (msgs, ack) = self.reliable.handle(&plain[1..])?;
                if let Some(ack) = ack {
                    self.send_channel(1, &ack)?;
                }
                for msg in msgs {
                    match msg {
                        ReliableMsg::Inputs(events) => inputs.extend(events),
                        ReliableMsg::Stats {
                            loss_permille,
                            rtt_ms,
                        } => {
                            let rtt = self.rtt_ms.max(u32::from(rtt_ms));
                            self.bitrate.observe(u32::from(loss_permille), rtt);
                        }
                        ReliableMsg::Ping(token) => {
                            let packet = self
                                .reliable
                                .send(&ReliableMsg::Pong(token), Instant::now());
                            self.send_channel(1, &packet)?;
                        }
                        ReliableMsg::Pong(token) => {
                            if let Some((sent, at)) = self.ping_at {
                                if sent == token {
                                    self.rtt_ms = at.elapsed().as_millis().min(10_000) as u32;
                                    self.ping_at = None;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn unwrap_datagram(&mut self, packet: &[u8]) -> Result<Option<Vec<u8>>> {
        let secure = match parse_udp(packet) {
            Some(UdpPacket::Relay { inner, .. }) => parse_udp(&inner),
            other => other,
        };
        let Some(UdpPacket::Secure { nonce, ciphertext }) = secure else {
            return Ok(None);
        };
        if !self.seen.insert(nonce) {
            return Ok(None);
        }
        if self.seen.len() > 4096 {
            let floor = nonce.saturating_sub(2048);
            self.seen.retain(|seen| *seen >= floor);
        }
        match crypto::open(&mut self.crypto, nonce, &ciphertext) {
            Ok(plain) => Ok(Some(plain)),
            Err(_) => Ok(None),
        }
    }

    fn send_channel(&mut self, channel: u8, plain: &[u8]) -> Result<()> {
        let mut body = Vec::with_capacity(1 + plain.len());
        body.push(channel);
        body.extend_from_slice(plain);
        let nonce = self.nonce;
        self.nonce = self.nonce.wrapping_add(1);
        let ciphertext = crypto::seal(&mut self.crypto, nonce, &body)?;
        let secure = udp_secure(nonce, &ciphertext);
        let packet = if self.relay {
            udp_relay(&self.token, &secure)
        } else {
            secure
        };
        let dest = if self.relay { self.signal } else { self.dest };
        self.udp.send_to(&packet, dest)?;
        let _ = UDP_MAGIC;
        Ok(())
    }
}

struct Conn {
    tcp: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    fn send(&mut self, msg: &SigMsg) -> Result<()> {
        self.tcp.write_all(&encode_message(msg))?;
        Ok(())
    }

    fn recv(&mut self) -> Result<SigMsg> {
        loop {
            if let Some((msg, n)) = read_message_from(&self.buf)? {
                self.buf.drain(..n);
                return Ok(msg);
            }
            let mut tmp = [0u8; 4096];
            let n = self.tcp.read(&mut tmp).context("read signal socket")?;
            if n == 0 {
                bail!("signal server closed the connection");
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}
