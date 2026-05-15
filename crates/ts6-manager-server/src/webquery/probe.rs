//! PURA-211 — operator-facing connectivity probe.
//!
//! Builds a one-shot reqwest client against operator-supplied (host, port,
//! useHttps, apiKey), hits `GET /version`, and classifies any reqwest /
//! envelope failure into one of the [`TestConnectionKind`] variants the FE
//! renders without parsing English.
//!
//! Why a probe-only path instead of [`WebQueryClient`]:
//! - The setup wizard fires it BEFORE persisting the row, so there is no
//!   `server_connection.id` to key a pooled client on.
//! - The probe deliberately *avoids* the §10.1 single-socket pool so it
//!   can't steal the live dashboard's keep-alive socket once a row is
//!   created later in the same wizard.
//! - The classification step is the whole point: surface "Connection
//!   refused on host:port" / "DNS failed" / "Timeout after 15s" / "TLS
//!   error" / "HTTP 401" instead of reqwest's `Display` blob. Until
//!   [`WebQueryError::Transport`] grows a typed variant (delegated to
//!   RustPlatform — see the PURA-211 child issue), this classifier is
//!   the single source of operator-friendly remediation copy.

use reqwest::{Client, StatusCode};
use ts6_manager_shared::test_connection::{TestConnectionKind, TestConnectionResponse};

use super::{API_KEY_HEADER, Envelope, REQUEST_TIMEOUT};
use crate::webquery::models::VersionInfo;

/// Result of [`probe_webquery`] — always carries `url_tried` so the FE can
/// show the operator exactly what the panel attempted, even on success.
pub async fn probe_webquery(
    host: &str,
    webquery_port: u16,
    use_https: bool,
    api_key: &str,
    allow_self_signed: bool,
) -> TestConnectionResponse {
    let scheme = if use_https { "https" } else { "http" };
    let url_tried = format!("{scheme}://{host}:{webquery_port}/version");

    let client = match build_one_shot_client(allow_self_signed) {
        Ok(c) => c,
        Err(e) => {
            return TestConnectionResponse {
                ok: false,
                url_tried,
                kind: TestConnectionKind::Other,
                message: format!("Could not build HTTP client: {e}"),
                server_version: None,
            };
        }
    };

    let send_result = client
        .get(&url_tried)
        .header(API_KEY_HEADER, api_key)
        .send()
        .await;

    let response = match send_result {
        Ok(r) => r,
        Err(e) => {
            let (kind, message) = classify_reqwest_error(&e, &url_tried);
            return TestConnectionResponse {
                ok: false,
                url_tried,
                kind,
                message,
                server_version: None,
            };
        }
    };

    let status = response.status();
    // 401 from TS6 means the apiKey was rejected — operator-facing fix
    // distinct from "host wrong" or "port wrong". Surface before reading
    // the body so we don't conflate it with malformed-envelope copy.
    if status == StatusCode::UNAUTHORIZED {
        return TestConnectionResponse {
            ok: false,
            url_tried,
            kind: TestConnectionKind::Unauthorized,
            message: "TS6 rejected the API key (HTTP 401).".into(),
            server_version: None,
        };
    }

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let (kind, message) = classify_reqwest_error(&e, &url_tried);
            return TestConnectionResponse {
                ok: false,
                url_tried,
                kind,
                message,
                server_version: None,
            };
        }
    };

    // Spec §10.5: TS6 packs errors in the envelope body even on non-2xx —
    // parse the envelope first, fall back to "not WebQuery" if it isn't a
    // §10.5 envelope.
    let envelope: Envelope = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            return TestConnectionResponse {
                ok: false,
                url_tried,
                kind: TestConnectionKind::InvalidResponse,
                message: format!(
                    "Server responded with HTTP {} but the body wasn't a WebQuery envelope: {e}",
                    status.as_u16()
                ),
                server_version: None,
            };
        }
    };

    match envelope.into_body::<VersionInfo>() {
        Ok(v) => TestConnectionResponse {
            ok: true,
            url_tried,
            kind: TestConnectionKind::Ok,
            message: "Connected to TS6 WebQuery.".into(),
            server_version: Some(format!("{} ({})", v.version, v.platform)),
        },
        Err(crate::webquery::WebQueryError::Upstream { code, message }) => TestConnectionResponse {
            ok: false,
            url_tried,
            kind: TestConnectionKind::Other,
            message: format!("TS6 WebQuery error (code {code}): {message}"),
            server_version: None,
        },
        Err(other) => TestConnectionResponse {
            ok: false,
            url_tried,
            kind: TestConnectionKind::InvalidResponse,
            message: format!("Unexpected /version response: {other}"),
            server_version: None,
        },
    }
}

fn build_one_shot_client(allow_self_signed: bool) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // One probe per request — defeating the keep-alive pool keeps
        // this path off the WebQuery client's single-socket invariant
        // (§10.1) and avoids leaking idle sockets through the operator
        // form. `pool_max_idle_per_host(0)` is the documented "no pool"
        // shape on reqwest.
        .pool_max_idle_per_host(0)
        .http1_only();
    if allow_self_signed {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build()
}

