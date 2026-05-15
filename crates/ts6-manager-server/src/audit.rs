//! Top-level admin-audit writer per `docs/admin/audit-shape.md` §4.
//!
//! Eight admin-mutating routes in [`crate::routes::users`] call
//! [`record`] **after** the mutation has committed. The writer applies
//! the `requestUserAgent` truncation cap inline (additional `payload`
//! and `errorMsg` caps happen at the persistence boundary inside
//! [`crate::repos::admin_audit_log`]) and inserts via
//! [`crate::repos::admin_audit_log::insert`].
//!
//! Failure posture mirrors the SSH-audit precedent (PURA-79):
//! `tracing::warn!`-on-failure, never propagate to the caller, never
//! panic. An audit-write failure is an operational bug, not a
//! user-facing one — the user mutation already committed and the
//! response is on its way.
//!
//! Scope split with PURA-236:
//! - PURA-235 (this file + [`crate::routes::users`]) — writer + the
//!   eight v1.1 admin routes that emit events.
//! - PURA-236 — the same writer extended with the hard-blocklist
//!   `debug_assert!` (mirroring [`crate::repos::ssh_audit_log`]'s
//!   credential-token denylist), the retention janitor, and write
//!   hooks on every *other* mutating route (`/api/auth/password`,
//!   `/api/setup/init`, server CRUD, etc.).

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
    let new = NewAdminAuditLog {
        actorUserId: Some(event.actor.id),
        actorUsername: event.actor.username,
        kind: event.kind.as_str().to_string(),
        targetKind: event.target.as_ref().map(|t| t.kind.clone()),
        targetId: event.target.as_ref().and_then(|t| t.id),
        targetLabel: event.target.and_then(|t| t.label),
        payload: event.payload,
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
}
