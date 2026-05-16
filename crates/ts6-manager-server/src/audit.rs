//! Top-level admin-audit writer per `docs/admin/audit-shape.md` §4.
//!
//! Every admin-mutating route calls [`record`] **after** the mutation
//! has committed. [`record`] applies the §2.3 credential hard-blocklist
//! (see [`redaction`]) to the payload, then inserts via
//! [`crate::repos::admin_audit_log::insert`] — which applies the §2.3
//! size caps (`payload` 2 KiB, `errorMsg`, `requestUserAgent`).
//!
//! Failure posture mirrors the SSH-audit precedent (PURA-79):
//! `tracing::warn!`-on-failure, never propagate to the caller, never
//! panic. An audit-write failure is an operational bug, not a
//! user-facing one — the user mutation already committed and the
//! response is on its way.
//!
//! Scope history (PURA-235 + PURA-236, both A-* siblings of PURA-228):
//! - PURA-235 — this writer + the eight `/api/users*` routes that emit
//!   events (`userCreated`/`userPatched`/`userDisabled`/`userEnabled`/
//!   `userRoleChanged`/`userPasswordReset`/`userDeleted`/`sessionRevoked`).
//! - PURA-236 — the [`redaction`] credential hard-blocklist wired into
//!   [`record`], the [`retention`] janitor (§3), and the remaining two
//!   route hooks: `selfPasswordChanged` (`PUT /api/auth/password`) and
//!   `setupCompleted` (`POST /api/setup/init`). Server-connection and
//!   direct-WebQuery audit are explicitly **v1.2** per audit-shape.md
//!   §2.1 — not wired here.

// PURA-236 submodules: the §2.3 credential hard-blocklist applied inside
// [`record`], and the retention janitor (§3) spawned at server boot.
pub mod redaction;
pub mod retention;

use crate::auth::extractors::{AuthUser, RequestMeta};
use crate::db::Database;
use crate::repos::admin_audit_log::{self, NewAdminAuditLog};

/// Event-kind discriminant per `docs/admin/audit-shape.md` §2.1. The
/// `as_str` mapping is the **wire-stable identifier** — renaming a
/// variant is a breaking change for forensic queries.
///
/// `SelfPasswordChanged` and `SetupCompleted` are emitted by the auth /
/// setup surfaces (`PUT /api/auth/password`, `POST /api/setup/init`),
/// which PURA-236 wires up — the variants are declared here so the full
/// §2.1 taxonomy lives in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AuditKind {
    UserCreated,
    UserPatched,
    UserDisabled,
    UserEnabled,
    UserRoleChanged,
    UserPasswordReset,
    UserDeleted,
    SessionRevoked,
    SelfPasswordChanged,
    SetupCompleted,
    UserPermissionsChanged,
    /// Phase 9.0 moderation (PURA-286) — case lifecycle + note events.
    ModerationCaseOpened,
    ModerationCaseActioned,
    ModerationCaseResolved,
    ModerationCaseReopened,
    ModerationNoteAdded,
    /// Phase 9.0 complaint sub-surface (PURA-289) — a TS6 complaint was
    /// dismissed via `complaindel` / `complaindelall`.
    ModerationComplaintResolved,
    /// Phase 9.1 automod (PURA-301) — a `Moderate` flow node opened or
    /// reused a case and applied a moderation effect.
    ModerationAutomodAction,
    /// Phase 9.2 appeals (PURA-308) — an operator promoted a
    /// `moderation_report` to a case, dismissed a report without opening
    /// one, or decided (upheld / overturned) an appeal.
    ModerationReportPromoted,
    ModerationReportDismissed,
    ModerationAppealDecided,
}

impl AuditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditKind::UserCreated => "userCreated",
            AuditKind::UserPatched => "userPatched",
            AuditKind::UserDisabled => "userDisabled",
            AuditKind::UserEnabled => "userEnabled",
            AuditKind::UserRoleChanged => "userRoleChanged",
            AuditKind::UserPasswordReset => "userPasswordReset",
            AuditKind::UserDeleted => "userDeleted",
            AuditKind::SessionRevoked => "sessionRevoked",
            AuditKind::SelfPasswordChanged => "selfPasswordChanged",
            AuditKind::SetupCompleted => "setupCompleted",
            AuditKind::UserPermissionsChanged => "userPermissionsChanged",
            AuditKind::ModerationCaseOpened => "moderationCaseOpened",
            AuditKind::ModerationCaseActioned => "moderationCaseActioned",
            AuditKind::ModerationCaseResolved => "moderationCaseResolved",
            AuditKind::ModerationCaseReopened => "moderationCaseReopened",
            AuditKind::ModerationNoteAdded => "moderationNoteAdded",
            AuditKind::ModerationComplaintResolved => "moderationComplaintResolved",
            AuditKind::ModerationAutomodAction => "moderationAutomodAction",
            AuditKind::ModerationReportPromoted => "moderationReportPromoted",
            AuditKind::ModerationReportDismissed => "moderationReportDismissed",
            AuditKind::ModerationAppealDecided => "moderationAppealDecided",
        }
    }
}

