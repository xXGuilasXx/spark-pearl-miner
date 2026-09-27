//! Who is on the other end of a loopback TCP connection.
//!
//! For a peer on `127.0.0.0/8` or `::1` the kernel lists the peer's own socket in `/proc/net/tcp`
//! (IPv4) or `/proc/net/tcp6` (IPv6, including v4-mapped addresses), with the UID that owns it.
//! The row we want is the one whose local address is the peer's `ip:port` and whose remote address
//! is our listener. That UID, compared with the daemon's own effective UID, tells "the same user
//! account on this machine" apart from other accounts on a multi-user box.
//!
//! The parsers are pure functions over the file text so they can be tested with sample lines.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Resolves the UID owning the peer socket of a connection: `(peer, local)` → UID.
pub type PeerUidResolver = Box<dyn Fn(SocketAddr, SocketAddr) -> Option<u32> + Send + Sync>;

/// TCP state `TIME_WAIT` in `/proc/net/tcp*` (its UID is meaningless).
const TCP_TIME_WAIT: &str = "06";

/// A loopback address, also when written as an IPv4-mapped IPv6 address (`::ffff:127.x.x.x`).
pub fn is_loopback(ip: IpAddr) -> bool {
    match normalize(ip) {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// `::ffff:a.b.c.d` → `a.b.c.d`; everything else unchanged.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// One `ADDR:PORT` field: 8 hex chars (IPv4, the 4 bytes in the kernel's native order) or 32 hex
/// chars (IPv6, four native-order 32-bit words), a colon, and the port as 4 hex chars (big-endian).
pub fn parse_proc_addr(field: &str) -> Option<SocketAddr> {
    let (addr, port) = field.split_once(':')?;
    if port.len() != 4 {
        return None;
    }
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |s: &str| u32::from_str_radix(s, 16).ok().map(u32::to_ne_bytes);
    let ip = match addr.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(addr.get(i * 8..i * 8 + 8)?)?);
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// One data row: `(local, remote, state, uid)`. The header row and malformed rows give `None`.
/// Tokens: `sl`, local, remote, `st`, `tx_queue:rx_queue`, `tr:tm->when`, `retrnsmt`, `uid`, …
pub fn parse_proc_row(line: &str) -> Option<(SocketAddr, SocketAddr, &str, u32)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.len() < 8 || !t[0].ends_with(':') {
        return None;
    }
    let local = parse_proc_addr(t[1])?;
    let remote = parse_proc_addr(t[2])?;
    let st = t[3];
    if st.len() != 2 || !st.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let uid = t[7].parse().ok()?;
    // Timewait and orphan mini-sockets (also seen as FIN_WAIT2) carry inode 0 and uid 0.
    if t.get(9).is_some_and(|inode| *inode == "0") {
        return None;
    }
    Some((local, remote, st, uid))
}

fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && normalize(a.ip()) == normalize(b.ip())
}

/// The UID owning the socket `peer` → `local` in the text of `/proc/net/tcp` or `/proc/net/tcp6`.
/// Rows in `TIME_WAIT` and rows that do not parse are ignored.
pub fn find_peer_uid(table: &str, peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    table.lines().filter_map(parse_proc_row).find_map(|(l, r, st, uid)| {
        (st != TCP_TIME_WAIT && same(l, peer) && same(r, local)).then_some(uid)
    })
}

/// The default resolver: reads `/proc/net/tcp` and `/proc/net/tcp6`. Only loopback peers are
/// looked up; anything else is `None`.
pub fn proc_peer_uid(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    if !is_loopback(peer.ip()) {
        return None;
    }
    let files: &[&str] = match normalize(peer.ip()) {
        IpAddr::V4(_) => &["/proc/net/tcp", "/proc/net/tcp6"],
        IpAddr::V6(_) => &["/proc/net/tcp6"],
    };
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .find_map(|text| find_peer_uid(&text, peer, local))
}

