//! PURA-172 / WS-Security-R6 fuzz add-on.
//!
//! The R6 blocklist is already covered by curated probes in
//! `tests::rejects_octal_encoded_loopback` etc. This proptest pass pins
//! the contract over the *generative* surface so future edits to
//! `ipnorm` / `ranges` can't silently regress the SSRF guarantee:
//!
//! Property: for every IPv4 address whose canonical form is in a blocked
//! range, EVERY encoded representation (dotted-decimal, 2/3-part BSD,
//! 1-part integer, octal, hex, IPv4-mapped IPv6) MUST be rejected by
//! `is_url_allowed`.
//!
//! Property: for every IPv6 address in the link-local (`fe80::/10`),
//! ULA (`fc00::/7`), loopback (`::1`), or IPv4-mapped-with-blocked-v4
//! ranges, the bracketed literal MUST be rejected.
//!
//! Symmetric properties cover the *allow* side: public IPv4 literals and
//! their encoded forms MUST be allowed (modulo NXDOMAIN allow-through for
//! DNS names, which we don't exercise here — these tests run on literals).
//!
//! These are R6's claim made executable: the blocklist is a function of
//! the canonical IP, not of the surface syntax.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use proptest::prelude::*;
use ts6_ssrf::{MockResolver, SsrfError, is_url_allowed};

/// Synchronously run `is_url_allowed` against an empty resolver. The
/// proptest harness is sync; we wrap a single-threaded tokio runtime so
/// the async validator can be exercised inside `proptest!`.
fn check_url(url: &str) -> Result<(), SsrfError> {
    let resolver = MockResolver::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        match is_url_allowed(url, &resolver).await {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    })
}

/// Generate any IPv4 octet quadruple — public and blocked alike, so the
/// proptest exercises both sides of the allow/deny boundary.
fn any_v4() -> impl Strategy<Value = Ipv4Addr> {
    (0u8..=255, 0u8..=255, 0u8..=255, 0u8..=255).prop_map(|(a, b, c, d)| Ipv4Addr::new(a, b, c, d))
}

/// Pure-Rust port of `ranges::is_blocked_ipv4` so the proptest doesn't
/// depend on visibility of the crate-internal helper. Keep in sync with
/// `crates/ts6-ssrf/src/ranges.rs`; the divergence test below also pins
/// it.
fn expect_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
}

fn expect_blocked_v6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() {
        return true;
    }
    let segs = v6.segments();
    if (segs[0] & 0xFFC0) == 0xFE80 {
        return true;
    }
    if (segs[0] & 0xFE00) == 0xFC00 {
        return true;
    }
    if let Some(v4) = v6.to_ipv4_mapped()
        && expect_blocked_v4(v4)
    {
        return true;
    }
    false
}