/// Audit-row target descriptor. The pair (`kind`, `id`) is indexed via
/// `admin_audit_log_target_idx`; `label` is a forensic snapshot of the
/// target's human-readable name at event time so the row stays
/// readable after the target row is renamed or deleted.
#[derive(Debug, Clone)]
pub struct Target {
    pub kind: String,
    pub id: Option<i64>,
    pub label: Option<String>,
}

impl Target {
    pub fn user(id: i64, username: impl Into<String>) -> Self {
        Self {
            kind: "user".to_string(),
            id: Some(id),
            label: Some(username.into()),
        }
    }

    pub fn session(id: i64) -> Self {
        Self {
            kind: "session".to_string(),
            id: Some(id),
            label: None,
        }
    }

    /// Phase 9.0 — a moderation case. `label` snapshots the subject UID
    /// so the audit row stays meaningful after the case is deleted.
    pub fn moderation_case(id: i64, subject_uid: impl Into<String>) -> Self {
        Self {
            kind: "moderation_case".to_string(),
            id: Some(id),
            label: Some(subject_uid.into()),
        }
    }

    /// Phase 9.0 — a moderation subject keyed by UID. `id` is `None`
    /// because a subject UID is not an integer row id.
    pub fn moderation_subject(subject_uid: impl Into<String>) -> Self {
        Self {
            kind: "moderation_subject".to_string(),
            id: None,
            label: Some(subject_uid.into()),
        }
    }

    /// Phase 9.2 — a report-intake row (`moderation_report`). `label`
    /// snapshots the accused subject so the audit row stays meaningful
    /// after the report is purged.
    pub fn moderation_report(id: i64, subject: impl Into<String>) -> Self {
        Self {
            kind: "moderation_report".to_string(),
            id: Some(id),
            label: Some(subject.into()),
        }
    }

    /// Phase 9.0 — a TS6 complaint target. `id` is the target
    /// client-database id (`tcldbid`); a complaint has no single id of
    /// its own, so the target subject is the forensically useful key.
    pub fn moderation_complaint(tcldbid: i64) -> Self {
        Self {
            kind: "moderation_complaint".to_string(),
            id: Some(tcldbid),
            label: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Success,
    /// v1.1 only ever writes `success` rows — failure-path audit is
    /// deferred to v1.2 per `docs/admin/audit-shape.md` §1.2. The variant
    /// exists so the `outcome` column already models the eventual state.
    #[allow(dead_code)]
    Failure,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
        }
    }
}

/// Convenience builder for a single audit row.
#[derive(Debug, Clone)]
pub struct Event {
    pub actor: AuthUser,
    pub kind: AuditKind,
    pub target: Option<Target>,
    pub payload: Option<serde_json::Value>,
    pub outcome: Outcome,
    pub error_msg: Option<String>,
    pub request: RequestMeta,
}

