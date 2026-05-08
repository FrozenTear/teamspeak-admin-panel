//! Wire-format types for `/api/setup/*` (spec §7.2).
//!
//! `POST /api/setup/init` is a one-shot endpoint that creates the bootstrap
//! admin user **and** the first `server_connection` row in a single
//! transaction. Once any user exists the endpoint hard-fails with HTTP 409
//! (PURA-22 acceptance criterion — concurrent inits resolve to one success
//! plus one `409`).
//!
//! Rust fields stay snake_case; `#[serde(rename_all = "camelCase")]` does
//! the rename at the wire boundary per the PURA-4 convention.
//!
//! Implementation lives in `ts6-manager-server::routes::setup` (Phase 1
//! SECURITY slice 5).

use serde::{Deserialize, Serialize};

use crate::auth::UserInfo;
use crate::servers::ServerSummary;

/// `GET /api/setup/status` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatusResponse {
    pub needs_setup: bool,
}

/// `POST /api/setup/init` request body. Carries everything the wizard
/// needs in one round-trip — the operator does not re-authenticate
/// between admin creation and first-server creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupInitRequest {
    pub username: String,
    pub password: String,
    pub display_name: Option<String>,
    pub server: SetupInitServer,
}

/// First-server fields supplied as part of the wizard. Mirrors the
/// `POST /api/servers` body but lives here so the wizard can be served
/// before any auth context exists.
///
/// PURA-99 added `controlPath` / `sshAuthMethod` / `sshHostKeyFingerprint`
/// to the request side so the wizard can pick the SSH backend at first
/// run. The response surface ([`SetupInitResponse::server`] →
/// [`crate::servers::ServerSummary`]) intentionally still omits these
/// fields — the D-SSH-AUTH redaction gate from PURA-77 stays intact.
/// `sshPrivateKey` / `sshKeyAgentSocket` are NOT exposed on the wire
/// here either; key-based SSH still requires a direct DB edit pending
/// SecurityEngineer sign-off on the public surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupInitServer {
    pub name: String,
    pub host: String,
    pub webquery_port: Option<i64>,
    pub api_key: String,
    pub use_https: Option<bool>,
    pub ssh_port: Option<i64>,
    pub ssh_username: Option<String>,
    pub ssh_password: Option<String>,
    /// `"webquery"` (default) or `"ssh"`. Picks the
    /// [`crate::servers::ServerSummary`] backend lazily through
    /// `ControlBackendPool` (PURA-78 / PURA-99).
    #[serde(default)]
    pub control_path: Option<String>,
    /// `"password"` (default), `"key"`, or `"agent"`. Only consulted
    /// when `controlPath == "ssh"`.
    #[serde(default)]
    pub ssh_auth_method: Option<String>,
    /// SHA-256 host-key fingerprint pinned by the operator. The
    /// SSH bridge refuses to connect when this is null and
    /// `TS_SSH_KNOWN_HOSTS` is also unset — fail-closed posture per
    /// PURA-76.
    #[serde(default)]
    pub ssh_host_key_fingerprint: Option<String>,
}

/// `POST /api/setup/init` success body (HTTP 201). Returns the freshly
/// created admin user (without `passwordHash`) and the freshly created
/// server connection (without `apiKey` — see [`ServerSummary`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupInitResponse {
    pub user: UserInfo,
    pub server: ServerSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_response_uses_camel_case() {
        let body = SetupStatusResponse { needs_setup: true };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""needsSetup":true"#));
        assert!(!json.contains("needs_setup"));
    }

    #[test]
    fn init_request_round_trips_with_camel_case_keys() {
        let req = SetupInitRequest {
            username: "admin".into(),
            password: "Hunter2!ok".into(),
            display_name: Some("Admin".into()),
            server: SetupInitServer {
                name: "Primary".into(),
                host: "ts.example.com".into(),
                webquery_port: Some(10080),
                api_key: "WEBQUERY-KEY".into(),
                use_https: Some(true),
                ssh_port: Some(10022),
                ssh_username: Some("serveradmin".into()),
                ssh_password: Some("hunter2".into()),
                control_path: None,
                ssh_auth_method: None,
                ssh_host_key_fingerprint: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        // None of the multi-word fields may leak snake_case to the wire.
        for forbidden in [
            "display_name",
            "webquery_port",
            "api_key",
            "use_https",
            "ssh_port",
            "ssh_username",
            "ssh_password",
        ] {
            assert!(
                !json.contains(forbidden),
                "snake_case field `{forbidden}` leaked to the wire: {json}"
            );
        }
        assert!(json.contains(r#""displayName":"Admin""#));
        assert!(json.contains(r#""apiKey":"WEBQUERY-KEY""#));
        let back: SetupInitRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.username, "admin");
        assert_eq!(back.server.api_key, "WEBQUERY-KEY");
    }

    #[test]
    fn init_request_accepts_minimal_payload_with_optionals_omitted() {
        // Display name + every optional server field must be optional —
        // the wizard's "advanced settings" UX hides these behind a
        // disclosure and the back-end MUST default them.
        let json = r#"{
            "username":"admin",
            "password":"Hunter2!ok",
            "server":{
                "name":"Primary",
                "host":"ts.example.com",
                "apiKey":"K"
            }
        }"#;
        let parsed: SetupInitRequest = serde_json::from_str(json).unwrap();
        assert!(parsed.display_name.is_none());
        assert!(parsed.server.webquery_port.is_none());
        assert!(parsed.server.use_https.is_none());
        assert!(parsed.server.ssh_username.is_none());
    }
}
