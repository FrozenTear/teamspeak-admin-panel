//! Spec §6.8 — reverse-proxy trust for client-IP attribution and
//! `X-Forwarded-Proto` (HSTS).
//!
//! When the listener sits behind a trusted reverse proxy (nginx, Traefik,
//! HAProxy, Caddy, etc.), the client's real IP arrives in `X-Forwarded-For`
//! and the original scheme in `X-Forwarded-Proto`. The spec mandates that
//! the back-end MUST trust **exactly one** proxy hop and MUST NOT trust
//! client-supplied forwarding-header entries.
//!
//! Hop count alone is not enough. `:3001` is often bound on `0.0.0.0`
//! (host network), so any TCP peer can send `X-Forwarded-For` and
//! `X-Forwarded-Proto`. Forwarding headers are honoured only when **both**:
//!
//! - `TRUSTED_PROXY_HOPS` > 0, and
//! - the `ConnectInfo` peer address is inside `TRUSTED_PROXY_CIDRS`.
//!
//! An empty CIDR list (the default) never trusts forwarding headers, even
//! when hops is 1. A deployment that sets `TRUSTED_PROXY_HOPS=1` must also
//! set the proxy's peer CIDR (Caddy's address), or keep `:3001` unreachable
//! except from that proxy. Hops without a CIDR does not turn the headers on.
//!
//! The convention this module enforces once the peer is trusted: the proxy
//! **appends** the client IP it observed to whatever XFF the request arrived
//! with. The rightmost entry is therefore the entry our proxy added; it is
//! the only XFF entry we trust. Anything to the left could have been spoofed
//! by a malicious client and is discarded.
//!
//! Configuration:
//!
//! - `TRUSTED_PROXY_HOPS=0` (default) — listener is exposed directly; XFF
//!   and `X-Forwarded-Proto` are ignored. The source IP comes from
//!   `ConnectInfo<SocketAddr>`.
//! - `TRUSTED_PROXY_HOPS=1` plus `TRUSTED_PROXY_CIDRS=<proxy>/32` — single
//!   trusted proxy in front; the rightmost XFF entry is the trusted client
//!   IP. This matches the spec's "exactly one proxy hop" mandate.
//! - `TRUSTED_PROXY_HOPS=N` (N > 1) — for chained trusted proxies (CDN +
//!   internal LB, etc.). The Nth-from-right entry is taken, still only when
//!   the immediate peer is inside the CIDR list. Spec advises against N > 1,
//!   but the parameter is honoured if operators have audited the proxy chain.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::http::header::HeaderName;
use ipnet::IpNet;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Hop count plus the proxy CIDR allow-list. Cheap to clone (`Arc`).
///
/// Empty `cidrs` is default-deny: forwarding headers are ignored even
/// when `hops > 0`.
#[derive(Clone, Debug)]
pub struct ProxyTrust {
    pub hops: u8,
    pub cidrs: Arc<Vec<IpNet>>,
}

impl ProxyTrust {
    pub fn from_parts(hops: u8, cidrs: Vec<IpNet>) -> Self {
        Self {
            hops,
            cidrs: Arc::new(cidrs),
        }
    }

    /// Direct listener. Forwarding headers are ignored.
    pub fn direct() -> Self {
        Self::from_parts(0, Vec::new())
    }

    /// True when this TCP peer is allowed to supply `X-Forwarded-*`.
    pub fn honors_forwarding(&self, peer: IpAddr) -> bool {
        self.hops > 0 && self.cidrs.iter().any(|net| net.contains(&peer))
    }
}

