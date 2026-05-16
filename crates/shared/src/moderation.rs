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
/// `kick` / `ban` / `ban_ip` / `mute` / `unmute` / `note` / `resolve` /
/// `reopen`.
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
/// `ban` acts on the case subject UID and ignores it. `ban_ip` requires
/// `ip` instead. `reason` is always required — every timeline row carries
/// one (plan §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendActionRequest {
    pub action_kind: String,
    pub reason: String,
    #[serde(default)]
    pub clid: Option<i64>,
    /// IP address to ban — required for `ban_ip`, ignored by every other
    /// kind. A `ban_ip` action keys on the address, not the subject UID.
    #[serde(default)]
    pub ip: Option<String>,
    /// Ban duration in seconds. Absent / `0` requests a permanent ban.
    #[serde(default)]
    pub ban_duration_secs: Option<i64>,
}

/// `POST /api/moderation/cases/{id}/resolve` request body.
///
/// `falsePositive` is the Phase 9.1.4 automod affordance: when an operator
/// resolves an `origin = automod` case because the rule misfired, setting
/// it records `payload.falsePositive = true` on the `resolve` timeline
/// action so the per-rule metrics view can compute a false-positive rate.
/// It is absent / `false` for an ordinary resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCaseRequest {
    pub resolution_note: String,
    #[serde(default)]
    pub false_positive: Option<bool>,
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

/// One TS6 complaint in the moderation queue (`GET /api/moderation/complaints`).
///
/// A complaint is a `(tcldbid, fcldbid)` pair — the `t*` fields name the
/// **target** (the subject complained about), the `f*` fields name the
/// **complainant**. TS6 exposes no single complaint id, so the resolve
/// endpoint addresses a complaint by this pair rather than a path id.
/// Field names are the TS6 `complainlist` wire keys, preserved verbatim
/// (spec §7.15 is a passthrough surface).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Complaint {
    /// Target client-database id — the subject complained about.
    pub tcldbid: i64,
    /// Target client's last known nickname.
    pub tname: String,
    /// Complainant client-database id.
    pub fcldbid: i64,
    /// Complainant client's last known nickname.
    pub fname: String,
    pub message: String,
    /// Complaint creation time as a Unix timestamp (seconds).
    pub timestamp: i64,
}

/// `POST /api/moderation/complaints/resolve` request body.
///
/// With `fcldbid` present, dismisses the single complaint identified by
/// the `(tcldbid, fcldbid)` pair (`complaindel`). With `fcldbid` absent,
/// dismisses every complaint about the `tcldbid` subject
/// (`complaindelall`). `tcldbid` is always required — `complaindelall`
/// is per-target, not a vserver-wide purge (`9.0-spike`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveComplaintRequest {
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub tcldbid: i64,
    #[serde(default)]
    pub fcldbid: Option<i64>,
}

/// One row of the per-rule automod metrics view
/// (`GET /api/moderation/automod/metrics`) — Phase 9.1.4.
///
/// Aggregated over every `origin = automod` `moderation_case` whose
/// `originRef` carries this `ruleKey` (the `<ruleKey>:<flowId>` key the
/// 9.1.2 case bridge writes). This is the surface an operator reads to
/// decide whether to promote a rule from `shadow` to `enforce`:
///
/// - `actionsEnforced` / `shadowHits` — automod timeline actions split by
///   the safeguard `mode` recorded on each action's payload. A rule with
///   many shadow hits and few false positives is a promotion candidate.
/// - `falsePositives` — `resolve` actions flagged `payload.falsePositive`.
/// - `circuitBreakerTrips` — per-rule breaker trips (brief §6). Breaker
///   trips are not yet recorded to a queryable store, so this is always
///   `0` — the field is wired through for when that instrumentation
///   lands.
///
/// The false-positive *rate* is intentionally not on the wire — the view
/// derives it (`falsePositives / casesTotal`) so the division-by-zero
/// guard lives in one place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomodRuleMetrics {
    pub rule_key: String,
    pub cases_total: i64,
    pub actions_enforced: i64,
    pub shadow_hits: i64,
    pub false_positives: i64,
    pub circuit_breaker_trips: i64,
}

// ---------------------------------------------------------------------
// Phase 9.2 — public report / appeal surface (`/api/public/moderation/*`,
// PURA-307). These shapes back the *unauthenticated* routes: a reporter or
// an appealing subject is not an operator and has no account, so the
// request bodies carry an opaque single-use token instead of a JWT.
// ---------------------------------------------------------------------

/// `POST /api/public/moderation/request-report-link` request body.
///
/// A connected client asks the server to mint a `report_challenge_token`
/// and deliver it — over the TS6 control channel — to the client that
/// holds `uid`. The token is delivered to whoever actually controls the
/// UID, never to the HTTP caller, so a caller cannot harvest a token for
/// a UID they do not control (brief §2 / token spec hook 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestReportLinkRequest {
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    /// The TS6 `client_unique_identifier` to challenge. The poke is
    /// re-resolved to a live connection at delivery time.
    pub uid: String,
}

/// `POST /api/public/moderation/reports` request body. Requires a valid
/// `report_challenge_token`; the reporter UID is taken from the token, not
/// from the body. `evidence_url` is the optional URL field — no file
/// uploads on the 9.2 public surface (brief §4.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicReportRequest {
    /// The `report_challenge_token` wire string (`lookup.secret`).
    pub token: String,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    /// Who is being reported — a UID when known, else a free-text nickname.
    pub subject_uid_or_nickname: String,
    /// Report category — `spam` / `harassment` / `other` by convention.
    pub category: String,
    pub statement: String,
    #[serde(default)]
    pub evidence_url: Option<String>,
}

