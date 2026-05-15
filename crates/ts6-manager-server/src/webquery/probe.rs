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
//!   error" / "HTTP 401" instead of reqwest's `Display` blob. PURA-220
//!   shared the underlying classifier with the main WebQuery request
//!   path — see [`crate::webquery::transport_class`]. The probe still
//!   owns the wizard-only `Unauthorized` and `InvalidResponse` legs
//!   because those flow from envelope-level shapes, not reqwest errors.

use reqwest::{Client, StatusCode};
use ts6_manager_shared::test_connection::{TestConnectionKind, TestConnectionResponse};

use super::transport_class::{ClassifiedTransport, WebQueryTransportKind};
use super::{API_KEY_HEADER, Envelope, REQUEST_TIMEOUT, transport_class};
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
            return classified_into_response(
                transport_class::classify_reqwest_error(&e, &url_tried),
                url_tried,
            );
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
            return classified_into_response(
                transport_class::classify_response_body_error(&e, &url_tried),
                url_tried,
            );
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

/// Map a shared [`ClassifiedTransport`] onto the wizard's
/// [`TestConnectionResponse`]. The wizard's `kind` enum is a wire-shape
/// superset of the transport classifier (it adds `Unauthorized` /
/// `InvalidResponse` / `Ok` because those are envelope-level shapes the
/// probe handles after the handshake), so this is a one-way mapping.
fn classified_into_response(
    classified: ClassifiedTransport,
    url_tried: String,
) -> TestConnectionResponse {
    let kind = match classified.kind {
        WebQueryTransportKind::Dns => TestConnectionKind::Dns,
        WebQueryTransportKind::Connect => TestConnectionKind::Connect,
        WebQueryTransportKind::Timeout => TestConnectionKind::Timeout,
        WebQueryTransportKind::Tls => TestConnectionKind::Tls,
        // The wizard predates the shared classifier; "lost body mid-flight"
        // wasn't a wizard outcome (probe always reads the full body of a
        // 1-row /version response). Fold it onto Other so the FE renders
        // the operator-friendly message without inventing a new chip.
        WebQueryTransportKind::Body | WebQueryTransportKind::Other => TestConnectionKind::Other,
    };
    TestConnectionResponse {
        ok: false,
        url_tried,
        kind,
        message: classified.message,
        server_version: None,
    }
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
        let resp = probe_webquery("no-such-host.invalid", 10080, false, "k", false).await;
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
    fn classifier_kind_maps_onto_wizard_kind() {
        // The probe relies on the shared classifier's variants funnelling
        // into the wizard's `TestConnectionKind` without losing the four
        // remediation-tagged buckets. If a future classifier variant
        // sneaks in we want this mapping to fail noisily.
        let cases = [
            (WebQueryTransportKind::Dns, TestConnectionKind::Dns),
            (WebQueryTransportKind::Connect, TestConnectionKind::Connect),
            (WebQueryTransportKind::Timeout, TestConnectionKind::Timeout),
            (WebQueryTransportKind::Tls, TestConnectionKind::Tls),
            (WebQueryTransportKind::Body, TestConnectionKind::Other),
            (WebQueryTransportKind::Other, TestConnectionKind::Other),
        ];
        for (input, expected) in cases {
            let resp = classified_into_response(
                ClassifiedTransport {
                    kind: input,
                    message: "m".into(),
                },
                "http://x/y".into(),
            );
            assert_eq!(
                resp.kind, expected,
                "WebQueryTransportKind::{input:?} must map to {expected:?}"
            );
            assert_eq!(resp.message, "m");
            assert!(!resp.ok);
        }
    }
}
