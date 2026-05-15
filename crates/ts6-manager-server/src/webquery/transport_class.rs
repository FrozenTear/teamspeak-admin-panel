//! Shared classifier for `reqwest`-level failures hitting TS6 WebQuery.
//!
//! PURA-211 first introduced this logic inside the wizard-only
//! [`crate::webquery::probe`] so the operator stopped seeing reqwest's
//! `Display` blob ("error sending request for url …") on the
//! Test-connection card. PURA-220 lifts the classifier into this module so
//! the dashboard / channels / clients / server-info handlers carry the
//! same operator-friendly class + cause prefix on every WebQuery transport
//! failure — not just the probe.
//!
//! Two entry points:
//! - [`classify_reqwest_error`] — used by [`crate::webquery::probe`] and by
//!   [`crate::webquery::WebQueryClient::request`] for `client.send()`
//!   failures. Drops `is_timeout` / `is_connect` / `is_request` checks onto
//!   the order-sensitive ladder TS6 needs to distinguish DNS-vs-connect on
//!   the same `is_connect()` branch.
//! - [`classify_response_body_error`] — used when [`reqwest::Response::bytes`]
//!   fails after a successful handshake. Always returns
//!   [`WebQueryTransportKind::Body`] because the network already worked
//!   and the remaining shape is "we lost the body mid-flight".
//!
//! The classifier's `message` field is pre-formatted, so the §7.0.2
//! `details` envelope and the dashboard banner can render it verbatim
//! without further string-massaging.

use reqwest::Error as ReqwestError;
use serde::{Deserialize, Serialize};

/// Typed classification of a reqwest failure for the WebQuery transport.
///
/// Order of variants intentionally mirrors the
/// [`crate::shared::test_connection::TestConnectionKind`] subset that the
/// wizard probe maps to, with `Body` added because the
/// `WebQueryClient::request` path can fail *after* the handshake on the
/// response-body read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebQueryTransportKind {
    /// Hostname did not resolve.
    Dns,
    /// TCP refused / unreachable / immediate RST.
    Connect,
    /// Request exceeded the §10.2 fixed 15s deadline.
    Timeout,
    /// TLS handshake / cert / SNI mismatch.
    Tls,
    /// Response body could not be read after a successful handshake.
    Body,
    /// Catch-all — builder errors, header errors, unrecognised reqwest
    /// shapes, pool-construction failures, etc.
    Other,
}

impl WebQueryTransportKind {
    /// Stable wire-string suitable for the `code`/`details`-adjacent
    /// channel on the §7.0.2 envelope. Matches the `serde(rename_all =
    /// "snake_case")` derivation so this string is also what `serde_json`
    /// would emit for this variant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Body => "body",
            Self::Other => "other",
        }
    }
}

/// Pre-formatted, operator-friendly classification of a transport-class
/// failure. `kind` is the FE/UI discriminator; `message` is the human-
/// readable rendering that already names the URL + cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTransport {
    pub kind: WebQueryTransportKind,
    pub message: String,
}

