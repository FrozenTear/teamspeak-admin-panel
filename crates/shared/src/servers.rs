//! Wire-format types for `/api/servers` (spec §7.5).
//!
//! Phase 1 ships only `GET /api/servers` (list, grant-scoped) and
//! `POST /api/servers` (admin-only create). Per-server `PATCH/DELETE/test`
//! and the dashboard count endpoint live in their own follow-up tickets
//! (PURA-22 § "Out of scope").
//!
//! Spec §7.5 mandates **`apiKey` MUST NOT appear in any response**;
//! [`ServerSummary`] preserves that invariant by construction — it has no
//! `api_key` field. Each row also carries `hasSshCredentials: !!sshUsername`
//! (verbatim spec wording) so the FE can render the per-server state
//! without holding ciphertext.
//!
//! Rust fields stay snake_case; `#[serde(rename_all = "camelCase")]` does
//! the rename at the wire boundary per the PURA-4 convention.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `POST /api/servers` request body. Mirrors spec §7.5 — `apiKey` is
/// required, the optional fields default in the handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateServerRequest {
    pub name: String,
    pub host: String,
    pub webquery_port: Option<i64>,
    pub api_key: String,
    pub use_https: Option<bool>,
    pub ssh_port: Option<i64>,
    pub ssh_username: Option<String>,
    pub ssh_password: Option<String>,
    /// `"webquery"` (default) or `"ssh"`.
    #[serde(default)]
    pub control_path: Option<String>,
    /// `"password"`, `"key"`, or `"agent"`. Only consulted when `controlPath == "ssh"`.
    #[serde(default)]
    pub ssh_auth_method: Option<String>,
    /// SHA-256 host-key fingerprint. Required when `controlPath == "ssh"`.
    #[serde(default)]
    pub ssh_host_key_fingerprint: Option<String>,
}

/// `PATCH /api/servers/:id` request body. All fields optional — only supplied
/// fields are mutated; omitted fields preserve the existing DB value.
/// `apiKey` and `sshPassword` are re-sealed on mutation; omitted ciphertext
/// is preserved unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PatchServerRequest {
    pub name: Option<String>,
    pub host: Option<String>,
    pub webquery_port: Option<i64>,
    pub api_key: Option<String>,
    pub use_https: Option<bool>,
    pub ssh_port: Option<i64>,
    pub ssh_username: Option<String>,
    pub ssh_password: Option<String>,
    pub control_path: Option<String>,
    pub ssh_auth_method: Option<String>,
    pub ssh_host_key_fingerprint: Option<String>,
}

/// Response shape for both `GET /api/servers` (list) and the freshly
/// created row from `POST /api/servers` / `POST /api/setup/init`.
///
/// Deliberately omits `apiKey` and `sshPassword` — neither ciphertext
/// nor plaintext belong on the wire (spec §7.5).
///
/// `PartialEq` is derived so this type can ride through Dioxus `Props`
/// (which require their fields to be equality-comparable for change
/// detection) without an in-FE wrapper newtype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSummary {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub webquery_port: i64,
    pub use_https: bool,
    pub ssh_port: i64,
    pub ssh_username: Option<String>,
    /// `!!sshUsername` per spec §7.5 — booleanised so the FE can branch
    /// without re-checking the username field.
    pub has_ssh_credentials: bool,
    pub query_bot_channel: Option<String>,
    pub query_bot_nickname: Option<String>,
    pub ssh_bot_nickname: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_server_request_round_trips_with_camel_case_keys() {
        let req = CreateServerRequest {
            name: "Primary".into(),
            host: "ts.example.com".into(),
            webquery_port: Some(10080),
            api_key: "K".into(),
            use_https: Some(true),
            ssh_port: Some(10022),
            ssh_username: Some("admin".into()),
            ssh_password: Some("pw".into()),
            control_path: Some("ssh".into()),
            ssh_auth_method: Some("password".into()),
            ssh_host_key_fingerprint: Some("SHA256:abc".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        for forbidden in [
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
        assert!(json.contains(r#""apiKey":"K""#));
        assert!(json.contains(r#""useHttps":true"#));
    }

    #[test]
    fn server_summary_has_no_api_key_field_on_the_wire() {
        // Spec §7.5: "apiKey MUST NOT appear in any response."
        let now = chrono::Utc::now();
        let row = ServerSummary {
            id: 1,
            name: "Primary".into(),
            host: "ts.example.com".into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: Some("admin".into()),
            has_ssh_credentials: true,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(!json.contains("apiKey"));
        assert!(!json.contains("api_key"));
        assert!(!json.contains("sshPassword"));
        assert!(!json.contains("ssh_password"));
        assert!(json.contains(r#""hasSshCredentials":true"#));
    }
}
