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
//! **Persistence (DB-backed audit table) is deferred to a follow-up child
//! issue under PURA-69 once SecurityEngineer has signed off on the field
//! list.** The structured-tracing emission lands first so audit coverage is
//! never missing while persistence is being designed.
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
}
