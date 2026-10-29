//! Find `name.local` hosts (Umbrel, Start9…) with a one-shot mDNS query,
//! for systems whose resolver doesn't (many Android phones).
//!
//! The query is sent from an ordinary port, so responders answer it by
//! unicast straight back to us (RFC 6762 §6.7, "legacy unicast"): no
//! multicast group to join, no special permission.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const MDNS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);

/// IPv4 addresses for `host` (which must end in `.local`), or empty.
pub fn resolve(host: &str, timeout: Duration) -> Vec<Ipv4Addr> {
    let host = host.trim_end_matches('.');
    if !host.to_ascii_lowercase().ends_with(".local") {
        return Vec::new();
    }
    let Some(query) = query(host) else {
        return Vec::new();
    };
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let _ = sock.set_multicast_ttl_v4(255);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    // Ask twice: the first packet out on a phone sometimes goes nowhere.
    for attempt in 0..2 {
        if sock.send_to(&query, SocketAddr::V4(MDNS)).is_err() {
            return Vec::new();
        }
        let round_end = if attempt == 0 {
            Instant::now() + timeout / 3
        } else {
            deadline
        };
        loop {
            let left = round_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let _ = sock.set_read_timeout(Some(left));
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let found = answers(&buf[..n], host);
                    if !found.is_empty() {
                        return found;
                    }
                }
                Err(_) => break,
            }
        }
    }
    Vec::new()
}

/// A standard query for `host`'s A record (id 0, one question).
fn query(host: &str) -> Option<Vec<u8>> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0, 1, 0, 1]); // A, IN
    Some(q)
}

/// Read a (possibly compressed) name at `pos`; returns it and the position after it.
fn name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut out = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            return Some((out.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *msg.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        let label = msg.get(pos + 1..pos + 1 + len)?;
        out.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        pos += 1 + len;
    }
    None
}

/// A records for `host` in a response (answers and additional records).
fn answers(msg: &[u8], host: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if msg.len() < 12 || msg[2] & 0x80 == 0 {
        return out; // not a response
    }
    let count = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]) as usize;
    let (qd, records) = (count(4), count(6) + count(8) + count(10));
    let mut pos = 12;
    for _ in 0..qd {
        let Some((_, p)) = name(msg, pos) else { return out };
        pos = p + 4;
    }
    let want = host.to_ascii_lowercase();
    for _ in 0..records {
        let Some((n, p)) = name(msg, pos) else { return out };
        let Some(head) = msg.get(p..p + 10) else { return out };
        let rtype = u16::from_be_bytes([head[0], head[1]]);
        let len = u16::from_be_bytes([head[8], head[9]]) as usize;
        let Some(data) = msg.get(p + 10..p + 10 + len) else { return out };
        if rtype == 1 && len == 4 && n == want {
            out.push(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        }
        pos = p + 10 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_answer_with_a_compressed_name() {
        let q = query("umbrel.local").unwrap();
        // A response: the question echoed, then an answer pointing back at it.
        let mut r = q.clone();
        r[2] = 0x84; // response, authoritative
        r[7] = 1; // one answer
        r.extend_from_slice(&[0xC0, 12, 0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 192, 168, 1, 20]);
        assert_eq!(answers(&r, "umbrel.local"), vec![Ipv4Addr::new(192, 168, 1, 20)]);
        assert!(answers(&r, "other.local").is_empty());
        assert!(answers(&q, "umbrel.local").is_empty(), "a query isn't an answer");
        assert!(resolve("example.com", Duration::from_millis(10)).is_empty());
    }
}
