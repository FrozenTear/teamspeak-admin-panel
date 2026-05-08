//! PURA-69 — structured audit log entry for every command issued via the
//! SSH bridge.
//!
//! Issue requirement: "every command issued via SSHBridge gets a structured
//! audit log entry (user, server, command, exit code)." The audit record
//! is emitted as a `tracing::info!` event with named fields so the JSON
//! formatter (`tracing-subscriber` json layer, configured by
//! `crate::logging`) renders it as a single structured line. Operators
//! grep for `target=sshbridge::audit` to pull the audit slice out of the
//! main log stream.
//!
//! Persistence lives at [`AuditEntry::persist`] (PURA-79, follow-up D under
//! PURA-69). The DB write is **best-effort and fire-and-forget**: a failed
//! insert MUST NOT cancel the in-flight operator command. On any DB error
//! the entry is re-emitted under target `sshbridge::audit::persist_failed`
//! with every audit field plus the DB error so an operator who only
//! watches the DB never sees a silent log/DB divergence (R5). The
//! structured-tracing line in [`AuditEntry::emit`] still lands first, so
//! audit coverage is never missing.
//!
//! Two non-negotiables, enforced at construction time:
//!
//! - **No plaintext SSH password, key blob, or API key may be referenced
//!   in `command`.** [`AuditEntry::for_command`] takes only the wire
//!   ServerQuery line, which is constructed from public fields (`use sid=…`,
//!   `clientlist -uid`, etc.). Operator credentials never travel through
//!   command construction.
//! - **`error_msg` may surface upstream messages but never the raw key,
//!   key path, or agent socket.** Caller is responsible for substituting
//!   sensitive fragments before passing them in.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Outcome of one ServerQuery command issued over SSH.
///
/// **Source of truth.** `exit_code` is canonical; `outcome` is a derived
/// convenience field stored explicit so SurrealQL filtering on the
/// persisted log is ergonomic. A mismatch between the two indicates
/// corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// Upstream `error id=0 msg=ok`.
    Success,
    /// Upstream `error id=<n>` with `n != 0`. `exit_code` carries `n`.
    UpstreamError,
    /// Transport- or protocol-level failure before an `error` frame arrived.
    /// Reported as `exit_code = -1` to match the WebQuery side's convention
    /// (see [`crate::webquery::WebQueryError::upstream_code`]).
    Transport,
}

impl AuditOutcome {
    /// Lowercase enum-variant string used by the `outcome` column on the
    /// `ssh_audit_log` table (PURA-79). Kept on the type so the repo and
    /// downstream readers share one mapping.
    pub fn as_db_string(self) -> &'static str {
        match self {
            AuditOutcome::Success => "success",
            AuditOutcome::UpstreamError => "upstream_error",
            AuditOutcome::Transport => "transport",
        }
    }
}

/// One audit-log entry. Emit with [`AuditEntry::emit`].
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// `server_connection.id` — the configured server the command targeted.
    pub server_config_id: i64,
    /// Optional virtual-server id if the command was sid-scoped (after a
    /// `use sid=…`). `None` for instance-scoped commands.
    pub virtual_server_id: Option<i64>,
    /// Logged-in operator (`user.id`) on whose behalf the command was issued.
    /// `None` for system-initiated commands (boot, keepalive, reconnect).
    pub user_id: Option<i64>,
    /// First whitespace-delimited token of the wire ServerQuery line — the
    /// command keyword (`clientlist`, `serverinfo`, `clientmove`). Stored
    /// separately from `command` so audits group cheaply by command.
    pub command: String,
    /// Full wire ServerQuery line, with §10.4 escape table applied. MUST NOT
    /// contain credentials. The pool layer constructs commands from only
    /// public fields; password/private-key bytes never reach this struct.
    pub command_line: String,
    /// 0 on success; upstream `error id=<n>` value otherwise; `-1` on
    /// transport-level failure.
    pub exit_code: i64,
    /// Outcome bucket — derived from `exit_code` but kept explicit so the
    /// audit consumer doesn't have to redo the classification.
    pub outcome: AuditOutcome,
    /// Operator-friendly error string. Empty on success. Caller is
    /// responsible for stripping any sensitive fragment before passing in.
    pub error_msg: String,
    /// Wall-clock timestamp at which the command was acknowledged (success
    /// or error). Issued in UTC.
    pub completed_at: DateTime<Utc>,
    /// End-to-end latency from command issue to terminator arrival.
    pub latency: Duration,
}