/// Decide which IP to attribute the request to.
///
/// Returns the trusted client IP per the policy in the module docs:
/// either the Nth-from-rightmost `X-Forwarded-For` entry (when the peer
/// is inside a configured proxy CIDR and `hops > 0`) or the direct
/// connection IP from `ConnectInfo`.
///
/// If hops are set but the peer is outside the CIDR list, XFF is ignored.
/// If the peer is trusted but XFF is missing / malformed / shorter than
/// `hops`, the connection IP is used as a fail-safe — better to
/// rate-limit by the immediate peer than to fall through to a wide-open
/// path.
pub fn client_ip(headers: &HeaderMap, connect_info: SocketAddr, trust: &ProxyTrust) -> IpAddr {
    if !trust.honors_forwarding(connect_info.ip()) {
        return connect_info.ip();
    }
    let trusted_hops = trust.hops;

    let raw = match joined_header_lines(headers, &X_FORWARDED_FOR) {
        Some(s) => s,
        None => return connect_info.ip(),
    };

    // XFF is a comma-separated list. Parse from the right because the
    // rightmost entries are the ones nearest us (added by trusted proxies);
    // leftmost entries may have been forged by the original client.
    let entries: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
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

/// Trusted `X-Forwarded-Proto` token, using the same peer-CIDR and
/// hop-count policy as [`client_ip`].
///
/// - peer outside `TRUSTED_PROXY_CIDRS`, or `hops == 0` — header is
///   ignored. A direct client on bare `:3001` cannot spoof HTTPS and
///   train HSTS, even if hops is set and the CIDR list is empty.
/// - otherwise the Nth-from-right comma-separated entry is returned after
///   trim. Missing / short / empty headers yield `None`.
pub fn forwarded_proto(headers: &HeaderMap, peer: IpAddr, trust: &ProxyTrust) -> Option<String> {
    if !trust.honors_forwarding(peer) {
        return None;
    }
    let trusted_hops = trust.hops;

    let raw = joined_header_lines(headers, &X_FORWARDED_PROTO)?;
    let entries: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let from_right = trusted_hops as usize;
    if entries.len() < from_right {
        return None;
    }

    let candidate = entries[entries.len() - from_right];
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

/// Join every line of `name`. A proxy that appends a second header
/// (instead of extending the first comma-separated line) must still
/// contribute the rightmost hop. A non-UTF-8 line fails closed (`None`).
fn joined_header_lines(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    let mut parts = Vec::new();
    for value in headers.get_all(name) {
        let text = value.to_str().ok()?;
        if !text.is_empty() {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// True when a trusted proxy marked `X-Forwarded-Proto: https`.
///
/// The request-target's URI scheme is not consulted. An absolute-form
/// request line (`GET https://panel.example/ HTTP/1.1`) over cleartext
/// is client-controlled and must not turn HSTS on. In-process TLS, if
/// it is ever terminated here, has to be signalled by a trusted proxy
/// header (or by not using this helper).
///
/// Client-supplied `X-Forwarded-Proto` is ignored unless the TCP peer is
/// inside [`ProxyTrust`]'s CIDR list and hops is non-zero.
pub fn request_is_https(headers: &HeaderMap, peer: IpAddr, trust: &ProxyTrust) -> bool {
    forwarded_proto(headers, peer, trust).is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer() -> SocketAddr {
        "203.0.113.7:54321".parse().unwrap()
    }

    fn outside_peer() -> SocketAddr {
        "198.51.100.8:9".parse().unwrap()
    }

    /// Hop-count tests assume the direct peer sits in this CIDR.
    fn trust_for(hops: u8) -> ProxyTrust {
        ProxyTrust::from_parts(hops, vec!["203.0.113.0/24".parse().unwrap()])
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
        let ip = client_ip(&h, peer(), &trust_for(0));
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn missing_xff_falls_back_to_peer() {
        let ip = client_ip(&header_map(None), peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn one_hop_takes_rightmost_entry() {
        // Real client at 198.51.100.5 → trusted proxy appended that IP to
        // XFF. Anything to the left is whatever the client claimed.
        let h = header_map(Some("evil-claim, 198.51.100.5"));
        let ip = client_ip(&h, peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn one_hop_with_single_entry() {
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn peer_outside_cidr_ignores_xff_even_with_hops() {
        // hops=1 is not enough. A direct client that is not the proxy
        // must not choose its own rate-limit / audit IP.
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, outside_peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "198.51.100.8");
    }

    #[test]
    fn empty_cidr_list_ignores_xff_even_with_hops() {
        let h = header_map(Some("198.51.100.5"));
        let trust = ProxyTrust::from_parts(1, Vec::new());
        let ip = client_ip(&h, peer(), &trust);
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn two_hops_takes_second_from_right() {
        // CDN → internal LB → us. Rightmost = LB-as-seen-from-us, second
        // from right = client-as-seen-by-CDN. With hops=2, we trust the
        // CDN-attributed entry.
        let h = header_map(Some("client-claim, 198.51.100.5, 192.0.2.10"));
        let ip = client_ip(&h, peer(), &trust_for(2));
        assert_eq!(ip.to_string(), "198.51.100.5");
    }

    #[test]
    fn malformed_xff_entry_falls_back_to_peer() {
        // Operator misconfigured the proxy and it forwarded "unknown"
        // instead of an IP literal. Rate-limit by direct peer rather than
        // bypass the limiter entirely.
        let h = header_map(Some("evil, not-an-ip"));
        let ip = client_ip(&h, peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn xff_shorter_than_trusted_chain_falls_back_to_peer() {
        // hops=2 but only one XFF entry → chain shorter than the operator
        // configured for. Don't pull from out-of-bounds; use peer.
        let h = header_map(Some("198.51.100.5"));
        let ip = client_ip(&h, peer(), &trust_for(2));
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    #[test]
    fn separate_xff_header_lines_are_joined() {
        // A proxy that appends a second header line, rather than extending
        // the client's comma-separated value, must not let the client pick
        // the rate-limit key. hops=1 is the rightmost line.
        let mut h = HeaderMap::new();
        h.append(X_FORWARDED_FOR, HeaderValue::from_static("203.0.113.9"));
        h.append(X_FORWARDED_FOR, HeaderValue::from_static("192.0.2.10"));
        let ip = client_ip(&h, peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "192.0.2.10");
    }

    fn ipv6_in_xff_round_trips() {
        let h = header_map(Some("evil-claim, 2001:db8::1"));
        let ip = client_ip(&h, peer(), &trust_for(1));
        assert_eq!(ip.to_string(), "2001:db8::1");
    }

    #[test]
    fn entries_with_whitespace_are_trimmed() {
        let h = header_map(Some(" 198.51.100.5 , 192.0.2.10 "));
        let ip = client_ip(&h, peer(), &trust_for(1));
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
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(0)).as_deref(),
            None
        );
        assert!(!request_is_https(&h, peer().ip(), &trust_for(0)));
    }

    #[test]
    fn missing_forwarded_proto_is_none() {
        assert_eq!(
            forwarded_proto(&proto_map(None), peer().ip(), &trust_for(1)).as_deref(),
            None
        );
    }

    #[test]
    fn one_hop_takes_rightmost_proto() {
        let h = proto_map(Some("http, https"));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(1)).as_deref(),
            Some("https")
        );
        assert!(request_is_https(&h, peer().ip(), &trust_for(1)));
    }

    #[test]
    fn peer_outside_cidr_ignores_forwarded_proto() {
        let h = proto_map(Some("https"));
        assert_eq!(
            forwarded_proto(&h, outside_peer().ip(), &trust_for(1)).as_deref(),
            None
        );
        assert!(!request_is_https(&h, outside_peer().ip(), &trust_for(1)));
    }

    #[test]
    fn one_hop_single_https_entry() {
        let h = proto_map(Some("https"));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(1)).as_deref(),
            Some("https")
        );
        assert!(request_is_https(&h, peer().ip(), &trust_for(1)));
    }

    #[test]
    fn one_hop_http_is_not_https() {
        let h = proto_map(Some("http"));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(1)).as_deref(),
            Some("http")
        );
        assert!(!request_is_https(&h, peer().ip(), &trust_for(1)));
    }

    #[test]
    fn two_hops_takes_second_from_right_proto() {
        // Client claimed https; the nearest trusted proxy recorded http.
        // hops=1 → rightmost = http; hops=2 → second from right = https.
        let h = proto_map(Some("https, http"));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(2)).as_deref(),
            Some("https")
        );
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(1)).as_deref(),
            Some("http")
        );
        assert!(!request_is_https(&h, peer().ip(), &trust_for(1)));
    }

    #[test]
    fn proto_shorter_than_trusted_chain_is_none() {
        let h = proto_map(Some("https"));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(2)).as_deref(),
            None
        );
    }

    #[test]
    fn proto_entries_with_whitespace_are_trimmed() {
        let h = proto_map(Some(" http , https "));
        assert_eq!(
            forwarded_proto(&h, peer().ip(), &trust_for(1)).as_deref(),
            Some("https")
        );
    }

    #[test]
    fn absolute_form_https_uri_does_not_count_as_https() {
        // The request line is client-controlled. A cleartext client must
        // not train HSTS by sending `GET https://panel.example/ HTTP/1.1`.
        let h = proto_map(None);
        assert!(!request_is_https(&h, peer().ip(), &ProxyTrust::direct()));
        assert!(!request_is_https(&h, peer().ip(), &trust_for(1)));
    }

    #[test]
    fn forwarded_proto_https_is_case_insensitive() {
        let h = proto_map(Some("HTTPS"));
        assert!(request_is_https(&h, peer().ip(), &trust_for(1)));
    }
}
