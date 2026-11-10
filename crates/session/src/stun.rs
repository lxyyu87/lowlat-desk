use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Public STUN servers. Xiaomi and Bilibili are reachable from mainland China
/// more often than Google's STUN. Any of them can be replaced on the command line,
/// including a STUN server you run yourself.
pub const DEFAULT_STUN: &[&str] = &[
    "stun.miwifi.com:3478",
    "stun.chat.bilibili.com:3478",
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
];

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const XOR_MAPPED: u16 = 0x0020;

pub fn encode_binding_request(tx_id: &[u8; 12]) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    packet[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    packet[8..20].copy_from_slice(tx_id);
    packet
}

pub fn decode_binding_response(packet: &[u8], tx_id: &[u8; 12]) -> Option<SocketAddr> {
    if packet.len() < 20 {
        return None;
    }
    let kind = u16::from_be_bytes(packet[0..2].try_into().ok()?);
    if kind != BINDING_RESPONSE || packet[4..8] != MAGIC.to_be_bytes() || packet[8..20] != *tx_id {
        return None;
    }
    let mut offset = 20;
    while offset + 4 <= packet.len() {
        let attr = u16::from_be_bytes(packet[offset..offset + 2].try_into().ok()?);
        let len = u16::from_be_bytes(packet[offset + 2..offset + 4].try_into().ok()?) as usize;
        offset += 4;
        if offset + len > packet.len() {
            return None;
        }
        if attr == XOR_MAPPED && len >= 8 {
            let family = packet[offset + 1];
            let xport = u16::from_be_bytes(packet[offset + 2..offset + 4].try_into().ok()?);
            let port = xport ^ (MAGIC >> 16) as u16;
            if family == 0x01 && len >= 8 {
                let raw = u32::from_be_bytes(packet[offset + 4..offset + 8].try_into().ok()?);
                let ip = Ipv4Addr::from(raw ^ MAGIC);
                return Some(SocketAddr::new(IpAddr::V4(ip), port));
            }
            if family == 0x02 && len >= 20 {
                let mut addr = [0u8; 16];
                addr.copy_from_slice(&packet[offset + 4..offset + 20]);
                let magic_bytes = MAGIC.to_be_bytes();
                for (index, byte) in addr.iter_mut().enumerate() {
                    let mask = if index < 4 {
                        magic_bytes[index]
                    } else {
                        tx_id[index - 4]
                    };
                    *byte ^= mask;
                }
                return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(addr)), port));
            }
        }
        offset += len + (4 - len % 4) % 4;
    }
    None
}

pub fn query_stun(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let server: SocketAddr = server
        .parse()
        .with_context(|| format!("stun address {server}"))?;
    let tx_id = rand_id();
    socket
        .send_to(&encode_binding_request(&tx_id), server)
        .context("stun send")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(400)))
        .ok();
    let mut buf = [0u8; 512];
    let (n, _) = socket.recv_from(&mut buf).context("stun recv")?;
    decode_binding_response(&buf[..n], &tx_id).context("stun response had no mapped address")
}

fn rand_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(1);
    id.copy_from_slice(&now.to_le_bytes()[..12]);
    id
}

pub fn first_mapped(socket: &UdpSocket, servers: &[String]) -> Option<SocketAddr> {
    for server in servers {
        match query_stun(socket, server) {
            Ok(addr) => {
                tracing::info!(server, %addr, "stun mapped address");
                return Some(addr);
            }
            Err(err) => tracing::debug!(server, error = %err, "stun query failed"),
        }
    }
    None
}

/// Command-line STUN list. Empty uses [`DEFAULT_STUN`]. `none` or `off` disables STUN.
pub fn stun_from_args(list: &[String]) -> Vec<String> {
    if list.iter().any(|item| item == "none" || item == "off") {
        return Vec::new();
    }
    if list.is_empty() {
        return DEFAULT_STUN
            .iter()
            .map(|server| (*server).to_string())
            .collect();
    }
    list.iter()
        .flat_map(|item| parse_server_list(item).unwrap_or_default())
        .collect()
}

pub fn parse_server_list(text: &str) -> Result<Vec<String>> {
    let list: Vec<String> = text
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    if list.is_empty() {
        bail!("stun server list is empty");
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_an_xor_mapped_ipv4_response() {
        let tx_id = [9u8; 12];
        let ip = Ipv4Addr::new(203, 0, 113, 8);
        let port = 40000u16;
        let xport = port ^ (MAGIC >> 16) as u16;
        let xip = u32::from(ip) ^ MAGIC;
        let mut packet = vec![0u8; 32];
        packet[0..2].copy_from_slice(&BINDING_RESPONSE.to_be_bytes());
        packet[2..4].copy_from_slice(&12u16.to_be_bytes());
        packet[4..8].copy_from_slice(&MAGIC.to_be_bytes());
        packet[8..20].copy_from_slice(&tx_id);
        packet[20..22].copy_from_slice(&XOR_MAPPED.to_be_bytes());
        packet[22..24].copy_from_slice(&8u16.to_be_bytes());
        packet[25] = 0x01;
        packet[26..28].copy_from_slice(&xport.to_be_bytes());
        packet[28..32].copy_from_slice(&xip.to_be_bytes());
        let mapped = decode_binding_response(&packet, &tx_id).unwrap();
        assert_eq!(mapped, SocketAddr::new(IpAddr::V4(ip), port));
    }
}
