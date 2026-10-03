//! The sidecar's own last check before it connects. The daemon is the
//! authority on destinations; this only refuses addresses no egress decision
//! may ever reach, as defense in depth.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4_never(ip: Ipv4Addr) -> bool {
    ip.octets()[0] == 0
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
}

fn v6_embedded(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    let low = Ipv4Addr::new(
        (segments[6] >> 8) as u8,
        segments[6] as u8,
        (segments[7] >> 8) as u8,
        segments[7] as u8,
    );
    let mapped = segments[..5] == [0; 5] && segments[5] == 0xffff;
    let compatible = segments[..6] == [0; 6];
    let nat64 = segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
    (mapped || compatible || nat64).then_some(low)
}

/// Loopback, unspecified, link-local, multicast or broadcast, including the
/// IPv4-mapped, IPv4-compatible and NAT64 forms of those.
pub fn is_never(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => v4_never(ip),
        IpAddr::V6(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || v6_embedded(ip).is_some_and(v4_never)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_table() {
        for never in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.0.9",
            "169.254.169.254",
            "224.0.0.1",
            "239.1.1.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:0.0.0.0",
            "::7f00:1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(is_never(never.parse().unwrap()), "{never}");
        }
        for allowed in [
            "1.1.1.1",
            "10.0.0.1",
            "192.168.127.254",
            "100.64.0.1",
            "2606:4700::1",
            "fd00::1",
            "fec0::1",
            "::ffff:8.8.8.8",
            "::ffff:10.0.0.1",
            "64:ff9b::808:808",
        ] {
            assert!(!is_never(allowed.parse().unwrap()), "{allowed}");
        }
    }
}
