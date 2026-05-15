//! Wire-format types for the WebQuery "Test connection" probe.
//!
//! PURA-211 — the setup wizard (and, later, the per-server edit form) calls
//! this before persisting credentials. The route exercises `GET /version`
//! against the operator-supplied host:port + apiKey and returns a typed
//! result the FE can render without parsing reqwest's `Display` blob.
//!
//! Two endpoints share the wire shape:
//! - `POST /api/setup/test-connection` (unauthenticated, only while
//!   `needsSetup == true` — same gate as `POST /api/setup/init`).
//! - `POST /api/servers/{configId}/test-connection` (admin, against a
//!   stored row — landing in a follow-up child once the server-edit
//!   form ships).
//!
//! `kind` is a stable wire string so the FE can branch without parsing
//! English copy. Adding a new variant is a paired FE+BE change.

use serde::{Deserialize, Serialize};

/// Request body for the unauth `POST /api/setup/test-connection` route.
/// Mirrors the subset of `SetupInitServer` the probe needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestConnectionRequest {
    pub host: String,
    pub api_key: String,
    pub webquery_port: Option<i64>,
    pub use_https: Option<bool>,
}

/// `POST /api/{setup,servers/:id}/test-connection` response body. `ok` is the
/// pin the FE branches on; `urlTried` is always populated (regardless of
/// outcome) so the operator can copy it into a bug report verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TestConnectionResponse {
    pub ok: bool,
    pub url_tried: String,
    pub kind: TestConnectionKind,
    pub message: String,
    /// TS version banner on `ok: true`, surfaced under the "Connected" copy.
    /// `None` on every failure path.
    pub server_version: Option<String>,
}

/// Stable wire-string classification of the probe outcome. The FE picks copy
/// + remediation hint off this discriminator. A non-`ok` response always
/// carries a `kind != Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestConnectionKind {
    /// `GET /version` returned a valid envelope.
    Ok,
    /// Host did not resolve. Operator likely typo'd the hostname.
    Dns,
    /// TCP connection refused or unreachable. Either WebQuery is bound
    /// elsewhere (e.g. `127.0.0.1`-only) or the firewall drops the port.
    /// This is the PURA-211 root-cause shape.
    Connect,
    /// Request exceeded the 15s WebQuery timeout. Drop on the wire, slow
    /// upstream, or hairpin-NAT black hole.
    Timeout,
    /// TLS handshake failed — self-signed cert without
    /// `TS_ALLOW_SELF_SIGNED`, expired cert, hostname mismatch.
    Tls,
    /// TS6 returned HTTP 401 — apiKey is wrong. Distinct from `Connect`
    /// because the operator's fix is to re-copy the key from TS6's admin
    /// UI, not to change the host.
    Unauthorized,
    /// Reachable but the response was not the expected envelope. Either
    /// the operator pointed at something other than TS6's WebQuery
    /// (caddy / nginx return 200 with HTML), or TS6 is buggy.
    InvalidResponse,
    /// Catch-all for anything the classifier did not recognise. Forwards
    /// the underlying message verbatim so an operator can paste it into
    /// a bug report.
    Other,
}

impl TestConnectionKind {
    /// One-line operator hint tied to this classification. The FE renders
    /// this under the headline message so the operator does not have to
    /// guess at remediation.
    pub fn hint(self) -> &'static str {
        match self {
            TestConnectionKind::Ok => "",
            TestConnectionKind::Dns => {
                "Check the host spelling and that the panel can resolve it."
            }
            TestConnectionKind::Connect => {
                "TS6's WebQuery may be bound to 127.0.0.1, or a firewall is dropping the port. \
                 If the panel runs on the same host as TS6, try 127.0.0.1."
            }
            TestConnectionKind::Timeout => {
                "The host accepted the connection but never responded. Check that TS6's WebQuery \
                 is enabled and not stuck behind a slow hairpin-NAT path."
            }
            TestConnectionKind::Tls => {
                "TLS handshake failed. Use HTTP for self-hosted TS6, or set TS_ALLOW_SELF_SIGNED=1 \
                 if you intentionally use a self-signed certificate."
            }
            TestConnectionKind::Unauthorized => {
                "TS6 rejected the API key. Copy the key from TS6's admin UI and paste it again."
            }
            TestConnectionKind::InvalidResponse => {
                "The host responded but didn't speak WebQuery. Check the port — TS6 defaults to \
                 10080; a different service may be listening on the one you typed."
            }
            TestConnectionKind::Other => {
                "See the panel logs for the full reqwest error."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_with_camel_case_keys() {
        let req = TestConnectionRequest {
            host: "ts.example.com".into(),
            api_key: "K".into(),
            webquery_port: Some(10080),
            use_https: Some(false),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""apiKey":"K""#));
        assert!(json.contains(r#""webqueryPort":10080"#));
        assert!(json.contains(r#""useHttps":false"#));
        assert!(!json.contains("api_key"));
    }

    #[test]
    fn response_kind_serialises_as_snake_case_wire_string() {
        let resp = TestConnectionResponse {
            ok: false,
            url_tried: "http://ts.example.com:10080".into(),
            kind: TestConnectionKind::Connect,
            message: "Connection refused".into(),
            server_version: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""kind":"connect""#), "got: {json}");
        assert!(json.contains(r#""urlTried":"http://ts.example.com:10080""#));
        assert!(json.contains(r#""serverVersion":null"#));
    }

    #[test]
    fn response_round_trip_preserves_discriminator() {
        for kind in [
            TestConnectionKind::Ok,
            TestConnectionKind::Dns,
            TestConnectionKind::Connect,
            TestConnectionKind::Timeout,
            TestConnectionKind::Tls,
            TestConnectionKind::Unauthorized,
            TestConnectionKind::InvalidResponse,
            TestConnectionKind::Other,
        ] {
            let resp = TestConnectionResponse {
                ok: matches!(kind, TestConnectionKind::Ok),
                url_tried: "http://ts.example.com:10080".into(),
                kind,
                message: "msg".into(),
                server_version: None,
            };
            let back: TestConnectionResponse =
                serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
            assert_eq!(back.kind, kind);
        }
    }

    #[test]
    fn hint_for_connect_mentions_loopback_127_0_0_1() {
        assert!(TestConnectionKind::Connect.hint().contains("127.0.0.1"));
    }

    #[test]
    fn hint_for_ok_is_empty_so_ui_can_branch_on_emptiness() {
        assert!(TestConnectionKind::Ok.hint().is_empty());
    }
}