impl AuditEntry {
    /// Construct an entry for a successful command.
    pub fn success(
        server_config_id: i64,
        virtual_server_id: Option<i64>,
        user_id: Option<i64>,
        command_line: impl Into<String>,
        latency: Duration,
    ) -> Self {
        let command_line = command_line.into();
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            command: extract_command(&command_line),
            command_line,
            exit_code: 0,
            outcome: AuditOutcome::Success,
            error_msg: String::new(),
            completed_at: Utc::now(),
            latency,
        }
    }

    /// Construct an entry for an upstream-rejected command (`error id != 0`).
    pub fn upstream_error(
        server_config_id: i64,
        virtual_server_id: Option<i64>,
        user_id: Option<i64>,
        command_line: impl Into<String>,
        upstream_code: i64,
        upstream_msg: impl Into<String>,
        latency: Duration,
    ) -> Self {
        let command_line = command_line.into();
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            command: extract_command(&command_line),
            command_line,
            exit_code: upstream_code,
            outcome: AuditOutcome::UpstreamError,
            error_msg: upstream_msg.into(),
            completed_at: Utc::now(),
            latency,
        }
    }

    /// Construct an entry for a transport-level failure (no `error` frame).
    pub fn transport(
        server_config_id: i64,
        virtual_server_id: Option<i64>,
        user_id: Option<i64>,
        command_line: impl Into<String>,
        error_msg: impl Into<String>,
        latency: Duration,
    ) -> Self {
        let command_line = command_line.into();
        Self {
            server_config_id,
            virtual_server_id,
            user_id,
            command: extract_command(&command_line),
            command_line,
            exit_code: -1,
            outcome: AuditOutcome::Transport,
            error_msg: error_msg.into(),
            completed_at: Utc::now(),
            latency,
        }
    }

    /// Emit the entry as a single structured `tracing::info!` event under
    /// `target = "sshbridge::audit"`. Operators grep this target to pull
    /// the audit stream out of the main log.
    ///
    /// Renders all fields by name. Latency is exported as milliseconds so
    /// downstream JSON consumers don't have to know the rust `Duration` shape.
    pub fn emit(&self) {
        tracing::info!(
            target: "sshbridge::audit",
            server_config_id = self.server_config_id,
            virtual_server_id = self.virtual_server_id,
            user_id = self.user_id,
            command = %self.command,
            command_line = %self.command_line,
            exit_code = self.exit_code,
            outcome = ?self.outcome,
            error_msg = %self.error_msg,
            completed_at = %self.completed_at.to_rfc3339(),
            latency_ms = self.latency.as_millis() as u64,
            "sshbridge command"
        );
    }

    /// Best-effort DB persist (PURA-79). Companion to [`Self::emit`].
    ///
    /// **Never returns a `Result`.** A DB-write failure MUST NOT cancel the
    /// in-flight operator command — that's the issue's hard rule. On any
    /// failure we fall back to a `tracing::warn!` under target
    /// `sshbridge::audit::persist_failed` carrying every audit field plus
    /// the DB error string, so an operator who only watches the DB does not
    /// see a silent log/DB divergence (R5).
    ///
    /// Callers should still call [`Self::emit`] **before** `persist`: the
    /// info-line landing first guarantees audit coverage is never lost on a
    /// DB outage.
    pub async fn persist(&self, db: &crate::db::Database) {
        if let Err(err) = self.persist_inner(db).await {
            self.emit_persist_failed(&err);
        }
    }

    async fn persist_inner(&self, db: &crate::db::Database) -> anyhow::Result<()> {
        crate::repos::ssh_audit_log::insert(
            db,
            crate::repos::ssh_audit_log::NewSshAuditLog {
                serverConfigId: self.server_config_id,
                virtualServerId: self.virtual_server_id,
                userId: self.user_id,
                command: self.command.clone(),
                commandLine: self.command_line.clone(),
                exitCode: self.exit_code,
                outcome: self.outcome.as_db_string().to_string(),
                errorMsg: self.error_msg.clone(),
                completedAt: self.completed_at,
                latencyMs: self.latency.as_millis() as i64,
            },
        )
        .await?;
        Ok(())
    }

    /// PURA-79 R5: re-emit every audit field under a distinct target on
    /// persist failure. **Every field of [`emit`] MUST be reproduced here**
    /// or an operator who only checks the DB sees a silent divergence;
    /// `tests::emit_persist_failed_carries_every_audit_field` enforces this
    /// at compile time via static source-grep.
    fn emit_persist_failed(&self, err: &anyhow::Error) {
        tracing::warn!(
            target: "sshbridge::audit::persist_failed",
            server_config_id = self.server_config_id,
            virtual_server_id = self.virtual_server_id,
            user_id = self.user_id,
            command = %self.command,
            command_line = %self.command_line,
            exit_code = self.exit_code,
            outcome = ?self.outcome,
            error_msg = %self.error_msg,
            completed_at = %self.completed_at.to_rfc3339(),
            latency_ms = self.latency.as_millis() as u64,
            audit_persist_status = "failed",
            db_error = %err,
            "sshbridge audit DB persist failed"
        );
    }
}