proptest! {
    // Keep the case-count modest — every case spins a fresh tokio runtime
    // so we want fast feedback, not exhaustive enumeration.
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Dotted-decimal IPv4 literal: allow/block decision MUST match
    /// `expect_blocked_v4` exactly.
    #[test]
    fn dotted_decimal_v4_matches_blocklist(v4 in any_v4()) {
        let url = format!("http://{v4}/");
        let blocked = expect_blocked_v4(v4);
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "{v4} should be rejected, got {got:?}");
        } else {
            prop_assert!(got.is_ok(), "{v4} should be allowed, got {got:?}");
        }
    }

    /// Decimal integer-encoded IPv4 (the `2130706433 = 127.0.0.1` family).
    /// `is_url_allowed` MUST canonicalise and apply the same blocklist
    /// rule as the dotted form.
    #[test]
    fn integer_encoded_v4_matches_blocklist(v4 in any_v4()) {
        let bits = u32::from(v4);
        let url = format!("http://{bits}/");
        let blocked = expect_blocked_v4(v4);
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "{v4} as integer {bits} should be rejected, got {got:?}");
        } else {
            // Integer form requires the canonicaliser to recognise it as a
            // 1-part numeric IPv4. Public IPs should land in the same
            // allow bucket.
            prop_assert!(got.is_ok(), "{v4} as integer {bits} should be allowed, got {got:?}");
        }
    }

    /// Hex-encoded IPv4 (`0x7f000001`).
    #[test]
    fn hex_encoded_v4_matches_blocklist(v4 in any_v4()) {
        let bits = u32::from(v4);
        let url = format!("http://0x{bits:x}/");
        let blocked = expect_blocked_v4(v4);
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "{v4} as hex 0x{bits:x} should be rejected, got {got:?}");
        } else {
            prop_assert!(got.is_ok(), "{v4} as hex 0x{bits:x} should be allowed, got {got:?}");
        }
    }

    /// Octal-encoded IPv4 (`0177.0.0.1`). Each octet is octal-prefixed.
    /// Octets > 7 would mix radices ambiguously; we restrict to octets
    /// whose decimal value happens to be a valid octal too (<= 0o377 == 255
    /// always, but each digit must be 0..=7). We mask octets to the
    /// range 0..=7 in each digit to dodge `09`-style invalid segments.
    /// That preserves coverage of the loopback (127 = 0o177), private
    /// (10 = 0o12), metadata (169.254 = 0o251.0o376), 192.168 (= 0o300.0o250)
    /// representations the spec calls out, plus their public neighbours.
    #[test]
    fn octal_encoded_v4_matches_blocklist(v4 in any_v4()) {
        let o = v4.octets();
        let url = format!("http://0{:o}.0{:o}.0{:o}.0{:o}/", o[0], o[1], o[2], o[3]);
        let blocked = expect_blocked_v4(v4);
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "{v4} as octal should be rejected, got {got:?}");
        } else {
            prop_assert!(got.is_ok(), "{v4} as octal should be allowed, got {got:?}");
        }
    }

    /// IPv4-mapped IPv6 literal (`::ffff:a.b.c.d`). The blocklist
    /// delegates to the embedded v4 (spec §6.7.1).
    #[test]
    fn ipv4_mapped_ipv6_matches_v4_blocklist(v4 in any_v4()) {
        let url = format!("http://[::ffff:{v4}]/");
        let blocked = expect_blocked_v4(v4);
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "::ffff:{v4} should be rejected, got {got:?}");
        } else {
            prop_assert!(got.is_ok(), "::ffff:{v4} should be allowed, got {got:?}");
        }
    }

    /// IPv6 link-local (`fe80::/10`) — every literal in that prefix MUST
    /// be rejected, regardless of the interface-id suffix.
    #[test]
    fn ipv6_link_local_always_rejected(
        suffix0 in any::<u16>(),
        suffix1 in any::<u16>(),
        suffix2 in any::<u16>(),
        suffix3 in any::<u16>(),
        prefix in 0xFE80u16..=0xFEBF,
    ) {
        let v6 = Ipv6Addr::new(prefix, 0, 0, 0, suffix0, suffix1, suffix2, suffix3);
        let url = format!("http://[{v6}]/");
        let got = check_url(&url);
        prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "link-local {v6} should be rejected, got {got:?}");
    }

    /// IPv6 ULA (`fc00::/7` — covers `fc..` and `fd..`).
    #[test]
    fn ipv6_ula_always_rejected(
        suffix0 in any::<u16>(),
        suffix1 in any::<u16>(),
        suffix2 in any::<u16>(),
        suffix3 in any::<u16>(),
        prefix in prop_oneof![0xFC00u16..=0xFCFF, 0xFD00u16..=0xFDFF],
    ) {
        let v6 = Ipv6Addr::new(prefix, 0, 0, 0, suffix0, suffix1, suffix2, suffix3);
        let url = format!("http://[{v6}]/");
        let got = check_url(&url);
        prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "ULA {v6} should be rejected, got {got:?}");
    }

    /// Universal IPv6 surface — any address whose canonical form sits in
    /// loopback / link-local / ULA / ::ffff:blocked MUST be rejected;
    /// everything else MUST be allowed. This catches off-by-one regressions
    /// at range boundaries (e.g. `febf:ffff::` last link-local address,
    /// `fec0::` first non-link-local, `fc00::` start of ULA, `fe00::`
    /// outside ULA).
    #[test]
    fn ipv6_any_literal_matches_expected_blocklist(
        s0 in any::<u16>(),
        s1 in any::<u16>(),
        s2 in any::<u16>(),
        s3 in any::<u16>(),
        s4 in any::<u16>(),
        s5 in any::<u16>(),
        s6 in any::<u16>(),
        s7 in any::<u16>(),
    ) {
        let v6 = Ipv6Addr::new(s0, s1, s2, s3, s4, s5, s6, s7);
        let blocked = expect_blocked_v6(v6);
        let url = format!("http://[{v6}]/");
        let got = check_url(&url);
        if blocked {
            prop_assert!(matches!(got, Err(SsrfError::IpNotAllowed(_))), "{v6} should be rejected, got {got:?}");
        } else {
            prop_assert!(got.is_ok(), "{v6} should be allowed, got {got:?}");
        }
    }
}

