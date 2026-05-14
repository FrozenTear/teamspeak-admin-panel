//! Spec §6.13 + §9.4 SSRF probe set + R6-specific octal/hex/integer + DNS
//! rebinding cases. All "MUST be rejected" rows fail; the public host case
//! and NXDOMAIN case pass.

use super::*;
use std::net::IpAddr;
use std::str::FromStr;

fn ip(s: &str) -> IpAddr {
    IpAddr::from_str(s).unwrap()
}

fn empty_resolver() -> MockResolver {
    MockResolver::new()
}

async fn assert_rejected(url: &str, resolver: &dyn Resolver) -> SsrfError {
    match is_url_allowed(url, resolver).await {
        Ok(t) => panic!("expected SSRF rejection for {url}, got Ok({t:?})"),
        Err(e) => e,
    }
}

async fn assert_allowed(url: &str, resolver: &dyn Resolver) -> PinnedTarget {
    match is_url_allowed(url, resolver).await {
        Ok(t) => t,
        Err(e) => panic!("expected SSRF allow for {url}, got Err({e})"),
    }
}

// --- Spec §6.13 / §9.4 baseline probe set ----------------------------------

#[tokio::test]
async fn rejects_loopback_v4() {
    assert_rejected("http://127.0.0.1", &empty_resolver()).await;
    assert_rejected("http://127.255.255.254/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_localhost_hostname() {
    let err = assert_rejected("http://localhost/", &empty_resolver()).await;
    matches!(err, SsrfError::HostnameNotAllowed(_));
}

#[tokio::test]
async fn rejects_loopback_v6() {
    assert_rejected("http://[::1]", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_private_ipv4_ranges() {
    assert_rejected("http://10.1.2.3/", &empty_resolver()).await;
    assert_rejected("http://192.168.1.1", &empty_resolver()).await;
    assert_rejected("http://172.16.0.1", &empty_resolver()).await;
    assert_rejected("http://172.31.255.255", &empty_resolver()).await;
}

#[tokio::test]
async fn allows_172_outside_private_window() {
    // 172.15 and 172.32 are public.
    let r = MockResolver::new().with("172-15.test", vec![ip("172.15.0.1")]);
    let _ = assert_allowed("http://172.15.0.1/", &empty_resolver()).await;
    let _ = assert_allowed("http://172.32.0.1/", &empty_resolver()).await;
    // and via DNS
    let _ = assert_allowed("http://172-15.test/", &r).await;
}

#[tokio::test]
async fn rejects_aws_metadata_ipv4() {
    let err = assert_rejected(
        "http://169.254.169.254/latest/meta-data/",
        &empty_resolver(),
    )
    .await;
    matches!(err, SsrfError::IpNotAllowed(_));
}

#[tokio::test]
async fn rejects_gcp_metadata_hostname() {
    let err = assert_rejected("http://metadata.google.internal/", &empty_resolver()).await;
    matches!(err, SsrfError::HostnameNotAllowed(_));
}

#[tokio::test]
async fn rejects_metadata_internal_hostname() {
    let err = assert_rejected("http://metadata.internal/", &empty_resolver()).await;
    matches!(err, SsrfError::HostnameNotAllowed(_));
}

#[tokio::test]
async fn rejects_aws_metadata_ipv6_literal() {
    // fd00:ec2::254 is the spec's named IPv6 metadata IP — caught by the
    // ULA range (fc00::/7).
    let err = assert_rejected("http://[fd00:ec2::254]/", &empty_resolver()).await;
    matches!(err, SsrfError::IpNotAllowed(_));
}

#[tokio::test]
async fn rejects_disallowed_protocol() {
    let err = assert_rejected("gopher://example.com/", &empty_resolver()).await;
    matches!(err, SsrfError::DisallowedProtocol(_));
    let err = assert_rejected("file:///etc/passwd", &empty_resolver()).await;
    matches!(err, SsrfError::DisallowedProtocol(_));
}

#[tokio::test]
async fn rejects_invalid_url() {
    let err = assert_rejected("not a url", &empty_resolver()).await;
    matches!(err, SsrfError::InvalidUrlFormat);
}

// --- R6 octal / hex / integer encodings -----------------------------------

#[tokio::test]
async fn rejects_octal_encoded_loopback() {
    // 0177.0.0.1 = 127.0.0.1
    assert_rejected("http://0177.0.0.1/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_hex_full_encoded_loopback() {
    // 0x7f000001 = 127.0.0.1
    assert_rejected("http://0x7f000001/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_decimal_integer_encoded_loopback() {
    // 2130706433 = 127.0.0.1
    assert_rejected("http://2130706433/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_bsd_two_part_loopback() {
    // 127.1 = 127.0.0.1
    assert_rejected("http://127.1/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_mixed_radix_loopback() {
    // 0x7f.0.0.1 = 127.0.0.1
    assert_rejected("http://0x7f.0.0.1/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_hex_encoded_private_v4() {
    // 0x0a000001 = 10.0.0.1
    assert_rejected("http://0x0a000001/", &empty_resolver()).await;
}

// --- IPv6 link-local + ULA ------------------------------------------------

#[tokio::test]
async fn rejects_ipv6_link_local() {
    assert_rejected("http://[fe80::1]/", &empty_resolver()).await;
    assert_rejected("http://[febf::1]/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_ipv6_ula() {
    assert_rejected("http://[fc00::1]/", &empty_resolver()).await;
    assert_rejected("http://[fd12:3456::1]/", &empty_resolver()).await;
}

#[tokio::test]
async fn rejects_ipv4_mapped_ipv6_with_blocked_v4() {
    // ::ffff:127.0.0.1 → loopback
    assert_rejected("http://[::ffff:127.0.0.1]/", &empty_resolver()).await;
    // ::ffff:10.0.0.1 → private
    assert_rejected("http://[::ffff:10.0.0.1]/", &empty_resolver()).await;
}

#[tokio::test]
async fn allows_ipv4_mapped_ipv6_with_public_v4() {
    let _ = assert_allowed("http://[::ffff:8.8.8.8]/", &empty_resolver()).await;
}

// --- Public allow + DNS rebinding + NXDOMAIN ------------------------------

#[tokio::test]
async fn allows_public_ipv4_literal() {
    let target = assert_allowed("https://1.1.1.1/", &empty_resolver()).await;
    assert_eq!(target.resolved_ip, Some(ip("1.1.1.1")));
}

#[tokio::test]
async fn allows_public_dns_name() {
    let r = MockResolver::new().with("example.com", vec![ip("93.184.216.34")]);
    let target = assert_allowed("https://example.com/", &r).await;
    assert_eq!(target.resolved_ip, Some(ip("93.184.216.34")));
    assert_eq!(target.host, "example.com");
}

#[tokio::test]
async fn rejects_dns_rebinder_to_private_v4() {
    let r = MockResolver::new().with("rebinder.example", vec![ip("10.1.2.3")]);
    let err = assert_rejected("http://rebinder.example/", &r).await;
    match err {
        SsrfError::ResolvedToBlockedRange { host, ip: bad } => {
            assert_eq!(host, "rebinder.example");
            assert_eq!(bad, ip("10.1.2.3"));
        }
        other => panic!("expected ResolvedToBlockedRange, got {other}"),
    }
}

#[tokio::test]
async fn rejects_dns_rebinder_to_loopback_v6() {
    let r = MockResolver::new().with("rebinder6.example", vec![ip("::1")]);
    assert_rejected("http://rebinder6.example/", &r).await;
}

#[tokio::test]
async fn rejects_dns_rebinder_when_any_resolved_ip_is_private() {
    // Resolver returns one public + one private IP. The validator MUST reject
    // because the attacker controls DNS — a single bad answer is enough.
    let r = MockResolver::new().with("mixed.example", vec![ip("8.8.8.8"), ip("10.1.2.3")]);
    assert_rejected("http://mixed.example/", &r).await;
}

#[tokio::test]
async fn allows_nxdomain_per_spec_dot_three() {
    // Spec §9.3: DNS failure (NXDOMAIN) MUST NOT block; let downstream fail
    // naturally. PinnedTarget.resolved_ip should be None to signal "no pin
    // possible; let the HTTP client try its own resolve".
    let r = MockResolver::new().nxdomain("nxdomain.example");
    let target = assert_allowed("http://nxdomain.example/", &r).await;
    assert_eq!(target.resolved_ip, None);
}

#[tokio::test]
async fn case_insensitive_metadata_hostnames() {
    let err = assert_rejected("http://LocalHost/", &empty_resolver()).await;
    matches!(err, SsrfError::HostnameNotAllowed(_));
    let err = assert_rejected("http://Metadata.Google.Internal/", &empty_resolver()).await;
    matches!(err, SsrfError::HostnameNotAllowed(_));
}

#[tokio::test]
async fn unknown_host_with_no_resolver_entry_treated_as_allow() {
    // A host with no entry in the resolver table = MockResolver::NotFound,
    // which the validator treats as DNS failure (allow per spec §9.3).
    let r = empty_resolver();
    let target = assert_allowed("http://nonexistent.example/", &r).await;
    assert_eq!(target.resolved_ip, None);
}
