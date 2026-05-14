//! IPv4 numeric normalisation — closes the SSRF gap (R6) where attackers feed
//! octal/hex/integer-encoded IPv4 forms past a naive `Ipv4Addr::from_str` validator.
//!
//! Spec §6.7.1 + impl-plan R6: the SSRF blocklist must catch private-range IPs
//! whatever surface form they arrive in. `std::net::Ipv4Addr::from_str` only
//! accepts dotted-decimal `a.b.c.d`. This helper accepts the BSD-style 1/2/3/4-part
//! decimal/octal/hex forms (`0177.0.0.1`, `0x7f000001`, `2130706433`, `127.1`,
//! `127.0.1`, `0x7f.0.0.1`) and returns the canonical `Ipv4Addr`.

use std::net::Ipv4Addr;

/// Try to canonicalise a string as an IPv4 address, accepting the historical
/// BSD-style numeric forms in addition to dotted-decimal.
///
/// Returns `None` if the input is not a valid numeric IPv4 in any of those
/// forms. Returns `Some(Ipv4Addr)` for any input that resolves to a 32-bit
/// IPv4 value; the caller then runs the private-range check on the canonical
/// address.
pub fn canonicalise_ipv4(input: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = input.split('.').collect();
    let n = parts.len();
    if n == 0 || n > 4 {
        return None;
    }

    let nums: Option<Vec<u64>> = parts.iter().map(|p| parse_segment(p)).collect();
    let nums = nums?;

    let bits: u32 = match n {
        1 => {
            let v = nums[0];
            if v > u32::MAX as u64 {
                return None;
            }
            v as u32
        }
        2 => {
            // a.b — a is the high byte (0..=255), b is the low 24 bits.
            let a = nums[0];
            let b = nums[1];
            if a > 0xFF || b > 0x00FF_FFFF {
                return None;
            }
            ((a as u32) << 24) | (b as u32)
        }
        3 => {
            // a.b.c — a (8) | b (8) | c (16).
            let a = nums[0];
            let b = nums[1];
            let c = nums[2];
            if a > 0xFF || b > 0xFF || c > 0xFFFF {
                return None;
            }
            ((a as u32) << 24) | ((b as u32) << 16) | (c as u32)
        }
        4 => {
            for v in &nums {
                if *v > 0xFF {
                    return None;
                }
            }
            ((nums[0] as u32) << 24)
                | ((nums[1] as u32) << 16)
                | ((nums[2] as u32) << 8)
                | (nums[3] as u32)
        }
        _ => unreachable!("split with non-empty parts is bounded 1..=4"),
    };

    Some(Ipv4Addr::from(bits))
}