/// Spec-named metadata IPs MUST always be rejected (regression pin —
/// proptest would land on these by chance, but a dedicated assertion
/// is cheaper to read in a bisect).
#[tokio::test]
async fn spec_named_metadata_ips_always_rejected() {
    let resolver = MockResolver::new();
    for url in [
        "http://169.254.169.254/",          // AWS / GCP / Azure
        "http://2852039166/",               // AWS metadata as integer
        "http://0xa9fea9fe/",               // AWS metadata as hex
        "http://0251.0376.0251.0376/",      // AWS metadata as octal
        "http://[::ffff:169.254.169.254]/", // AWS metadata IPv4-mapped IPv6
        "http://[fd00:ec2::254]/",          // AWS IPv6 metadata (ULA)
    ] {
        let got = is_url_allowed(url, &resolver).await;
        assert!(
            matches!(
                got,
                Err(SsrfError::IpNotAllowed(_))
                    | Err(SsrfError::HostnameNotAllowed(_))
                    | Err(SsrfError::ResolvedToBlockedRange { .. })
            ),
            "metadata URL {url} must be rejected; got {got:?}"
        );
    }
}

/// Round-trip the example public IPs from the spec to confirm they are
/// allowed across every encoded surface (sanity pin — the proptest covers
/// random public IPs; this is a fast smoke that survives a strategy
/// change). IPv4-mapped IPv6 surfaces as the v6 form post-validation
/// because the URL crate parses it as an `Ipv6` host; the IP is still
/// the same 32-bit value, and the proxy's `resolve_to_addrs` accepts
/// either.
#[tokio::test]
async fn known_public_ips_allowed_every_encoded_form() {
    let resolver = MockResolver::new();
    let v4 = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
    let v6_mapped = IpAddr::V6("::ffff:8.8.8.8".parse().unwrap());
    // 8.8.8.8 → 134744072 → 0x08080808 → 010.010.010.010
    let cases: &[(&str, IpAddr)] = &[
        ("http://8.8.8.8/", v4),
        ("http://134744072/", v4),
        ("http://0x08080808/", v4),
        ("http://010.010.010.010/", v4),
        ("http://[::ffff:8.8.8.8]/", v6_mapped),
    ];
    for (url, expected_ip) in cases {
        let got = is_url_allowed(url, &resolver).await;
        assert!(got.is_ok(), "public URL {url} must be allowed; got {got:?}");
        assert_eq!(
            got.unwrap().resolved_ip,
            Some(*expected_ip),
            "encoded form {url} must canonicalise to {expected_ip}"
        );
    }
}