/// First whitespace-delimited token, as the canonical "command keyword"
/// audit grouping key.
fn extract_command(line: &str) -> String {
    line.split_whitespace().next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_entry_zero_exit_code_no_error_msg() {
        let e = AuditEntry::success(7, Some(3), Some(42), "clientlist -uid", Duration::from_millis(15));
        assert_eq!(e.server_config_id, 7);
        assert_eq!(e.virtual_server_id, Some(3));
        assert_eq!(e.user_id, Some(42));
        assert_eq!(e.command, "clientlist");
        assert_eq!(e.exit_code, 0);
        assert_eq!(e.outcome, AuditOutcome::Success);
        assert!(e.error_msg.is_empty());
    }

    #[test]
    fn upstream_error_carries_upstream_code() {
        let e = AuditEntry::upstream_error(
            1,
            Some(1),
            None,
            "clientmove clid=99 cid=2",
            2568,
            "insufficient client permissions",
            Duration::from_millis(20),
        );
        assert_eq!(e.command, "clientmove");
        assert_eq!(e.exit_code, 2568);
        assert_eq!(e.outcome, AuditOutcome::UpstreamError);
        assert_eq!(e.error_msg, "insufficient client permissions");
    }

    #[test]
    fn transport_error_uses_minus_one() {
        let e = AuditEntry::transport(
            1,
            None,
            None,
            "version",
            "channel closed by peer",
            Duration::from_millis(2),
        );
        assert_eq!(e.exit_code, -1);
        assert_eq!(e.outcome, AuditOutcome::Transport);
        assert_eq!(e.error_msg, "channel closed by peer");
    }

    #[test]
    fn extract_command_pulls_first_whitespace_token() {
        assert_eq!(extract_command("clientlist -uid -away"), "clientlist");
        assert_eq!(extract_command("use sid=3"), "use");
        assert_eq!(extract_command(""), "");
        assert_eq!(extract_command("   leading"), "leading");
    }

    #[test]
    fn outcome_db_string_mapping_is_lowercase_snake() {
        // PURA-79 — the on-disk `outcome` column uses these exact values.
        // A future contributor renaming a variant must update both ends.
        assert_eq!(AuditOutcome::Success.as_db_string(), "success");
        assert_eq!(AuditOutcome::UpstreamError.as_db_string(), "upstream_error");
        assert_eq!(AuditOutcome::Transport.as_db_string(), "transport");
    }

    /// PURA-79 R5: static source-grep ensures the persist-failed warn macro
    /// reproduces every audit field. If a future contributor drops a field
    /// (e.g. forgets to mirror a new field added to `AuditEntry`), the silent
    /// log/DB divergence SecurityEngineer flagged becomes a CI failure.
    #[test]
    fn emit_persist_failed_carries_every_audit_field() {
        const SRC: &str = include_str!("audit.rs");

        // Locate the function body.
        let start = SRC
            .find("fn emit_persist_failed")
            .expect("emit_persist_failed function must exist");
        let body = &SRC[start..];
        // Function body ends at the first `\n    }` after the open brace.
        // (The function lives inside `impl AuditEntry`, so 4-space indent.)
        let end = body
            .find("\n    }\n")
            .expect("emit_persist_failed body must close at 4-space indent");
        let body = &body[..end];

        // Every field that `emit()` records MUST also be in the failure
        // warn line — else an operator watching only the DB sees a silent
        // divergence on persist failure.
        let required_fields = [
            "server_config_id",
            "virtual_server_id",
            "user_id",
            "command",
            "command_line",
            "exit_code",
            "outcome",
            "error_msg",
            "completed_at",
            "latency_ms",
            "audit_persist_status",
            "db_error",
        ];
        for field in required_fields {
            assert!(
                body.contains(field),
                "emit_persist_failed missing required field `{field}` — \
                 PURA-79 R5 forbids silent log/DB divergence on persist failure"
            );
        }

        // Target string must be the spec-required one so operator tooling
        // can grep `sshbridge::audit::persist_failed` reliably.
        assert!(
            body.contains(r#"target: "sshbridge::audit::persist_failed""#),
            "emit_persist_failed must use target `sshbridge::audit::persist_failed`"
        );
    }

    /// PURA-79 hard-rule: a DB persist failure MUST NOT cancel the in-flight
    /// operator command. We simulate "DB unavailable" by connecting to an
    /// in-memory SurrealDB without applying migrations, so the
    /// `ssh_audit_log` table doesn't exist and the insert fails.
    #[tokio::test]
    async fn persist_swallows_db_errors_no_panic_no_propagate() {
        let db = crate::db::connect_in_memory()
            .await
            .expect("in-memory connect");
        // Note: NO `migrations::run` call — table is missing on purpose.
        let entry = AuditEntry::success(
            7,
            Some(3),
            Some(42),
            "clientlist -uid",
            Duration::from_millis(15),
        );
        // Returns `()` — no `?`, no panic.
        entry.persist(&db).await;
    }
}
