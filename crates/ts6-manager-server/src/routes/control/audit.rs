//! PURA-71 — structured audit log entry for every Phase 2 control action.
//!
//! Issue requirement: "All write endpoints emit a structured audit-log
//! entry." Mirrors the [`crate::sshbridge::audit`] precedent so operators
//! can grep both layers with the same shape — this module emits under
//! `target = "control::audit"`.
//!
//! **Persistence (DB-backed audit table) is deferred.** SecurityEngineer
//! has not signed off on the field list yet (same gate as `sshbridge::audit`);
//! a follow-up child issue under the Phase 2 epic will fold both audit
//! emitters into a shared persistent table once that lands.
//!
//! Emission contract: a write handler builds an entry on success or
//! upstream-error, then calls [`AuditEntry::emit`]. The entry itself is
//! never returned to the client.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Outcome bucket for an audited write. Mirrors the
/// [`crate::sshbridge::audit::AuditOutcome`] shape so log consumers can
/// pull both sources through the same dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Success,
    UpstreamError,
    Transport,
}

/// One audit entry. The `action` field is the canonical action keyword
/// (`client.kick`, `client.move`, `client.mute`, `client.unmute`,
/// `ban.add`, `ban.delete`); the `target_*` fields carry the relevant
/// numeric identifiers so downstream consumers can group cheaply by
/// `(action, target)` without re-parsing `details`.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub user_id: i64,
    pub username: String,
    pub action: &'static str,
    /// Free-form payload describing the operation (e.g.
    /// `clid=14 reasonid=5 reason="Spamming"`). MUST NOT carry
    /// credentials — control endpoints never see them.
    pub details: String,
    /// `clid` for client actions, `banid` for ban actions, `cid` for
    /// channel actions. `None` when the action targets the virtual
    /// server itself.
    pub target_id: Option<i64>,
    pub outcome: AuditOutcome,
    /// 0 on success; upstream `error id=<n>` value otherwise; `-1` on
    /// transport-level failure (matches WebQuery convention).
    pub exit_code: i64,
    /// Operator-friendly error string. Empty on success.
    pub error_msg: String,
    pub completed_at: DateTime<Utc>,
    pub latency: Duration,
}

// Each constructor takes the audit-row fields by value; bundling them into a
// "common context" struct would just push the same surface to every call
// site (handlers spread `server_config_id`/`virtual_server_id`/… into these
// directly from request state). Allow per-fn rather than refactoring the
// call sites in this lint-only pass.
#[allow(clippy::too_many_arguments)]
impl AuditEntry {
    pub fn success(
        server_config_id: i64,
        virtual_server_id: i64,
        user_id: i64,
        username: impl Into<String>,
        action: &'static str,
        target_id: Option<i64>,
        details: impl Into<String>,
        latency: Duration,
    ) -> Self {
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            username: username.into(),
            action,
            details: details.into(),
            target_id,
            outcome: AuditOutcome::Success,
            exit_code: 0,
            error_msg: String::new(),
            completed_at: Utc::now(),
            latency,
        }
    }

    pub fn upstream_error(
        server_config_id: i64,
        virtual_server_id: i64,
        user_id: i64,
        username: impl Into<String>,
        action: &'static str,
        target_id: Option<i64>,
        details: impl Into<String>,
        upstream_code: i64,
        upstream_msg: impl Into<String>,
        latency: Duration,
    ) -> Self {
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            username: username.into(),
            action,
            details: details.into(),
            target_id,
            outcome: AuditOutcome::UpstreamError,
            exit_code: upstream_code,
            error_msg: upstream_msg.into(),
            completed_at: Utc::now(),
            latency,
        }
    }

    pub fn transport(
        server_config_id: i64,
        virtual_server_id: i64,
        user_id: i64,
        username: impl Into<String>,
        action: &'static str,
        target_id: Option<i64>,
        details: impl Into<String>,
        error_msg: impl Into<String>,
        latency: Duration,
    ) -> Self {
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            username: username.into(),
            action,
            details: details.into(),
            target_id,
            outcome: AuditOutcome::Transport,
            exit_code: -1,
            error_msg: error_msg.into(),
            completed_at: Utc::now(),
            latency,
        }
    }

    /// Emit as a structured `tracing::info!` event under
    /// `target = "control::audit"`.
    pub fn emit(&self) {
        tracing::info!(
            target: "control::audit",
            server_config_id = self.server_config_id,
            virtual_server_id = self.virtual_server_id,
            user_id = self.user_id,
            username = %self.username,
            action = self.action,
            target_id = self.target_id,
            details = %self.details,
            exit_code = self.exit_code,
            outcome = ?self.outcome,
            error_msg = %self.error_msg,
            completed_at = %self.completed_at.to_rfc3339(),
            latency_ms = self.latency.as_millis() as u64,
            "control action"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_entry_zero_exit_code() {
        let e = AuditEntry::success(
            1,
            1,
            42,
            "alice",
            "client.kick",
            Some(7),
            "reasonid=5",
            Duration::from_millis(10),
        );
        assert_eq!(e.exit_code, 0);
        assert_eq!(e.outcome, AuditOutcome::Success);
        assert_eq!(e.action, "client.kick");
        assert_eq!(e.target_id, Some(7));
        assert!(e.error_msg.is_empty());
    }

    #[test]
    fn upstream_error_carries_code() {
        let e = AuditEntry::upstream_error(
            1,
            1,
            42,
            "alice",
            "client.move",
            Some(99),
            "cid=2",
            2568,
            "insufficient client permissions",
            Duration::from_millis(10),
        );
        assert_eq!(e.exit_code, 2568);
        assert_eq!(e.outcome, AuditOutcome::UpstreamError);
        assert_eq!(e.error_msg, "insufficient client permissions");
    }
}
