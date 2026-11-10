//! Rendezvous server: device registration, candidate exchange, and UDP relay.
//!
//! The server never sees the session password. Hosts register a verifier, and
//! the peers authenticate each other with a different key inside the Noise
//! handshake. When UDP hole punching fails, datagrams are forwarded between
//! the peers' UDP sockets.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rd_session::{encode_message, parse_udp, read_message_from, udp_bind_ack, SigMsg, UdpPacket};

struct Peer {
    id: String,
    host: bool,
    token: [u8; 16],
    partner: Option<[u8; 16]>,
    udp: Option<SocketAddr>,
    relay: bool,
    tcp: Arc<Mutex<TcpStream>>,
}

struct State {
    hosts: HashMap<String, [u8; 32]>,
    peers: HashMap<[u8; 16], Peer>,
}

pub struct SignalServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SignalServer {
    pub fn spawn(bind: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(bind).with_context(|| format!("bind tcp {bind}"))?;
        listener.set_nonblocking(true)?;
        let udp = UdpSocket::bind(listener.local_addr()?)?;
        udp.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            if let Err(err) = serve(listener, udp, flag) {
                tracing::error!(error = %err, "signal server stopped");
            }
        });
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for SignalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: TcpListener, udp: UdpSocket, stop: Arc<AtomicBool>) -> Result<()> {
    let state = Arc::new(Mutex::new(State {
        hosts: HashMap::new(),
        peers: HashMap::new(),
    }));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nodelay(true).ok();
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(err) = handle_tcp(stream, state) {
                        tracing::debug!(error = %err, "signal peer disconnected");
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err.into()),
        }
        poll_udp(&udp, &state)?;
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn handle_tcp(stream: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    // Read and write on separate clones so a blocking read does not hold the
    // write lock while another thread forwards a message.
    let mut reader = stream.try_clone()?;
    let tcp = Arc::new(Mutex::new(stream));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut token = None;
    loop {
        let n = reader.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = read_message_from(&buf)? {
            buf.drain(..consumed);
            token = Some(on_message(msg, token, Arc::clone(&tcp), &state)?);
        }
    }
    if let Some(token) = token {
        let partner_tcp = {
            let mut state = state.lock().expect("state");
            let Some(peer) = state.peers.remove(&token) else {
                return Ok(());
            };
            if peer.host {
                state.hosts.remove(&peer.id);
            }
            peer.partner.and_then(|partner| {
                let other = state.peers.get_mut(&partner)?;
                other.partner = None;
                Some(Arc::clone(&other.tcp))
            })
        };
        if let Some(tcp) = partner_tcp {
            let _ = send(&tcp, &SigMsg::Error("peer closed".into()));
        }
    }
    Ok(())
}

fn on_message(
    msg: SigMsg,
    token: Option<[u8; 16]>,
    tcp: Arc<Mutex<TcpStream>>,
    state: &Mutex<State>,
) -> Result<[u8; 16]> {
    match msg {
        SigMsg::Register { id, verifier } => {
            let mut state = state.lock().expect("state");
            if let Some(existing) = state.hosts.get(&id) {
                if existing != &verifier {
                    drop(state);
                    send(&tcp, &SigMsg::Error("bad password".into()))?;
                    bail!("bad password");
                }
            }
            state.hosts.insert(id.clone(), verifier);
            let stale: Vec<[u8; 16]> = state
                .peers
                .iter()
                .filter(|(_, peer)| peer.host && peer.id == id)
                .map(|(token, _)| *token)
                .collect();
            for token in stale {
                state.peers.remove(&token);
            }
            let token = fresh_token();
            state.peers.insert(
                token,
                Peer {
                    id,
                    host: true,
                    token,
                    partner: None,
                    udp: None,
                    relay: false,
                    tcp,
                },
            );
            let peer = state.peers.get(&token).expect("inserted");
            send(&peer.tcp, &SigMsg::RegisterOk { token })?;
            Ok(token)
        }
        SigMsg::Connect { id } => {
            let mut state = state.lock().expect("state");
            let host_token = state
                .peers
                .values()
                .find(|peer| peer.host && peer.id == id)
                .map(|peer| peer.token);
            let Some(host_token) = host_token else {
                drop(state);
                send(&tcp, &SigMsg::Error("offline".into()))?;
                bail!("offline");
            };
            let token = fresh_token();
            state.peers.insert(
                token,
                Peer {
                    id,
                    host: false,
                    token,
                    partner: Some(host_token),
                    udp: None,
                    relay: false,
                    tcp: Arc::clone(&tcp),
                },
            );
            if let Some(host) = state.peers.get_mut(&host_token) {
                host.partner = Some(token);
                host.relay = false;
            }
            let host_tcp = state.peers.get(&host_token).expect("host").tcp.clone();
            drop(state);
            send(
                &tcp,
                &SigMsg::Intro {
                    token,
                    peer_token: host_token,
                },
            )?;
            send(
                &host_tcp,
                &SigMsg::Intro {
                    token: host_token,
                    peer_token: token,
                },
            )?;
            Ok(token)
        }
        SigMsg::Noise(data) => {
            let token = token.context("noise before registration")?;
            relay_tcp(state, token, &SigMsg::Noise(data))?;
            Ok(token)
        }
        SigMsg::Candidate(addr) => {
            let token = token.context("candidate before registration")?;
            relay_tcp(state, token, &SigMsg::Candidate(addr))?;
            Ok(token)
        }
        SigMsg::CandidatesDone => {
            let token = token.context("candidates before registration")?;
            relay_tcp(state, token, &SigMsg::CandidatesDone)?;
            Ok(token)
        }
        SigMsg::UseRelay => {
            let token = token.context("relay before registration")?;
            let readers = {
                let mut state = state.lock().expect("state");
                let partner = state.peers.get(&token).and_then(|peer| peer.partner);
                if let Some(peer) = state.peers.get_mut(&token) {
                    peer.relay = true;
                }
                if let Some(partner) = partner {
                    if let Some(peer) = state.peers.get_mut(&partner) {
                        peer.relay = true;
                    }
                }
                let mut readers = vec![tcp];
                if let Some(partner) = state.peers.get(&token).and_then(|peer| peer.partner) {
                    if let Some(peer) = state.peers.get(&partner) {
                        readers.push(Arc::clone(&peer.tcp));
                    }
                }
                readers
            };
            for tcp in readers {
                send(&tcp, &SigMsg::RelayOn)?;
            }
            Ok(token)
        }
        SigMsg::PunchOk => Ok(token.unwrap_or([0; 16])),
        other => {
            let _ = other;
            Ok(token.unwrap_or([0; 16]))
        }
    }
}

