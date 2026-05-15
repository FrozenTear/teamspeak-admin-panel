//! Wire-format types for the v1.1 admin management surface
//! (`/api/users/*`, `/api/users/{id}/sessions`, `/api/audit`).
//!
//! Per `docs/admin/http-api.md` §2: JSON field names are camelCase; Rust
//! fields use idiomatic snake_case with `#[serde(rename_all = "camelCase")]`
//! mapping at the (de)serialise boundary. The wire keys are the external
//! contract — the camelCase regression test at the bottom of this module
//! pins each struct so a missing `rename_all` on a future addition fails
//! CI rather than drifting silently.
//!
//! Implementation lives in `ts6-manager-server::routes::users` and
//! `ts6-manager-server::routes::audit`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `GET /api/users` / `POST /api/users` / `PATCH /api/users/{id}` response.
///
/// Spec §7.4 + `docs/admin/http-api.md` §2.1: never includes `passwordHash`
/// — the absence is enforced at the type level (no field exists for it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    /// Count of refresh_token rows for this user where `replacedBy IS NONE`
    /// and `expiresAt > now`. See `docs/admin/http-api.md` §2.1.
    pub active_session_count: i64,
}

/// `POST /api/users` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserCreate {
    /// `1..=64` ASCII chars matching `[a-z0-9._-]+`. Server-side lowercased.
    pub username: String,
    /// Validated against spec §6.2.2 complexity rules.
    pub password: String,
    pub display_name: String,
    /// Defaults to `"viewer"` when absent. Must be `admin`, `moderator`, or
    /// `viewer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// `PATCH /api/users/{id}` request body. At least one field must be
/// present; an empty patch is rejected with 400.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Validated against spec §6.2.2 when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// `GET /api/users/{id}/sessions` element. The refresh-token byte string is
/// **never** exposed; only the `family` id rides the wire so the operator UI
/// can group sessions by login chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSession {
    pub id: i64,
    pub family: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub replaced_by: Option<String>,
}

/// `GET /api/audit` element. See `docs/admin/audit-shape.md` §2 for the
/// full schema; this is the wire mirror.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: i64,
    pub occurred_at: DateTime<Utc>,
    pub inserted_at: DateTime<Utc>,
    pub actor_user_id: Option<i64>,
    pub actor_username: String,
    pub kind: String,
    pub target_kind: Option<String>,
    pub target_id: Option<i64>,
    pub target_label: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub outcome: String,
    pub error_msg: Option<String>,
    pub request_ip: Option<String>,
    pub request_user_agent: Option<String>,
}

/// Generic paginated envelope used by `GET /api/audit`. See
/// `docs/admin/http-api.md` §2.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_admin_user() -> AdminUser {
        AdminUser {
            id: 1,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: "admin".into(),
            enabled: true,
            created_at: Utc.with_ymd_and_hms(2026, 5, 19, 13, 5, 14).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 5, 19, 13, 5, 14).unwrap(),
            last_login_at: None,
            active_session_count: 2,
        }
    }

    #[test]
    fn admin_user_serialises_with_camel_case_keys() {
        let json = serde_json::to_value(sample_admin_user()).unwrap();
        for key in [
            "id",
            "username",
            "displayName",
            "role",
            "enabled",
            "createdAt",
            "updatedAt",
            "lastLoginAt",
            "activeSessionCount",
        ] {
            assert!(
                json.get(key).is_some(),
                "AdminUser missing camelCase key `{key}`: {json}"
            );
        }
        // Spec §7.4 — passwordHash MUST NOT appear on the wire.
        assert!(json.get("passwordHash").is_none());
        assert!(json.get("password").is_none());
    }

    #[test]
    fn admin_user_create_serialises_with_camel_case_keys() {
        let body = AdminUserCreate {
            username: "moderator1".into(),
            password: "SecurePass123!".into(),
            display_name: "Moderator One".into(),
            role: Some("moderator".into()),
        };
        let raw = serde_json::to_string(&body).unwrap();
        assert!(raw.contains(r#""displayName":"Moderator One""#));
        assert!(!raw.contains("display_name"));
    }

    #[test]
    fn admin_user_patch_skips_absent_fields() {
        // Empty patch body — every field is None and should be omitted so the
        // server can reject the "no mutable fields supplied" case cleanly.
        let body = AdminUserPatch::default();
        let raw = serde_json::to_string(&body).unwrap();
        assert_eq!(raw, "{}");
    }

    #[test]
    fn admin_user_patch_round_trips_partial_payload() {
        let body = AdminUserPatch {
            enabled: Some(false),
            ..Default::default()
        };
        let raw = serde_json::to_string(&body).unwrap();
        assert!(raw.contains(r#""enabled":false"#));
        let back: AdminUserPatch = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.enabled, Some(false));
        assert!(back.role.is_none());
    }

    #[test]
    fn admin_session_serialises_with_camel_case_keys() {
        let s = AdminSession {
            id: 17,
            family: Some("fam-abc".into()),
            created_at: Utc.with_ymd_and_hms(2026, 5, 19, 13, 5, 14).unwrap(),
            expires_at: Utc.with_ymd_and_hms(2026, 5, 19, 14, 5, 14).unwrap(),
            replaced_by: None,
        };
        let json = serde_json::to_value(&s).unwrap();
        for key in ["id", "family", "createdAt", "expiresAt", "replacedBy"] {
            assert!(
                json.get(key).is_some(),
                "AdminSession missing key `{key}`: {json}"
            );
        }
        // The refresh-token byte string MUST NOT ride the wire. Construction
        // guarantees this (no `token` field exists).
        assert!(json.get("token").is_none());
    }

    #[test]
    fn audit_event_serialises_with_camel_case_keys() {
        let ev = AuditEvent {
            id: 42,
            occurred_at: Utc.with_ymd_and_hms(2026, 5, 19, 13, 8, 1).unwrap(),
            inserted_at: Utc.with_ymd_and_hms(2026, 5, 19, 13, 8, 1).unwrap(),
            actor_user_id: Some(1),
            actor_username: "admin".into(),
            kind: "userDisabled".into(),
            target_kind: Some("user".into()),
            target_id: Some(2),
            target_label: Some("moderator1".into()),
            payload: None,
            outcome: "success".into(),
            error_msg: None,
            request_ip: Some("192.0.2.10".into()),
            request_user_agent: Some("Mozilla/5.0".into()),
        };
        let json = serde_json::to_value(&ev).unwrap();
        for key in [
            "id",
            "occurredAt",
            "insertedAt",
            "actorUserId",
            "actorUsername",
            "kind",
            "targetKind",
            "targetId",
            "targetLabel",
            "payload",
            "outcome",
            "errorMsg",
            "requestIp",
            "requestUserAgent",
        ] {
            assert!(
                json.get(key).is_some(),
                "AuditEvent missing key `{key}`: {json}"
            );
        }
    }

    #[test]
    fn page_envelope_serialises_with_camel_case_keys() {
        let p: Page<AdminUser> = Page {
            items: vec![sample_admin_user()],
            total: 1,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&p).unwrap();
        for key in ["items", "total", "limit", "offset"] {
            assert!(json.get(key).is_some(), "Page missing key `{key}`: {json}");
        }
    }
}