impl ClassifiedTransport {
    /// Render as `"<kind>: <message>"` — the form the §7.0.2 `details`
    /// envelope surfaces and the form [`Display`] forwards. Keeps the
    /// kind discoverable in any log line or operator message without
    /// requiring a second field on the wire shape.
    pub fn formatted(&self) -> String {
        format!("{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::fmt::Display for ClassifiedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.formatted())
    }
}

/// Spec §10.2 — fixed 15s request timeout. Re-exported here purely so the
/// timeout-class message can name it without pulling in the parent
/// module.
const REQUEST_TIMEOUT_SECS: u64 = 15;

/// Classify a `reqwest::Error` produced by `client.send()` / builder
/// construction / similar pre-body shapes. Order of `is_*` checks
/// matters: `is_timeout` MUST run before `is_connect` because some
/// libuv-style timeouts also look like a connect failure, and
/// `looks_like_dns_failure` MUST be considered before falling through to
/// the bare `is_connect` branch because reqwest collapses DNS-failed and
/// TCP-refused onto the same predicate.
pub fn classify_reqwest_error(err: &ReqwestError, url: &str) -> ClassifiedTransport {
    if err.is_timeout() {
        return ClassifiedTransport {
            kind: WebQueryTransportKind::Timeout,
            message: format!("Timed out after {REQUEST_TIMEOUT_SECS}s waiting for {url}."),
        };
    }

    let chain = error_chain_string(err);
    if err.is_connect() {
        if looks_like_dns_failure(&chain) {
            return ClassifiedTransport {
                kind: WebQueryTransportKind::Dns,
                message: format!(
                    "Could not resolve the host in {url}. ({})",
                    short_cause(&chain)
                ),
            };
        }
        return ClassifiedTransport {
            kind: WebQueryTransportKind::Connect,
            message: format!(
                "Could not open a TCP connection to {url}. ({})",
                short_cause(&chain)
            ),
        };
    }
    if is_tls_failure(err, &chain) {
        return ClassifiedTransport {
            kind: WebQueryTransportKind::Tls,
            message: format!(
                "TLS handshake failed for {url}. ({})",
                short_cause(&chain)
            ),
        };
    }
    // Some hickory/hyper builds surface "dns error: …" via a non-connect
    // request error rather than via `is_connect()`. Catch that as a
    // belt-and-braces — operators see a DNS failure framed as DNS.
    if looks_like_dns_failure(&chain) {
        return ClassifiedTransport {
            kind: WebQueryTransportKind::Dns,
            message: format!(
                "Could not resolve the host in {url}. ({})",
                short_cause(&chain)
            ),
        };
    }
    ClassifiedTransport {
        kind: WebQueryTransportKind::Other,
        message: format!("Request to {url} failed: {}", short_cause(&chain)),
    }
}

/// Classify a response-body read failure. The handshake already
/// succeeded, so the operator's mental model is "we lost the body
/// mid-flight" rather than "the server is unreachable" — we always
/// surface `Body` and let the message detail the underlying cause.
pub fn classify_response_body_error(err: &ReqwestError, url: &str) -> ClassifiedTransport {
    let chain = error_chain_string(err);
    // A response-body read can still time out if the server stops
    // streaming. Promote to `Timeout` so the operator sees the same
    // class on both send-side and body-side timeouts.
    if err.is_timeout() {
        return ClassifiedTransport {
            kind: WebQueryTransportKind::Timeout,
            message: format!(
                "Timed out after {REQUEST_TIMEOUT_SECS}s reading the response body from {url}."
            ),
        };
    }
    ClassifiedTransport {
        kind: WebQueryTransportKind::Body,
        message: format!(
            "Lost the response body mid-flight from {url}. ({})",
            short_cause(&chain)
        ),
    }
}

/// Static-message helper for transport-class failures that did not flow
/// through reqwest — e.g. "apiKey is not a valid HTTP header" or the
/// pool's "No connection configured for server config ID X" sentinel.
/// Always `Other`; the operator-friendly copy is what the caller passes
/// in.
pub fn other_static(message: impl Into<String>) -> ClassifiedTransport {
    ClassifiedTransport {
        kind: WebQueryTransportKind::Other,
        message: message.into(),
    }
}

fn error_chain_string<E: std::error::Error + 'static>(err: &E) -> String {
    let mut out = err.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(s) = source {
        out.push_str(" -> ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

fn short_cause(chain: &str) -> String {
    let trimmed = chain
        .split(" -> ")
        .last()
        .unwrap_or(chain)
        .trim()
        .trim_end_matches('.');
    const MAX: usize = 160;
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        format!("{}…", &trimmed[..MAX])
    }
}

fn looks_like_dns_failure(chain: &str) -> bool {
    let lower = chain.to_ascii_lowercase();
    lower.contains("dns error")
        || lower.contains("failed to lookup address")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("no such host")
        || lower.contains("temporary failure in name resolution")
}

fn is_tls_failure(err: &ReqwestError, chain: &str) -> bool {
    if !err.is_request() && !err.is_builder() {
        return false;
    }
    let lower = chain.to_ascii_lowercase();
    lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("ssl")
        || lower.contains("handshake")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_wire_strings_are_snake_case() {
        assert_eq!(WebQueryTransportKind::Dns.as_str(), "dns");
        assert_eq!(WebQueryTransportKind::Connect.as_str(), "connect");
        assert_eq!(WebQueryTransportKind::Timeout.as_str(), "timeout");
        assert_eq!(WebQueryTransportKind::Tls.as_str(), "tls");
        assert_eq!(WebQueryTransportKind::Body.as_str(), "body");
        assert_eq!(WebQueryTransportKind::Other.as_str(), "other");
    }

    #[test]
    fn formatted_renders_kind_prefix() {
        let ct = ClassifiedTransport {
            kind: WebQueryTransportKind::Connect,
            message: "Connection refused on 127.0.0.1:10080".into(),
        };
        assert_eq!(
            ct.formatted(),
            "connect: Connection refused on 127.0.0.1:10080"
        );
        assert_eq!(ct.to_string(), ct.formatted());
    }

    #[test]
    fn looks_like_dns_matches_known_fragments() {
        for fragment in [
            "dns error: failed to lookup address",
            "failed to lookup address information: Name or service not known",
            "No such host is known",
            "nodename nor servname provided",
            "temporary failure in name resolution",
        ] {
            assert!(
                looks_like_dns_failure(fragment),
                "fragment did not match DNS bucket: {fragment}"
            );
        }
    }

    #[test]
    fn short_cause_truncates_long_messages() {
        let long = "x".repeat(400);
        let out = short_cause(&long);
        assert!(out.ends_with('…'));
        assert!(
            out.chars().count() <= 161,
            "got {} chars: {out}",
            out.chars().count()
        );
    }

    #[test]
    fn short_cause_strips_trailing_dot_and_keeps_last_chain_segment() {
        assert_eq!(short_cause("outer -> inner cause."), "inner cause");
    }

    #[test]
    fn other_static_carries_message_verbatim() {
        let ct = other_static("apiKey is not a valid HTTP header");
        assert_eq!(ct.kind, WebQueryTransportKind::Other);
        assert_eq!(ct.message, "apiKey is not a valid HTTP header");
    }

    #[tokio::test]
    async fn classify_connect_refused_loopback() {
        // Bind / drop pattern reliably yields ECONNREFUSED on connect.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/version");
        let err = client.get(&url).send().await.unwrap_err();

        let ct = classify_reqwest_error(&err, &url);
        assert_eq!(ct.kind, WebQueryTransportKind::Connect);
        assert!(ct.message.contains(&url));
    }

    #[tokio::test]
    async fn classify_dns_unresolvable_host() {
        // `.invalid` is RFC-2606 reserved.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let url = "http://no-such-host.invalid:10080/version";
        let err = client.get(url).send().await.unwrap_err();
        let ct = classify_reqwest_error(&err, url);
        assert!(
            matches!(
                ct.kind,
                WebQueryTransportKind::Dns | WebQueryTransportKind::Connect
            ),
            "expected Dns or Connect across resolver builds, got {:?}: {}",
            ct.kind,
            ct.message
        );
    }
}