fn relay_tcp(state: &Mutex<State>, from: [u8; 16], msg: &SigMsg) -> Result<()> {
    let dest = {
        let state = state.lock().expect("state");
        let Some(peer) = state.peers.get(&from) else {
            return Ok(());
        };
        let Some(partner) = peer.partner else {
            return Ok(());
        };
        state.peers.get(&partner).map(|peer| Arc::clone(&peer.tcp))
    };
    if let Some(dest) = dest {
        send(&dest, msg)?;
    }
    Ok(())
}

fn send(tcp: &Mutex<TcpStream>, msg: &SigMsg) -> Result<()> {
    tcp.lock().expect("tcp").write_all(&encode_message(msg))?;
    Ok(())
}

fn poll_udp(udp: &UdpSocket, state: &Mutex<State>) -> Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = match udp.recv_from(&mut buf) {
            Ok(packet) => packet,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let Some(packet) = parse_udp(&buf[..n]) else {
            continue;
        };
        match packet {
            UdpPacket::Bind { token } => {
                let ack = udp_bind_ack(&token, from);
                udp.send_to(&ack, from)?;
                let mut state = state.lock().expect("state");
                if let Some(peer) = state.peers.get_mut(&token) {
                    peer.udp = Some(from);
                }
            }
            UdpPacket::Relay { token, inner } => {
                let mut state = state.lock().expect("state");
                let Some(peer) = state.peers.get_mut(&token) else {
                    continue;
                };
                peer.udp = Some(from);
                if !peer.relay {
                    continue;
                }
                let Some(partner) = peer.partner else {
                    continue;
                };
                let Some(dest) = state.peers.get(&partner).and_then(|peer| peer.udp) else {
                    continue;
                };
                udp.send_to(&inner, dest)?;
            }
            _ => {}
        }
    }
}

fn fresh_token() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let mut token = [0u8; 16];
    token[..8].copy_from_slice(&n.to_le_bytes());
    token[8..].copy_from_slice(&nanos.to_le_bytes());
    token
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rd_proto::InputEvent;
    use rd_session::{establish, Role, SessionParams};

    use super::*;

    fn params(signal: SocketAddr, role: Role, force_relay: bool) -> SessionParams {
        SessionParams {
            signal,
            id: "test-desk".into(),
            password: "correct horse".into(),
            role,
            force_relay,
            stun: Vec::new(),
        }
    }

    #[test]
    fn forced_relay_carries_input_and_video() {
        let server = SignalServer::spawn("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr();
        let host = std::thread::spawn(move || establish(&params(addr, Role::Host, true)).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        let mut client = establish(&params(addr, Role::Client, true)).unwrap();
        let mut host = host.join().unwrap();
        client
            .send_inputs(&[InputEvent::MouseMove { x: 15, y: 25 }])
            .unwrap();
        client.send_video(b"rd-video-packet").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut saw_input = false;
        let mut saw_video = false;
        while std::time::Instant::now() < deadline && (!saw_input || !saw_video) {
            let polled = host.poll().unwrap();
            saw_input |= polled
                .inputs
                .iter()
                .any(|event| event == &InputEvent::MouseMove { x: 15, y: 25 });
            saw_video |= polled
                .video
                .iter()
                .any(|packet| packet == b"rd-video-packet");
            let _ = client.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_input, "relay should deliver mouse input");
        assert!(saw_video, "relay should deliver a video datagram");
    }

    #[test]
    fn localhost_punch_carries_video_without_relay() {
        let server = SignalServer::spawn("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr();
        let mut host_params = params(addr, Role::Host, false);
        host_params.stun.clear();
        let mut client_params = params(addr, Role::Client, false);
        client_params.stun.clear();
        let host = std::thread::spawn(move || establish(&host_params).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        let mut client = establish(&client_params).unwrap();
        let mut host = host.join().unwrap();
        client.send_video(b"punched").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut saw = false;
        while std::time::Instant::now() < deadline && !saw {
            saw = host
                .poll()
                .unwrap()
                .video
                .iter()
                .any(|packet| packet == b"punched");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw, "punched path should deliver video without the relay");
    }

    #[test]
    fn wrong_password_does_not_establish() {
        let server = SignalServer::spawn("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr();
        let host = std::thread::spawn(move || establish(&params(addr, Role::Host, true)));
        std::thread::sleep(Duration::from_millis(50));
        let mut client_params = params(addr, Role::Client, true);
        client_params.password = "wrong".into();
        let client = establish(&client_params);
        assert!(client.is_err(), "wrong password must fail the handshake");
        drop(host);
    }
}
