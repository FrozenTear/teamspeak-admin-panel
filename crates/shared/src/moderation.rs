//! Wire-format types for the Phase 9.0 moderation surface — PURA-286.
//!
//! These shapes back the `/api/moderation/*` REST endpoints (case queue,
//! case detail + timeline, per-subject history, moderator notes). The
//! `9.0-data` document records (`moderation_case`, `moderation_case_action`,
//! `moderation_note`) carry camelCase field names verbatim from the design
//! brief §5 because they double as JSON wire keys; these DTOs reproduce
//! that key set via `#[serde(rename_all = "camelCase")]` so the server
//! repo rows and the Dioxus client agree byte-for-byte.
//!
//! The crate stays WASM-clean: `chrono` is the only non-trivial dependency
//! and it is already a shared-crate dependency (see `admin`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A moderation case — one actioned subject on a virtual server. Mirrors
/// the `moderation_case` document (brief §5). `status` is one of
/// `open` / `actioned` / `resolved`; `origin` one of
/// `operator` / `complaint` / `automod`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationCase {
    pub id: i64,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub subject_uid: String,
    pub subject_nickname_snapshot: String,
    pub origin: String,
    pub origin_ref: Option<String>,
    pub status: String,
    pub reason: String,
    pub resolution_note: Option<String>,
    pub opened_by_user_id: Option<i64>,
    pub opened_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// One row of a case's append-only action timeline. `actionKind` is one of
/// `kick` / `ban` / `mute` / `unmute` / `note` / `resolve` / `reopen`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationCaseAction {
    pub id: i64,
    pub case_id: i64,
    pub actor_user_id: Option<i64>,
    pub actor_username_snapshot: String,
    pub action_kind: String,
    pub reason: String,
    /// TS6 ban-id linkage when the action produced a server-side ban.
    pub ts_ref: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// A free-text moderator note on a subject UID, independent of cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationNote {
    pub id: i64,
    pub subject_uid: String,
    pub body: String,
    pub author_user_id: Option<i64>,
    pub author_username_snapshot: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `GET /api/moderation/cases/{id}` body — the case plus its full
/// chronological action timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseDetail {
    pub case: ModerationCase,
    pub timeline: Vec<ModerationCaseAction>,
}

/// `POST /api/moderation/cases` request body. `origin` defaults to
/// `operator` server-side when omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCaseRequest {
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub subject_uid: String,
    pub subject_nickname_snapshot: String,
    pub reason: String,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub origin_ref: Option<String>,
}

/// `POST /api/moderation/cases/{id}/actions` request body. `clid` is
/// required for `kick` / `mute` / `unmute` (those act on a live client);
/// `ban` acts on the case subject UID and ignores it. `reason` is always
/// required — every timeline row carries one (plan §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendActionRequest {
    pub action_kind: String,
    pub reason: String,
    #[serde(default)]
    pub clid: Option<i64>,
    /// Ban duration in seconds. Absent / `0` requests a permanent ban.
    #[serde(default)]
    pub ban_duration_secs: Option<i64>,
}

/// `POST /api/moderation/cases/{id}/resolve` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCaseRequest {
    pub resolution_note: String,
}

/// `POST /api/moderation/cases/{id}/reopen` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReopenCaseRequest {
    pub reason: String,
}

/// `GET /api/moderation/subjects/{uid}/history` body — every case,
/// action, and note for one subject UID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectHistory {
    pub subject_uid: String,
    pub cases: Vec<ModerationCase>,
    pub actions: Vec<ModerationCaseAction>,
    pub notes: Vec<ModerationNote>,
}

/// `POST /api/moderation/subjects/{uid}/notes` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateNoteRequest {
    pub body: String,
}

/// Error envelope for the moderation surface. Matches the per-surface
/// `ErrorBody` shape used by the control and music-bot routes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ErrorBody {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: None,
            details: None,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_serializes_with_camelcase_wire_keys() {
        let c = ModerationCase {
            id: 1,
            server_config_id: 3,
            virtual_server_id: 1,
            subject_uid: "uid-a".into(),
            subject_nickname_snapshot: "Nick".into(),
            origin: "operator".into(),
            origin_ref: None,
            status: "open".into(),
            reason: "spam".into(),
            resolution_note: None,
            opened_by_user_id: Some(7),
            opened_at: Utc::now(),
            updated_at: Utc::now(),
            resolved_at: None,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("serverConfigId").is_some());
        assert!(v.get("subjectUid").is_some());
        assert!(v.get("openedByUserId").is_some());
        let back: ModerationCase = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn append_action_request_round_trips() {
        let req = AppendActionRequest {
            action_kind: "ban".into(),
            reason: "repeated spam".into(),
            clid: None,
            ban_duration_secs: Some(3600),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["actionKind"], "ban");
        assert_eq!(v["banDurationSecs"], 3600);
        let back: AppendActionRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn open_case_request_origin_is_optional() {
        let req: OpenCaseRequest = serde_json::from_value(serde_json::json!({
            "serverConfigId": 1,
            "virtualServerId": 1,
            "subjectUid": "uid-a",
            "subjectNicknameSnapshot": "Nick",
            "reason": "spam"
        }))
        .unwrap();
        assert!(req.origin.is_none());
        assert!(req.origin_ref.is_none());
    }

    #[test]
    fn error_body_skips_absent_optionals() {
        let v = serde_json::to_value(ErrorBody::new("nope")).unwrap();
        assert_eq!(v, serde_json::json!({ "error": "nope" }));
        let v = serde_json::to_value(ErrorBody::new("nope").with_code("not_found")).unwrap();
        assert_eq!(v["code"], "not_found");
    }
}