/// The effective UID in the text of `/proc/<pid>/status` (`Uid: real effective saved fs`).
pub fn parse_status_euid(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// This process's effective UID (from `/proc/self/status`; no libc call needed).
pub fn effective_uid() -> Option<u32> {
    parse_status_euid(&std::fs::read_to_string("/proc/self/status").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0FEE 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 75255510 1 0000000000000000 100 0 0 10 0
   1: 0100007F:D431 0100007F:0FEE 01 00000000:00000000 00:00000000 00000000  1000        0 75255511 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:0FEE 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 75255512 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:D432 0100007F:0FEE 01 00000000:00000000 00:00000000 00000000  1001        0 75255513 1 0000000000000000 20 4 30 10 -1
   4: 0100007F:D433 0100007F:0FEE 06 00000000:00000000 03:00001234 00000000     0        0 0 3 0000000000000000
   5: 0100007F:D434 0100007F:0FEF 01 00000000:00000000 00:00000000 00000000  1002        0 75255514 1 0000000000000000 20 4 30 10 -1
   6: garbage line with 0100007F:D435 0100007F:0FEE 01 x y z 1003
   7: 0100007F:D436 0100007F:0FEE 01 00000000:00000000 00:00000000 00000000  notanumber 0 1
   8: 0100007F:D437 0100007F:0FEE 05 00000000:00000000 00:00000000 00000000     0        0 0 3 0000000000000000
";

    const V6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0FEE 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:C350 00000000000000000000000001000000:0FEE 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 20 4 30 10 -1
   2: 0000000000000000FFFF00000100007F:C351 0000000000000000FFFF00000100007F:0FEE 01 00000000:00000000 00:00000000 00000000  1004        0 3 1 0000000000000000 20 4 30 10 -1
";

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn addresses_decode_in_kernel_order() {
        assert_eq!(parse_proc_addr("0100007F:0FEE"), Some(sa("127.0.0.1:4078")));
        assert_eq!(parse_proc_addr("00000000000000000000000001000000:0FEE"), Some(sa("[::1]:4078")));
        assert_eq!(parse_proc_addr("0000000000000000FFFF00000100007F:C351"), Some(sa("[::ffff:127.0.0.1]:50001")));
        assert_eq!(parse_proc_addr("0100007F"), None);
        assert_eq!(parse_proc_addr("0100007:0FEE"), None);
        assert_eq!(parse_proc_addr("0100007F:FEE"), None);
        assert_eq!(parse_proc_addr("ZZ00007F:0FEE"), None);
    }

    #[test]
    fn ipv4_row_gives_the_peer_uid() {
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54321"), sa("127.0.0.1:4078")), Some(1000));
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54322"), sa("127.0.0.1:4078")), Some(1001));
    }

    #[test]
    fn ipv6_loopback_row() {
        assert_eq!(find_peer_uid(V6, sa("[::1]:50000"), sa("[::1]:4078")), Some(1000));
    }

    #[test]
    fn v4_mapped_row() {
        assert_eq!(find_peer_uid(V6, sa("[::ffff:127.0.0.1]:50001"), sa("[::ffff:127.0.0.1]:4078")), Some(1004));
        // The same connection seen with plain IPv4 addresses still matches.
        assert_eq!(find_peer_uid(V6, sa("127.0.0.1:50001"), sa("127.0.0.1:4078")), Some(1004));
    }

    #[test]
    fn time_wait_is_ignored() {
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54323"), sa("127.0.0.1:4078")), None);
    }

    #[test]
    fn orphan_rows_with_inode_zero_are_ignored() {
        // Row 8: FIN_WAIT2 mini-socket, uid 0, inode 0 (a closed client whose port could be reused).
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54327"), sa("127.0.0.1:4078")), None);
    }

    #[test]
    fn malformed_rows_are_ignored() {
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54325"), sa("127.0.0.1:4078")), None);
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54326"), sa("127.0.0.1:4078")), None);
        assert_eq!(parse_proc_row("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid"), None);
        assert_eq!(find_peer_uid("", sa("127.0.0.1:54321"), sa("127.0.0.1:4078")), None);
    }

    #[test]
    fn wrong_remote_address_is_ignored() {
        // Row 5 is connected to port 4079, not to our listener.
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54324"), sa("127.0.0.1:4078")), None);
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54324"), sa("127.0.0.1:4079")), Some(1002));
        assert_eq!(find_peer_uid(V4, sa("127.0.0.1:54321"), sa("127.0.0.2:4078")), None);
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1".parse().unwrap()));
        assert!(is_loopback("127.3.2.1".parse().unwrap()));
        assert!(is_loopback("::1".parse().unwrap()));
        assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_loopback("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!is_loopback("100.64.0.1".parse().unwrap()));
        assert_eq!(proc_peer_uid(sa("100.64.0.1:5000"), sa("127.0.0.1:4078")), None);
    }

    #[test]
    fn effective_uid_from_status() {
        assert_eq!(parse_status_euid("Name:\tx\nUid:\t1000\t1001\t1002\t1003\nGid:\t5\n"), Some(1001));
        assert_eq!(parse_status_euid("Name:\tx\n"), None);
        assert!(effective_uid().is_some());
    }
}
