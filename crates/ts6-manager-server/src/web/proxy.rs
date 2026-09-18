//! Spec §6.8 — single-hop reverse-proxy trust for client-IP attribution
//! and `X-Forwarded-Proto` (HSTS).
//!
//! When the listener sits behind a trusted reverse proxy (nginx, Traefik,
//! HAProxy, Caddy, etc.), the client's real IP arrives in `X-Forwarded-For`
//! and the original scheme in `X-Forwarded-Proto`. The spec mandates that
//! the back-end MUST trust **exactly one** proxy hop and MUST NOT trust
//! client-supplied forwarding-header entries.
//!
//! The convention this module enforces: the trusted proxy **appends** the
//! client IP it observed to whatever XFF the request arrived with. The
//! rightmost entry is therefore the entry our proxy added; it is the only
//! XFF entry we trust. Anything to the left could have been spoofed by a
//! malicious client and is discarded.
//!
//! Configuration:
//!
//! - `TRUSTED_PROXY_HOPS=0` (default) — listener is exposed directly; XFF
//!   is ignored and the source IP comes from `ConnectInfo<SocketAddr>`.
//! - `TRUSTED_PROXY_HOPS=1` — single trusted proxy in front; the rightmost
//!   XFF entry is the trusted client IP. This matches the spec's "exactly
//!   one proxy hop" mandate.
//! - `TRUSTED_PROXY_HOPS=N` (N > 1) — for chained trusted proxies (CDN +
//!   internal LB, etc.). The Nth-from-right entry is taken. Spec advises
//!   against this, but the parameter is honoured if operators have
//!   audited the proxy chain.

use std::net::IpAddr;
use std::net::SocketAddr;

use axum::http::HeaderMap;
use axum::http::Uri;
use axum::http::header::HeaderName;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Decide which IP to attribute the request to.
///
/// Returns the trusted client IP per the policy in the module docs:
/// either the Nth-from-rightmost `X-Forwarded-For` entry (when
/// `trusted_hops > 0`) or the direct connection IP from `ConnectInfo`.
///
/// If `trusted_hops > 0` but XFF is missing / malformed / shorter than
/// `trusted_hops`, the connection IP is used as a fail-safe — better to
/// rate-limit by the immediate peer than to fall through to a wide-open
/// path.
pub fn client_ip(headers: &HeaderMap, connect_info: SocketAddr, trusted_hops: u8) -> IpAddr {
    if trusted_hops == 0 {
        return connect_info.ip();
    }

    let raw = match headers.get(&X_FORWARDED_FOR).and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return connect_info.ip(),
    };

    // XFF is a comma-separated list. Parse from the right because the
    // rightmost entries are the ones nearest us (added by trusted proxies);
    // leftmost entries may have been forged by the original client.
    let entries: Vec<&str> = raw.split(',').map(str::trim).collect();
    let from_right = trusted_hops as usize;
    if entries.len() < from_right {
        // Header shorter than configured chain depth — operator
        // misconfiguration. Fall back to direct peer.
        return connect_info.ip();
    }

    let candidate = entries[entries.len() - from_right];
    candidate
        .parse::<IpAddr>()
        .unwrap_or_else(|_| connect_info.ip())
}

/// Trusted `X-Forwarded-Proto` token, using the same hop-count policy as
/// [`client_ip`].
///
/// - `trusted_hops == 0` — header is ignored (direct listener). A client
///   on bare `:3001` cannot spoof HTTPS and train HSTS.
/// - otherwise the Nth-from-right comma-separated entry is returned after
///   trim. Missing / short / empty headers yield `None`.
pub fn forwarded_proto(headers: &HeaderMap, trusted_hops: u8) -> Option<&str> {
    if trusted_hops == 0 {
        return None;
    }

    let raw = headers.get(&X_FORWARDED_PROTO)?.to_str().ok()?;
    let entries: Vec<&str> = raw.split(',').map(str::trim).collect();
    let from_right = trusted_hops as usize;
    if entries.len() < from_right {
        return None;
    }

    let candidate = entries[entries.len() - from_right];
    if candidate.is_empty() {
        None
    } else {
        Some(candidate)
    }
}

