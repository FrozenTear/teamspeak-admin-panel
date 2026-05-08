//! Spec §6.8 — single-hop reverse-proxy trust for client-IP attribution.
//!
//! When the listener sits behind a trusted reverse proxy (nginx, Traefik,
//! HAProxy, etc.), the client's real IP arrives in `X-Forwarded-For`. The
//! spec mandates that the back-end MUST trust **exactly one** proxy hop
//! and MUST NOT trust client-supplied XFF entries.
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
use axum::http::header::HeaderName;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

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
}
