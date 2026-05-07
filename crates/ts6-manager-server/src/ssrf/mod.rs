//! Spec §6.7 + Chapter 9 — outbound HTTP safety / SSRF protection.
//!
//! Single shared validator for every outbound URL the operator can supply
//! (flow webhook/httpRequest actions, music bot stream URLs, video sidecar
//! source URLs, yt-dlp inputs).
//!
//! The public API is [`is_url_allowed`]. Callers MUST use the returned
//! [`PinnedTarget::resolved_ip`] to pin the outbound HTTP connection to the
//! same IP that passed validation — otherwise DNS rebinding still wins.
//! `reqwest::ClientBuilder::resolve_to_addrs(host, &[resolved_ip])` is the
//! intended pattern.
//!
//! Risk owned: R6 (SSRF gaps).

#![allow(dead_code)] // consumed by future workstreams (FLOW, VOICE, VIDEO, REST)

mod ipnorm;
mod ranges;
mod resolver;

#[cfg(test)]
mod tests;

use std::net::IpAddr;

#[allow(unused_imports)]
// re-exported for future call sites; tests use MockResolver/Resolver only
pub use resolver::{HickoryResolver, MockResolver, ResolveError, Resolver};

/// Spec §6.7.1: hostname literals that MUST be rejected even when DNS would
/// have resolved them somewhere benign.
const METADATA_HOSTNAMES: &[&str] = &["localhost", "metadata.google.internal", "metadata.internal"];

/// A URL that has passed the synchronous + DNS rebinding checks.
///
/// `resolved_ip` is the IP that the outbound HTTP client MUST connect to.
/// Callers SHOULD use `reqwest::ClientBuilder::resolve_to_addrs(host, &[resolved_ip])`
/// to pin the connection — otherwise the resolver could return a different
/// (private-range) IP between this validation and the connect.
///
/// `resolved_ip` is `None` only when the host is a DNS name and resolution
/// failed (NXDOMAIN / timeout). Per spec §9.3 the request is allowed in that
/// case; the downstream HTTP/FFmpeg call fails naturally with a more
/// actionable error than a synthetic "DNS failed" rejection.
#[derive(Debug, Clone)]
pub struct PinnedTarget {
    pub url: url::Url,
    pub host: String,
    pub port: u16,
    pub resolved_ip: Option<IpAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("Invalid URL format")]
    InvalidUrlFormat,
    #[error("Disallowed protocol: {0}")]
    DisallowedProtocol(String),
    #[error("URL is missing a host")]
    MissingHost,
    #[error("Hostname not allowed: {0}")]
    HostnameNotAllowed(String),
    #[error("IP not allowed: {0}")]
    IpNotAllowed(IpAddr),
    #[error("Resolved {host} to blocked IP {ip}")]
    ResolvedToBlockedRange { host: String, ip: IpAddr },
}

/// Spec §6.7 + §9 — validate `raw` and return a [`PinnedTarget`] whose
/// `resolved_ip` MUST be used to pin the outbound HTTP connect.
///
/// Steps (short-circuit on first failure):
/// 1. URL parse.
/// 2. Scheme must be `http` or `https`.
/// 3. Host present.
/// 4. If host is a DNS name, reject if it's in [`METADATA_HOSTNAMES`].
/// 5. If host is an IP literal (or a domain that normalises to IPv4 via
///    octal/hex/integer encoding — see [`ipnorm`]), reject if the IP is in a
///    private range (see [`ranges`]).
/// 6. If host is a DNS name, resolve it. If any resolved IP is in a blocked
///    range, reject — the attacker controls DNS, so a single bad answer is
///    enough. If resolution fails, allow per spec §9.3.
pub async fn is_url_allowed(raw: &str, resolver: &dyn Resolver) -> Result<PinnedTarget, SsrfError> {
    // 1. Parse.
    let url = url::Url::parse(raw).map_err(|_| SsrfError::InvalidUrlFormat)?;

    // 2. Protocol allow-list.
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(SsrfError::DisallowedProtocol(scheme.to_string()));
    }

    // 3. Host extraction (typed). `url.host()` returns the typed `Host`
    // variant — IPv4/IPv6/Domain — without any bracket-stripping needed.
    let host_typed = url.host().ok_or(SsrfError::MissingHost)?;
    let host_str = host_typed.to_string();

    let port = url
        .port_or_known_default()
        .ok_or_else(|| SsrfError::DisallowedProtocol(scheme.to_string()))?;

    match host_typed {
        url::Host::Ipv4(v4) => {
            let ip = IpAddr::V4(v4);
            if ranges::is_blocked_ip(ip) {
                return Err(SsrfError::IpNotAllowed(ip));
            }
            Ok(PinnedTarget {
                url,
                host: host_str,
                port,
                resolved_ip: Some(ip),
            })
        }
        url::Host::Ipv6(v6) => {
            let ip = IpAddr::V6(v6);
            if ranges::is_blocked_ip(ip) {
                return Err(SsrfError::IpNotAllowed(ip));
            }
            Ok(PinnedTarget {
                url,
                host: host_str,
                port,
                resolved_ip: Some(ip),
            })
        }
        url::Host::Domain(name) => {
            let lc = name.to_ascii_lowercase();

            // 4. Metadata-hostname literal check.
            if METADATA_HOSTNAMES.contains(&lc.as_str()) {
                return Err(SsrfError::HostnameNotAllowed(name.to_string()));
            }

            // 5. Octal/hex/integer-encoded IPv4 hidden as a "domain".
            // The `url` crate treats `0177.0.0.1` as a domain; we recognise
            // these forms via the BSD-style canonicaliser and run the IPv4
            // range check on the canonical address.
            if let Some(v4) = ipnorm::canonicalise_ipv4(&lc) {
                let ip = IpAddr::V4(v4);
                if ranges::is_blocked_ip(ip) {
                    return Err(SsrfError::IpNotAllowed(ip));
                }
                return Ok(PinnedTarget {
                    url,
                    host: host_str,
                    port,
                    resolved_ip: Some(ip),
                });
            }

            // 6. DNS resolution + range check on every resolved IP.
            match resolver.resolve(&lc).await {
                Ok(ips) if ips.is_empty() => {
                    tracing::warn!(host = %lc, "ssrf: resolver returned empty IP set; allowing");
                    Ok(PinnedTarget {
                        url,
                        host: host_str,
                        port,
                        resolved_ip: None,
                    })
                }
                Ok(ips) => {
                    for ip in &ips {
                        if ranges::is_blocked_ip(*ip) {
                            return Err(SsrfError::ResolvedToBlockedRange {
                                host: name.to_string(),
                                ip: *ip,
                            });
                        }
                    }
                    Ok(PinnedTarget {
                        url,
                        host: host_str,
                        port,
                        resolved_ip: Some(ips[0]),
                    })
                }
                Err(e) => {
                    // Spec §9.3 — DNS failure is not an SSRF signal; allow.
                    tracing::warn!(host = %lc, error = %e, "ssrf: DNS failed; allowing per spec §9.3");
                    Ok(PinnedTarget {
                        url,
                        host: host_str,
                        port,
                        resolved_ip: None,
                    })
                }
            }
        }
    }
}