/// True when the request is HTTPS: either the URI scheme is `https`
/// (TLS terminated on this process) or a trusted proxy marked
/// `X-Forwarded-Proto: https`.
///
/// Client-supplied `X-Forwarded-Proto` is ignored unless
/// `trusted_hops > 0`, matching [`client_ip`].
pub fn request_is_https(headers: &HeaderMap, uri: &Uri, trusted_hops: u8) -> bool {
    uri.scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
        || forwarded_proto(headers, trusted_hops).is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer() -> SocketAddr {
        "203.0.113.7:54321".parse().unwrap()
    }

    fn header_map(xff: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = xff {
            h.insert(X_FORWARDED_FOR, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn trusted_hops_zero_ignores_xff_entirely() {
        // Even if the client crafts a believable XFF, hops=0 means we don't
        // trust it. Source IP must be the direct peer.
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, peer(), 0);
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn missing_xff_falls_back_to_peer() {
        let ip = client_ip(&header_map(None), peer(), 1);
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn one_hop_takes_rightmost_entry() {
        // Real client at 198.51.100.5 → trusted proxy appended that IP to
        // XFF. Anything to the left is whatever the client claimed.
        let h = header_map(Some("evil-claim, 198.51.100.5"));
        let ip = client_ip(&h, peer(), 1);
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn one_hop_with_single_entry() {
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, peer(), 1);
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn two_hops_takes_second_from_right() {
        // CDN → internal LB → us. Rightmost = LB-as-seen-from-us, second
        // from right = client-as-seen-by-CDN. With hops=2, we trust the
        // CDN-attributed entry.
        let h = header_map(Some("client-claim, 198.51.100.5, 192.0.2.10"));
        let ip = client_ip(&h, peer(), 2);
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn malformed_xff_entry_falls_back_to_peer() {
        // Operator misconfigured the proxy and it forwarded "unknown"
        // instead of an IP literal. Rate-limit by direct peer rather than
        // bypass the limiter entirely.
        let h = header_map(Some("evil, not-an-ip"));
        let ip = client_ip(&h, peer(), 1);
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn xff_shorter_than_trusted_chain_falls_back_to_peer() {
        // hops=2 but only one XFF entry → chain shorter than the operator
        // configured for. Don't pull from out-of-bounds; use peer.
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, peer(), 2);
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn ipv6_in_xff_round_trips() {
        let h = header_map(Some("evil-claim, 2001:db8::1"));
        let ip = client_ip(&h, peer(), 1);
        assert_eq!(ip.to_string(), "2001:db8::1");
    }

    #[test]
    fn entries_with_whitespace_are_trimmed() {
        let h = header_map(Some(" 198.51.100.5 , 192.0.2.10 "));
        let ip = client_ip(&h, peer(), 1);
        assert_eq!(ip.to_string(), "192.0.2.10");
    }

    fn proto_map(proto: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = proto {
            h.insert(X_FORWARDED_PROTO, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn trusted_hops_zero_ignores_forwarded_proto() {
        let h = proto_map(Some("https"));
        assert_eq!(forwarded_proto(&h, 0), None);
        assert!(!request_is_https(&h, &Uri::from_static("/health"), 0));
    }

    #[test]
    fn missing_forwarded_proto_is_none() {
        assert_eq!(forwarded_proto(&proto_map(None), 1), None);
    }

    #[test]
    fn one_hop_takes_rightmost_proto() {
        let h = proto_map(Some("http, https"));
        assert_eq!(forwarded_proto(&h, 1), Some("https"));
        assert!(request_is_https(&h, &Uri::from_static("/health"), 1));
    }

    #[test]
    fn one_hop_single_https_entry() {
        let h = proto_map(Some("https"));
        assert_eq!(forwarded_proto(&h, 1), Some("https"));
        assert!(request_is_https(&h, &Uri::from_static("/health"), 1));
    }

    #[test]
    fn one_hop_http_is_not_https() {
        let h = proto_map(Some("http"));
        assert_eq!(forwarded_proto(&h, 1), Some("http"));
        assert!(!request_is_https(&h, &Uri::from_static("/health"), 1));
    }

    #[test]
    fn two_hops_takes_second_from_right_proto() {
        // Client claimed https; the nearest trusted proxy recorded http.
        // hops=1 → rightmost = http; hops=2 → second from right = https.
        let h = proto_map(Some("https, http"));
        assert_eq!(forwarded_proto(&h, 2), Some("https"));
        assert_eq!(forwarded_proto(&h, 1), Some("http"));
        assert!(!request_is_https(&h, &Uri::from_static("/"), 1));
    }

    #[test]
    fn proto_shorter_than_trusted_chain_is_none() {
        let h = proto_map(Some("https"));
        assert_eq!(forwarded_proto(&h, 2), None);
    }

    #[test]
    fn proto_entries_with_whitespace_are_trimmed() {
        let h = proto_map(Some(" http , https "));
        assert_eq!(forwarded_proto(&h, 1), Some("https"));
    }

    #[test]
    fn https_uri_scheme_is_trusted_without_proxy() {
        let h = proto_map(None);
        let uri = Uri::from_static("https://panel.example.com/health");
        assert!(request_is_https(&h, &uri, 0));
    }

    #[test]
    fn forwarded_proto_https_is_case_insensitive() {
        let h = proto_map(Some("HTTPS"));
        assert!(request_is_https(&h, &Uri::from_static("/"), 1));
    }
}
