use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};

use anyhow::{Context, Result};
use rd_proto::{read_input_message, InputEvent, InputMessage, MAX_DATAGRAM};
use rd_session::{establish, SecureLink, SessionParams};

/// Stand-in address for the single internet peer. Datagrams have no UDP source.
pub fn internet_peer() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 1))
}

pub struct Transport {
    inner: Inner,
    bitrate: u32,
    inputs: Vec<InputEvent>,
}

struct LanState {
    socket: UdpSocket,
    listener: TcpListener,
    input: Option<TcpStream>,
    buf: Vec<u8>,
}

enum Inner {
    Lan(Box<LanState>),
    Internet(Box<SecureLink>),
}

impl Transport {
    pub fn lan(listen: SocketAddr, bitrate: u32) -> Result<Self> {
        let socket = UdpSocket::bind(listen).with_context(|| format!("bind udp {listen}"))?;
        socket.set_nonblocking(true)?;
        let listener = TcpListener::bind(listen).with_context(|| format!("bind tcp {listen}"))?;
        listener.set_nonblocking(true)?;
        tracing::info!(%listen, "listening for video (udp) and input (tcp)");
        Ok(Self {
            inner: Inner::Lan(Box::new(LanState {
                socket,
                listener,
                input: None,
                buf: Vec::new(),
            })),
            bitrate,
            inputs: Vec::new(),
        })
    }

    pub fn internet(params: &SessionParams, bitrate: u32) -> Result<Self> {
        let mut link = establish(params)?;
        link.set_max_bitrate(bitrate);
        let bitrate = link.bitrate();
        tracing::info!("encrypted session is up");
        Ok(Self {
            inner: Inner::Internet(Box::new(link)),
            bitrate,
            inputs: Vec::new(),
        })
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub fn is_internet(&self) -> bool {
        matches!(self.inner, Inner::Internet(_))
    }

    #[cfg(test)]
    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match &self.inner {
            Inner::Lan(lan) => lan.listener.local_addr().ok(),
            Inner::Internet(_) => None,
        }
    }

    pub fn pump(&mut self) -> Result<Vec<(Vec<u8>, SocketAddr)>> {
        match &mut self.inner {
            Inner::Lan(lan) => {
                let mut packets = Vec::new();
                let mut scratch = [0u8; MAX_DATAGRAM + 64];
                loop {
                    match lan.socket.recv_from(&mut scratch) {
                        Ok((n, src)) => packets.push((scratch[..n].to_vec(), src)),
                        Err(err) if would_block(&err) => break,
                        Err(err) => return Err(err).context("recv video"),
                    }
                }
                accept_input(&lan.listener, &mut lan.input);
                let mut chunk = Vec::new();
                let mut drop_input = false;
                if let Some(stream) = lan.input.as_mut() {
                    let mut tmp = [0u8; 2048];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => {
                                tracing::info!("input channel closed");
                                drop_input = true;
                                break;
                            }
                            Ok(n) => chunk.extend_from_slice(&tmp[..n]),
                            Err(err) if would_block(&err) => break,
                            Err(err) => {
                                tracing::warn!(error = %err, "input channel failed");
                                drop_input = true;
                                break;
                            }
                        }
                    }
                }
                if drop_input {
                    lan.input = None;
                    lan.buf.clear();
                } else {
                    lan.buf.extend_from_slice(&chunk);
                }
                decode_inputs(&mut lan.buf, &mut self.inputs)?;
                Ok(packets)
            }
            Inner::Internet(link) => {
                let polled = link.poll().context("session poll")?;
                self.bitrate = polled.bitrate;
                self.inputs.extend(polled.inputs);
                let peer = internet_peer();
                Ok(polled
                    .video
                    .into_iter()
                    .map(|packet| (packet, peer))
                    .collect())
            }
        }
    }

    pub fn send(&mut self, packet: &[u8], dest: SocketAddr) -> Result<()> {
        match &mut self.inner {
            Inner::Lan(lan) => {
                lan.socket.send_to(packet, dest).context("send video")?;
            }
            Inner::Internet(link) => {
                link.send_video(packet).context("send encrypted video")?;
            }
        }
        Ok(())
    }

    pub fn take_inputs(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.inputs)
    }
}

fn would_block(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut
}

fn accept_input(listener: &TcpListener, input: &mut Option<TcpStream>) {
    if input.is_some() {
        return;
    }
    match listener.accept() {
        Ok((stream, addr)) => {
            tracing::info!(%addr, "input channel connected");
            let _ = stream.set_nodelay(true);
            let _ = stream.set_nonblocking(true);
            *input = Some(stream);
        }
        Err(err) if would_block(&err) => {}
        Err(err) => tracing::warn!(error = %err, "accept input channel"),
    }
}

fn decode_inputs(buf: &mut Vec<u8>, inputs: &mut Vec<InputEvent>) -> Result<()> {
    if buf.len() > 1024 * 1024 {
        buf.clear();
        anyhow::bail!("input channel overflowed");
    }
    while let Some((message, n)) = read_input_message(buf)? {
        buf.drain(..n);
        if let InputMessage::Events(events) = message {
            inputs.extend(events);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::TcpStream;

    use rd_proto::{encode_input_message, InputEvent, InputMessage};

    use super::*;

    #[test]
    fn lan_tcp_delivers_mouse_input() {
        let mut transport = Transport::lan("127.0.0.1:0".parse().unwrap(), 1_000_000).unwrap();
        let addr = transport.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let bytes = encode_input_message(&InputMessage::Events(vec![InputEvent::MouseMove {
            x: 15,
            y: 25,
        }]));
        client.write_all(&bytes).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw = false;
        while std::time::Instant::now() < deadline && !saw {
            transport.pump().unwrap();
            saw = transport
                .take_inputs()
                .iter()
                .any(|event| event == &InputEvent::MouseMove { x: 15, y: 25 });
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw, "lan tcp should deliver the mouse move");
    }
}