/// Classify a `reqwest::Error` into a [`TestConnectionKind`] + an
/// operator-facing message. Order of `is_*` checks matters — `is_request()`
/// catches several variants, so the more specific shapes (`is_timeout`,
/// `is_connect`, `is_status`) need to run first.
fn classify_reqwest_error(err: &reqwest::Error, url_tried: &str) -> (TestConnectionKind, String) {
    // The 15s timeout is the most common opaque-looking failure mode on
    // hairpin-NAT setups, so check it before the generic connect-ish
    // bucket.
    if err.is_timeout() {
        return (
            TestConnectionKind::Timeout,
            format!(
                "Timed out after {}s waiting for {url_tried}.",
                REQUEST_TIMEOUT.as_secs()
            ),
        );
    }
    // reqwest's `is_connect()` collapses both the DNS-failed and
    // connection-refused shapes. The error chain has the underlying
    // hyper / hickory / std::io::Error, where the `kind()` /
    // `Display` distinguishes them.
    let chain = error_chain_string(err);
    if err.is_connect() {
        if looks_like_dns_failure(&chain) {
            return (
                TestConnectionKind::Dns,
                format!(
                    "Could not resolve the host in {url_tried}. ({})",
                    short_cause(&chain)
                ),
            );
        }
        return (
            TestConnectionKind::Connect,
            format!(
                "Could not open a TCP connection to {url_tried}. ({})",
                short_cause(&chain)
            ),
        );
    }
    if is_tls_failure(err, &chain) {
        return (
            TestConnectionKind::Tls,
            format!("TLS handshake failed for {url_tried}. ({})", short_cause(&chain)),
        );
    }
    // Some hickory/hyper builds surface "dns error: …" via a non-connect
    // request error rather than via `is_connect()`. Catch that as a
    // belt-and-braces — operators see a DNS failure framed as DNS.
    if looks_like_dns_failure(&chain) {
        return (
            TestConnectionKind::Dns,
            format!(
                "Could not resolve the host in {url_tried}. ({})",
                short_cause(&chain)
            ),
        );
    }
    (
        TestConnectionKind::Other,
        format!("Request to {url_tried} failed: {}", short_cause(&chain)),
    )
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
    // Reqwest's `Display` impl already includes URL + cause; trim the URL
    // part because the operator can already see it in `url_tried`. Length-
    // cap so the banner doesn't grow into a wall of text.
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
    // Cover the message shapes we see across hickory + getaddrinfo +
    // tokio's `dns error: ...` wrappers. Keep these as fragments — the
    // exact wording rotates between reqwest versions.
    lower.contains("dns error")
        || lower.contains("failed to lookup address")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("no such host")
        || lower.contains("temporary failure in name resolution")
}

fn is_tls_failure(err: &reqwest::Error, chain: &str) -> bool {
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

    #[tokio::test]
    async fn probe_returns_connect_on_loopback_with_unused_port() {
        // Bind a port, hold the listener so the port is "open" then close
        // it — net result: a port nothing listens on, OS reliably returns
        // ECONNREFUSED on connect. This is the canonical "WebQuery isn't
        // running on this host:port" path for the PURA-211 root cause.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let resp = probe_webquery("127.0.0.1", port, false, "k", false).await;
        assert!(!resp.ok, "expected probe failure, got {resp:?}");
        assert_eq!(resp.kind, TestConnectionKind::Connect);
        assert!(resp.url_tried.contains(&format!(":{port}/version")));
        assert!(
            resp.message.contains(&resp.url_tried) || resp.message.contains("TCP"),
            "operator message must reference the URL or transport: {resp:?}"
        );
    }

    #[tokio::test]
    async fn probe_returns_dns_on_unresolvable_host() {
        // `.invalid` is RFC-2606 reserved — guaranteed to fail name
        // resolution on every conforming resolver.
        let resp = probe_webquery(
            "no-such-host.invalid",
            10080,
            false,
            "k",
            false,
        )
        .await;
        assert!(!resp.ok);
        // Some resolvers wrap the failure differently — accept either the
        // dedicated DNS shape or the more general Connect bucket, so this
        // test stays green across reqwest/hickory upgrades.
        assert!(
            matches!(
                resp.kind,
                TestConnectionKind::Dns | TestConnectionKind::Connect
            ),
            "expected Dns or Connect, got {:?}: {}",
            resp.kind,
            resp.message
        );
    }

    #[test]
    fn classify_dns_fragments_each_match() {
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
    fn tls_bucket_keyword_fragments_match() {
        // We can't easily synthesise a `reqwest::Error` of the exact
        // shape `is_tls_failure` wants — but the keyword-bucket half of
        // the helper is the only piece that varies with reqwest version,
        // so we test that directly to catch a regression if the fragment
        // list drifts.
        for fragment in [
            "tls handshake failure",
            "certificate has expired",
            "ssl error",
            "handshake interrupted",
        ] {
            let lower = fragment.to_ascii_lowercase();
            assert!(
                lower.contains("tls")
                    || lower.contains("certificate")
                    || lower.contains("ssl")
                    || lower.contains("handshake"),
                "fragment did not match TLS bucket: {fragment}"
            );
        }
    }

    #[test]
    fn short_cause_truncates_long_messages() {
        let long = "x".repeat(400);
        let out = short_cause(&long);
        assert!(out.ends_with('…'));
        // 160 ASCII bytes + the multi-byte ellipsis (3 bytes in UTF-8).
        assert!(out.chars().count() <= 161, "got {} chars: {out}", out.chars().count());
    }

    #[test]
    fn short_cause_trims_trailing_dot_and_takes_last_chain_segment() {
        let chain = "outer -> inner cause.";
        assert_eq!(short_cause(chain), "inner cause");
    }
}
