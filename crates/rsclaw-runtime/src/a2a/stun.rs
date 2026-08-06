//! Minimal STUN (RFC 5389) client for gathering server-reflexive
//! (srflx) NAT candidates during P2P hole-punch setup (ADR 0002).
//!
//! Sends a binding request over UDP to each configured STUN server,
//! parses the XOR-MAPPED-ADDRESS attribute from the response, and
//! returns the public (IP, port) pair.
//!
//! This avoids pulling in a WebRTC/ICE stack — the whole protocol
//! interaction is ~40 lines of UDP + wire parsing.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// STUN binding request magic cookie (RFC 5389 §6).
const MAGIC_COOKIE: u32 = 0x2112A442;
/// STUN message header size (20 bytes).
const HEADER_SIZE: usize = 20;
/// Binding request type (0x0001).
const BINDING_REQUEST: u16 = 0x0001;
/// XOR-MAPPED-ADDRESS attribute type (0x0020).
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Resolve a "stun:host:port" URL to a `SocketAddr`.
fn resolve_stun_addr(stun_url: &str) -> Option<SocketAddr> {
    let host_port = stun_url.strip_prefix("stun:")?;
    host_port.to_socket_addrs().ok()?.next()
}

/// Send a STUN binding request and extract the public (IP, port)
/// from the XOR-MAPPED-ADDRESS in the response.
///
/// Returns `None` on timeout, parse failure, or network error.
pub fn stun_binding(stun_url: &str, timeout: Duration) -> Option<(String, u16)> {
    let addr = resolve_stun_addr(stun_url)?;

    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(timeout)).ok()?;

    // Build binding request (RFC 5389 §6):
    //  0                   1                   2                   3
    //  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    // |         Message Type          |         Message Length        |
    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    // |                         Magic Cookie                          |
    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    // |                                                               |
    // |                     Transaction ID (96 bits)                  |
    // |                                                               |
    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    let mut request = [0u8; HEADER_SIZE];
    request[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    request[2..4].copy_from_slice(&0u16.to_be_bytes()); // length = 0 (no attributes)
    request[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    // Transaction ID: simple counter-based to avoid rand dep.
    request[8..12].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
    request[12..16].copy_from_slice(&0xCAFEBABEu32.to_be_bytes());
    request[16..20].copy_from_slice(&0x8BADF00Du32.to_be_bytes());

    socket.send_to(&request, addr).ok()?;

    let mut buf = [0u8; 256];
    let (n, _src) = socket.recv_from(&mut buf).ok()?;

    parse_xor_mapped_address(&buf[..n])
}

/// Parse XOR-MAPPED-ADDRESS from a STUN response.
fn parse_xor_mapped_address(buf: &[u8]) -> Option<(String, u16)> {
    if buf.len() < HEADER_SIZE {
        return None;
    }
    // Verify magic cookie in response.
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if cookie != MAGIC_COOKIE {
        return None;
    }
    // Message length: total attribute bytes after the header.
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < HEADER_SIZE + msg_len {
        return None;
    }

    let attrs = &buf[HEADER_SIZE..HEADER_SIZE + msg_len];
    let mut pos = 0;
    while pos + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[pos], attrs[pos + 1]]);
        let attr_len = u16::from_be_bytes([attrs[pos + 2], attrs[pos + 3]]) as usize;
        pos += 4;
        if pos + attr_len > attrs.len() {
            break;
        }
        if attr_type == XOR_MAPPED_ADDRESS && attr_len >= 8 {
            let family = attrs[pos + 1];
            let x_port = u16::from_be_bytes([attrs[pos + 2], attrs[pos + 3]]);
            let port = x_port ^ (MAGIC_COOKIE >> 16) as u16;
            if family == 0x01 {
                // IPv4
                let x_addr = u32::from_be_bytes([
                    attrs[pos + 4],
                    attrs[pos + 5],
                    attrs[pos + 6],
                    attrs[pos + 7],
                ]);
                let ip = x_addr ^ MAGIC_COOKIE;
                let ip_str = format!(
                    "{}.{}.{}.{}",
                    (ip >> 24) as u8,
                    (ip >> 16) as u8,
                    (ip >> 8) as u8,
                    ip as u8,
                );
                return Some((ip_str, port));
            }
        }
        // Align to 4-byte boundary.
        pos += (attr_len + 3) & !3;
    }

    None
}

/// Gather srflx candidates from all configured STUN servers.
/// Returns a list of (public_ip, public_port) pairs.
pub fn gather_srflx_candidates(
    stun_urls: &[String],
    timeout: Duration,
) -> Vec<(String, u16)> {
    let mut results = Vec::new();
    for url in stun_urls {
        if let Some((ip, port)) = stun_binding(url, timeout) {
            results.push((ip, port));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_cookie_is_correct() {
        assert_eq!(MAGIC_COOKIE, 0x2112A442);
    }

    #[test]
    fn resolve_stun_addr_parses_host_port() {
        // google's well-known STUN
        let addr = resolve_stun_addr("stun:stun.l.google.com:19302");
        assert!(addr.is_some());
    }

    #[test]
    fn xor_mapped_address_ipv4() {
        // Construct a minimal STUN response with XOR-MAPPED-ADDRESS for IPv4.
        // XOR-MAPPED-ADDRESS value: 0x00 0x01 (family=IPv4)
        //    x_port ^ (MAGIC>>16) = port
        //    x_addr ^ MAGIC = ip
        let ip: u32 = 0xC0A80101; // 192.168.1.1
        let port: u16 = 12345;

        let x_port = port ^ (MAGIC_COOKIE >> 16) as u16;
        let x_addr = ip ^ MAGIC_COOKIE;

        let attr_len: u16 = 8; // IPv4 = 8 bytes
        let mut buf = vec![0u8; HEADER_SIZE + 4 + attr_len as usize];
        buf[0..2].copy_from_slice(&0x0101u16.to_be_bytes()); // Binding Success
        buf[2..4].copy_from_slice(&(attr_len + 4).to_be_bytes()); // msg len
        buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());

        let attr_start = HEADER_SIZE;
        buf[attr_start..attr_start + 2]
            .copy_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        buf[attr_start + 2..attr_start + 4]
            .copy_from_slice(&attr_len.to_be_bytes());
        buf[attr_start + 4] = 0x00; // reserved
        buf[attr_start + 5] = 0x01; // IPv4
        buf[attr_start + 6..attr_start + 8]
            .copy_from_slice(&x_port.to_be_bytes());
        buf[attr_start + 8..attr_start + 12]
            .copy_from_slice(&x_addr.to_be_bytes());

        let (result_ip, result_port) = parse_xor_mapped_address(&buf).unwrap();
        assert_eq!(result_ip, "192.168.1.1");
        assert_eq!(result_port, 12345);
    }
}