/// Insert one audit row. Best-effort: failures are logged at `warn` but
/// never propagated. Callers should NOT `await` for write completion in
/// a hot path that gates the user response — they typically call this
/// right before returning the success response.
pub async fn record(db: &Database, event: Event) {
    // PURA-236 §2.3 hard-blocklist: rewrite any credential-shaped payload
    // key to the redacted sentinel before the row is built. The §2.3
    // size caps (payload 2 KiB, errorMsg / userAgent) are applied
    // separately at the persistence boundary inside
    // `admin_audit_log::insert`.
    let payload = event.payload.map(|mut p| {
        redaction::redact_payload(&mut p);
        p
    });
    let new = NewAdminAuditLog {
        actorUserId: Some(event.actor.id),
        actorUsername: event.actor.username,
        kind: event.kind.as_str().to_string(),
        targetKind: event.target.as_ref().map(|t| t.kind.clone()),
        targetId: event.target.as_ref().and_then(|t| t.id),
        targetLabel: event.target.and_then(|t| t.label),
        payload,
        outcome: event.outcome.as_str().to_string(),
        errorMsg: event.error_msg,
        requestIp: event.request.ip,
        requestUserAgent: event.request.user_agent,
    };
    let kind = new.kind.clone();
    if let Err(e) = admin_audit_log::insert(db, new).await {
        tracing::warn!(
            error = %e,
            kind = %kind,
            "admin_audit_log: write failed (best-effort, not propagated)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_kind_as_str_is_wire_stable() {
        // Pin every kind string — these are the external contract for
        // forensic queries. Renaming a variant must show up here.
        assert_eq!(AuditKind::UserCreated.as_str(), "userCreated");
        assert_eq!(AuditKind::UserPatched.as_str(), "userPatched");
        assert_eq!(AuditKind::UserDisabled.as_str(), "userDisabled");
        assert_eq!(AuditKind::UserEnabled.as_str(), "userEnabled");
        assert_eq!(AuditKind::UserRoleChanged.as_str(), "userRoleChanged");
        assert_eq!(AuditKind::UserPasswordReset.as_str(), "userPasswordReset");
        assert_eq!(AuditKind::UserDeleted.as_str(), "userDeleted");
        assert_eq!(AuditKind::SessionRevoked.as_str(), "sessionRevoked");
        assert_eq!(
            AuditKind::SelfPasswordChanged.as_str(),
            "selfPasswordChanged"
        );
        assert_eq!(AuditKind::SetupCompleted.as_str(), "setupCompleted");
    }

    #[test]
    fn target_user_carries_label_and_kind() {
        let t = Target::user(42, "alice");
        assert_eq!(t.kind, "user");
        assert_eq!(t.id, Some(42));
        assert_eq!(t.label.as_deref(), Some("alice"));
    }

    #[test]
    fn target_session_has_no_label() {
        // Sessions are anonymous — only the id is forensically useful;
        // the `family` lives in the payload.
        let t = Target::session(17);
        assert_eq!(t.kind, "session");
        assert_eq!(t.id, Some(17));
        assert!(t.label.is_none());
    }

    fn dummy_actor() -> AuthUser {
        AuthUser {
            id: 1,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: "admin".into(),
            enabled: true,
        }
    }

    /// PURA-236 §2.3 — `record` runs the credential hard-blocklist before
    /// the row is persisted. Plant `password`/`apiKey`/`sshKey` in the
    /// payload and read the row back: the values must be the redacted
    /// sentinel, never the plaintext.
    #[tokio::test]
    async fn record_redacts_credentials_before_persist() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        record(
            &db,
            Event {
                actor: dummy_actor(),
                kind: AuditKind::UserPasswordReset,
                target: Some(Target::user(2, "bob")),
                payload: Some(serde_json::json!({
                    "password": "hunter2",
                    "apiKey": "k-aaa",
                    "sshKey": "k-bbb",
                    "sessionsRevoked": 3,
                })),
                outcome: Outcome::Success,
                error_msg: None,
                request: RequestMeta::default(),
            },
        )
        .await;

        let (rows, total) =
            admin_audit_log::list(&db, &admin_audit_log::ListFilter::default(), 10, 0)
                .await
                .unwrap();
        assert_eq!(total, 1);
        let payload = rows[0].payload.clone().expect("payload present");
        for key in ["password", "apiKey", "sshKey"] {
            assert_eq!(
                payload[key],
                serde_json::json!(redaction::REDACTED_SENTINEL),
                "{key} must be redacted before persist"
            );
        }
        assert_eq!(payload["sessionsRevoked"], serde_json::json!(3));
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(!raw.contains("hunter2"));
        assert!(!raw.contains("k-aaa"));
        assert!(!raw.contains("k-bbb"));
    }

    /// Best-effort posture — a DB-write failure must not panic or
    /// propagate. Simulated with an in-memory DB that never ran
    /// migrations, so the `admin_audit_log` table is missing.
    #[tokio::test]
    async fn record_swallows_db_errors_no_panic() {
        let db = crate::db::connect_in_memory().await.unwrap();
        // No migrations — table absent on purpose.
        record(
            &db,
            Event {
                actor: dummy_actor(),
                kind: AuditKind::UserCreated,
                target: Some(Target::user(2, "bob")),
                payload: Some(serde_json::json!({"role": "moderator"})),
                outcome: Outcome::Success,
                error_msg: None,
                request: RequestMeta::default(),
            },
        )
        .await;
    }
}