/// `POST /api/public/moderation/appeals` request body. Requires a valid
/// case-scoped `appeal_token`; the case and subject are taken from the
/// token, not from the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAppealRequest {
    /// The `appeal_token` wire string (`lookup.secret`).
    pub token: String,
    pub statement: String,
}

/// Response to an accepted public submission (`POST .../reports`,
/// `POST .../appeals`). `id` is the `moderation_report` / `moderation_appeal`
/// row id; `status` is its initial state (`pending`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSubmissionAccepted {
    pub id: i64,
    pub status: String,
}

/// One redacted timeline row in a [`RedactedCase`]. Carries only what the
/// appealing subject is allowed to see — the action kind, the reason text
/// shown to them at enforcement time, and when it happened. The operator,
/// the `payload`, and the TS6 ban-id linkage are all omitted (brief §6
/// hook 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedCaseAction {
    pub action_kind: String,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

/// `GET /api/public/moderation/case?token=…` body — the **redacted** view
/// of the case an `appeal_token` is scoped to.
///
/// Information-disclosure contract (brief §6 hook 4): this shape MUST omit
/// moderator notes, other subjects' UIDs, the internal `originRef`, the
/// acting operator, and the server-scope ids. It carries only the action
/// taken and the public reason — the minimum a subject needs to decide
/// whether and how to appeal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedCase {
    pub case_id: i64,
    /// Case state — `actioned` (appealable) or `appealed` (an appeal is
    /// already on file).
    pub status: String,
    /// The public reason recorded on the case.
    pub reason: String,
    pub opened_at: DateTime<Utc>,
    /// Whether this case can still be appealed — `true` only while
    /// `status = actioned` and no appeal is pending.
    pub appealable: bool,
    pub timeline: Vec<RedactedCaseAction>,
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
            ip: None,
            ban_duration_secs: Some(3600),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["actionKind"], "ban");
        assert_eq!(v["banDurationSecs"], 3600);
        let back: AppendActionRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn append_action_request_ban_ip_round_trips() {
        let req = AppendActionRequest {
            action_kind: "ban_ip".into(),
            reason: "open proxy abuse".into(),
            clid: None,
            ip: Some("203.0.113.7".into()),
            ban_duration_secs: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["actionKind"], "ban_ip");
        assert_eq!(v["ip"], "203.0.113.7");
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
    fn complaint_uses_ts6_native_wire_keys() {
        let c = Complaint {
            tcldbid: 5,
            tname: "Target".into(),
            fcldbid: 3,
            fname: "Reporter".into(),
            message: "spam".into(),
            timestamp: 1_700_000_000,
        };
        let v = serde_json::to_value(&c).unwrap();
        // Wire keys are the TS6 `complainlist` names verbatim — no
        // camelCase rename (spec §7.15 passthrough surface).
        assert!(v.get("tcldbid").is_some());
        assert!(v.get("fcldbid").is_some());
        assert!(v.get("tname").is_some());
        let back: Complaint = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn resolve_complaint_request_fcldbid_is_optional() {
        // `complaindelall` form — no fcldbid.
        let req: ResolveComplaintRequest = serde_json::from_value(serde_json::json!({
            "serverConfigId": 1,
            "virtualServerId": 1,
            "tcldbid": 5
        }))
        .unwrap();
        assert_eq!(req.tcldbid, 5);
        assert!(req.fcldbid.is_none());
        // `complaindel` form — fcldbid present.
        let req: ResolveComplaintRequest = serde_json::from_value(serde_json::json!({
            "serverConfigId": 1,
            "virtualServerId": 1,
            "tcldbid": 5,
            "fcldbid": 3
        }))
        .unwrap();
        assert_eq!(req.fcldbid, Some(3));
    }

    #[test]
    fn resolve_request_false_positive_is_optional() {
        // Ordinary resolve — no `falsePositive` key.
        let req: ResolveCaseRequest = serde_json::from_value(serde_json::json!({
            "resolutionNote": "warned and closed"
        }))
        .unwrap();
        assert!(req.false_positive.is_none());
        // Automod false-positive resolve.
        let req = ResolveCaseRequest {
            resolution_note: "rule misfired".into(),
            false_positive: Some(true),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["falsePositive"], true);
        let back: ResolveCaseRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn automod_rule_metrics_uses_camelcase_wire_keys() {
        let m = AutomodRuleMetrics {
            rule_key: "bad-name".into(),
            cases_total: 12,
            actions_enforced: 9,
            shadow_hits: 3,
            false_positives: 2,
            circuit_breaker_trips: 0,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("ruleKey").is_some());
        assert!(v.get("casesTotal").is_some());
        assert!(v.get("actionsEnforced").is_some());
        assert!(v.get("circuitBreakerTrips").is_some());
        let back: AutomodRuleMetrics = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn error_body_skips_absent_optionals() {
        let v = serde_json::to_value(ErrorBody::new("nope")).unwrap();
        assert_eq!(v, serde_json::json!({ "error": "nope" }));
        let v = serde_json::to_value(ErrorBody::new("nope").with_code("not_found")).unwrap();
        assert_eq!(v["code"], "not_found");
    }
}
