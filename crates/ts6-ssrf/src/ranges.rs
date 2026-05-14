//! Spec §6.7.1 — block list of disallowed IP ranges.
//!
//! IPv4: `0.0.0.0/8`, `10.0.0.0/8`, `127.0.0.0/8`, `169.254.0.0/16`,
//! `172.16.0.0/12` (only 16..=31), `192.168.0.0/16`.
//!
//! IPv6: `::1/128`, `fe80::/10` (link-local), `fc00::/7` (ULA — covers `fc..` and `fd..`),
//! IPv4-mapped `::ffff:0:0/96` (delegate the embedded v4 to IPv4 rules).
//!
//! Spec also names two metadata-IP literals (`169.254.169.254`, `fd00:ec2::254`).
//! Those are checked via the explicit metadata constants in `mod.rs`; this
//! module only owns the *range* classifier.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Returns `true` if the given IP falls in one of the spec's disallowed ranges
/// (private, loopback, link-local, ULA, IPv4-mapped-with-blocked-embedded-v4,
/// `0.0.0.0/8`).
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

pub fn is_blocked_ipv4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    // 0.0.0.0/8 (`this network`)
    if o[0] == 0 {
        return true;
    }
    // 10.0.0.0/8
    if o[0] == 10 {
        return true;
    }
    // 127.0.0.0/8 (loopback)
    if o[0] == 127 {
        return true;
    }
    // 169.254.0.0/16 (link-local)
    if o[0] == 169 && o[1] == 254 {
        return true;
    }
    // 172.16.0.0/12 — second octet 16..=31
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return true;
    }
    // 192.168.0.0/16
    if o[0] == 192 && o[1] == 168 {
        return true;
    }
    false
}

pub fn is_blocked_ipv6(v6: Ipv6Addr) -> bool {
    // ::1/128 (loopback)
    if v6.is_loopback() {
        return true;
    }
    let segs = v6.segments();
    // fe80::/10 (link-local)
    if (segs[0] & 0xFFC0) == 0xFE80 {
        return true;
    }
    // fc00::/7 (ULA — covers fc.. and fd..)
    if (segs[0] & 0xFE00) == 0xFC00 {
        return true;
    }
    // IPv4-mapped IPv6: ::ffff:a.b.c.d → delegate to IPv4 rules on the
    // embedded v4. Per spec §6.7.1: "IPv4-mapped IPv6 (`::ffff:` prefix)
    // where the embedded v4 fails the IPv4 check".
    if let Some(v4) = v6.to_ipv4_mapped()
        && is_blocked_ipv4(v4)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn v4(s: &str) -> Ipv4Addr {
        Ipv4Addr::from_str(s).unwrap()
    }
    fn v6(s: &str) -> Ipv6Addr {
        Ipv6Addr::from_str(s).unwrap()
    }

    #[test]
    fn ipv4_loopback_blocked() {
        assert!(is_blocked_ipv4(v4("127.0.0.1")));
        assert!(is_blocked_ipv4(v4("127.255.255.255")));
        assert!(!is_blocked_ipv4(v4("128.0.0.1"))); // boundary
    }

    #[test]
    fn ipv4_private_ranges_blocked() {
        assert!(is_blocked_ipv4(v4("10.0.0.0")));
        assert!(is_blocked_ipv4(v4("10.255.255.255")));
        assert!(is_blocked_ipv4(v4("192.168.1.1")));
        assert!(is_blocked_ipv4(v4("192.168.255.255")));
    }

    #[test]
    fn ipv4_172_16_through_31_blocked_other_172_allowed() {
        assert!(is_blocked_ipv4(v4("172.16.0.1")));
        assert!(is_blocked_ipv4(v4("172.31.255.255")));
        assert!(!is_blocked_ipv4(v4("172.15.0.1")));
        assert!(!is_blocked_ipv4(v4("172.32.0.1")));
        assert!(!is_blocked_ipv4(v4("172.0.0.1")));
    }

    #[test]
    fn ipv4_link_local_blocked() {
        assert!(is_blocked_ipv4(v4("169.254.169.254"))); // metadata IP via range
        assert!(is_blocked_ipv4(v4("169.254.0.1")));
        assert!(!is_blocked_ipv4(v4("169.255.0.1")));
    }

    #[test]
    fn ipv4_zero_block_blocked() {
        assert!(is_blocked_ipv4(v4("0.0.0.0")));
        assert!(is_blocked_ipv4(v4("0.255.255.255")));
        assert!(!is_blocked_ipv4(v4("1.1.1.1")));
    }

    #[test]
    fn public_ipv4_allowed() {
        assert!(!is_blocked_ipv4(v4("8.8.8.8")));
        assert!(!is_blocked_ipv4(v4("1.1.1.1")));
        assert!(!is_blocked_ipv4(v4("93.184.216.34"))); // example.com (historic)
    }

    #[test]
    fn ipv6_loopback_blocked() {
        assert!(is_blocked_ipv6(v6("::1")));
        assert!(!is_blocked_ipv6(v6("2001:db8::1"))); // doc range, not in our blocklist
    }

    #[test]
    fn ipv6_link_local_blocked() {
        assert!(is_blocked_ipv6(v6("fe80::1")));
        assert!(is_blocked_ipv6(v6(
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
        )));
        assert!(!is_blocked_ipv6(v6("fec0::1"))); // outside fe80::/10
    }

    #[test]
    fn ipv6_ula_blocked_fc_and_fd() {
        assert!(is_blocked_ipv6(v6("fc00::1")));
        assert!(is_blocked_ipv6(v6("fd12:3456:7890::1")));
        assert!(is_blocked_ipv6(v6(
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
        )));
        assert!(!is_blocked_ipv6(v6("fe00::1"))); // outside fc00::/7
    }

    #[test]
    fn ipv4_mapped_ipv6_inherits_ipv4_rules() {
        assert!(is_blocked_ipv6(v6("::ffff:127.0.0.1")));
        assert!(is_blocked_ipv6(v6("::ffff:10.0.0.1")));
        assert!(is_blocked_ipv6(v6("::ffff:169.254.169.254")));
        // public-mapped passes
        assert!(!is_blocked_ipv6(v6("::ffff:8.8.8.8")));
    }
}