/// Parse a single dotted segment with prefix-aware radix:
/// - `0x` / `0X` prefix → hex.
/// - leading `0` followed by digits → octal.
/// - bare `0` → decimal zero.
/// - otherwise → decimal.
///
/// Empty segments are rejected.
fn parse_segment(seg: &str) -> Option<u64> {
    if seg.is_empty() {
        return None;
    }
    let bytes = seg.as_bytes();

    // Hex: 0x... / 0X...
    if bytes.len() >= 2 && bytes[0] == b'0' && (bytes[1] == b'x' || bytes[1] == b'X') {
        let rest = &seg[2..];
        if rest.is_empty() {
            return None;
        }
        return u64::from_str_radix(rest, 16).ok();
    }

    // Octal: leading 0 with more digits.
    if bytes.len() > 1 && bytes[0] == b'0' {
        // All remaining must be octal digits (0..=7).
        let rest = &seg[1..];
        if rest.bytes().any(|b| !(b'0'..=b'7').contains(&b)) {
            return None;
        }
        return u64::from_str_radix(rest, 8).ok();
    }

    // Decimal.
    seg.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr::new(a, b, c, d)
    }

    #[test]
    fn dotted_decimal_works() {
        assert_eq!(canonicalise_ipv4("127.0.0.1"), Some(ip(127, 0, 0, 1)));
        assert_eq!(canonicalise_ipv4("0.0.0.0"), Some(ip(0, 0, 0, 0)));
        assert_eq!(
            canonicalise_ipv4("255.255.255.255"),
            Some(ip(255, 255, 255, 255))
        );
    }

    #[test]
    fn octal_segments_work() {
        // 0177 = 127 decimal
        assert_eq!(canonicalise_ipv4("0177.0.0.1"), Some(ip(127, 0, 0, 1)));
        // All-octal encoding of 10.0.0.1
        assert_eq!(canonicalise_ipv4("012.0.0.01"), Some(ip(10, 0, 0, 1)));
    }

    #[test]
    fn hex_segments_work() {
        // 0x7f = 127
        assert_eq!(canonicalise_ipv4("0x7f.0.0.1"), Some(ip(127, 0, 0, 1)));
        assert_eq!(canonicalise_ipv4("0X7F.0.0.1"), Some(ip(127, 0, 0, 1)));
    }

    #[test]
    fn one_part_integer_form() {
        // 2130706433 = 0x7f000001 = 127.0.0.1
        assert_eq!(canonicalise_ipv4("2130706433"), Some(ip(127, 0, 0, 1)));
        // 0x7f000001
        assert_eq!(canonicalise_ipv4("0x7f000001"), Some(ip(127, 0, 0, 1)));
        // 017700000001 (octal of 0x7f000001)
        assert_eq!(canonicalise_ipv4("017700000001"), Some(ip(127, 0, 0, 1)));
        // 0 → 0.0.0.0
        assert_eq!(canonicalise_ipv4("0"), Some(ip(0, 0, 0, 0)));
    }

    #[test]
    fn two_part_form_a_b() {
        // 127.1 = 127.0.0.1
        assert_eq!(canonicalise_ipv4("127.1"), Some(ip(127, 0, 0, 1)));
        // 10.65537 (10. + 0x010001) = 10.1.0.1 (24-bit lower part = 65537)
        assert_eq!(canonicalise_ipv4("10.65537"), Some(ip(10, 1, 0, 1)));
        // upper bound on 24-bit segment: 16777215 = 0xFFFFFF
        assert_eq!(
            canonicalise_ipv4("10.16777215"),
            Some(ip(10, 255, 255, 255))
        );
        // 16777216 (= 2^24) overflows the 24-bit slot.
        assert_eq!(canonicalise_ipv4("10.16777216"), None);
    }

    #[test]
    fn three_part_form_a_b_c() {
        // 127.0.1 = 127.0.0.1
        assert_eq!(canonicalise_ipv4("127.0.1"), Some(ip(127, 0, 0, 1)));
        // 192.168.65535 = 192.168.255.255
        assert_eq!(
            canonicalise_ipv4("192.168.65535"),
            Some(ip(192, 168, 255, 255))
        );
    }

    #[test]
    fn rejects_oversize_segments() {
        assert_eq!(canonicalise_ipv4("256.0.0.1"), None); // 256 > 255 in 4-part form
        assert_eq!(canonicalise_ipv4("127.0.0.999"), None);
        assert_eq!(canonicalise_ipv4("127.16777216"), None); // 0x01000000 > 24-bit max
        assert_eq!(canonicalise_ipv4("4294967296"), None); // 2^32 > u32::MAX
    }

    #[test]
    fn rejects_invalid_segments() {
        assert_eq!(canonicalise_ipv4(""), None);
        assert_eq!(canonicalise_ipv4("..."), None);
        assert_eq!(canonicalise_ipv4("127.0.0."), None);
        assert_eq!(canonicalise_ipv4("0x"), None); // hex with no digits
        assert_eq!(canonicalise_ipv4("09"), None); // 9 isn't an octal digit
        assert_eq!(canonicalise_ipv4("foo"), None);
        assert_eq!(canonicalise_ipv4("127.0.0.1.5"), None); // 5-part is invalid
    }

    #[test]
    fn mixed_radix() {
        // 0x7f.0.0.1 (hex) — already covered.
        // 0177.0x0.0.01 (octal . hex . dec . octal)
        assert_eq!(canonicalise_ipv4("0177.0x0.0.01"), Some(ip(127, 0, 0, 1)));
    }
}
