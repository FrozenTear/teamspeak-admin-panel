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
//! - **No plaintext SSH password, key blob, or API key may land in
//!   `command_line`.** Every public constructor pipes its incoming wire line
//!   through [`redact_credentials`] (PURA-83), which replaces the value of
//!   any `<name>=<value>` token whose name ends (case-insensitive) in
//!   `password`, `secret`, `token`, or `key` with `<redacted>` before the
//!   value is stored. The unredacted form is never assigned to
//!   [`AuditEntry::command_line`], so a future caller of `persist` can never
//!   re-leak it.
//! - **`error_msg` may surface upstream messages but never the raw key,
//!   key path, or agent socket.** Caller is responsible for substituting
//!   sensitive fragments before passing them in. (Per spec §6.10 and the
//!   PURA-83 scope, upstream `error id=…` messages don't echo input
//!   parameters today; revisit only if a TS6 build is observed echoing
//!   them.)

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
        let command_line = redact_credentials(&command_line.into());
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
        let command_line = redact_credentials(&command_line.into());
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
        let command_line = redact_credentials(&command_line.into());
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

/// Replace credential-bearing parameter values in a wire ServerQuery line.
///
/// Walks ASCII-whitespace-delimited tokens. For each `<name>=<value>` token
/// whose `<name>` ends (case-insensitive) in one of [`CREDENTIAL_SUFFIXES`]
/// (`password`, `secret`, `token`, `key`), `<value>` is replaced with
/// `<redacted>`. The boundary at the end of the value is the next ASCII
/// whitespace; per spec §10.4 the upstream escape table runs in
/// [`crate::sshbridge::wire`] before any line reaches this module, so a value
/// never itself contains an unescaped space — the whitespace boundary is
/// sufficient.
///
/// Command families enumerated by spec §10.4 / §6.10 that exercise this
/// redactor: `clientupdate client_login_password=…`,
/// `clientadd ..._password=…`, `tokendelete tokenkey=…`, `customset key=…`.
///
/// Tokens without `=`, tokens with non-credential names (`clid=`, `cid=`,
/// `sid=`, `cgid=`), and an `=` immediately preceded by whitespace (no name)
/// pass through untouched.
fn redact_credentials(input: &str) -> String {
    const REDACTED: &str = "<redacted>";
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ws_start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if ws_start != i {
            out.push_str(&input[ws_start..i]);
        }
        if i >= bytes.len() {
            break;
        }

        let tok_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let token = &input[tok_start..i];

        if let Some(eq_offset) = token.find('=') {
            let name = &token[..eq_offset];
            if name_is_credential(name) {
                out.push_str(name);
                out.push('=');
                out.push_str(REDACTED);
                continue;
            }
        }
        out.push_str(token);
    }

    out
}

/// Param-name suffixes whose value MUST be redacted before the audit line is
/// emitted or persisted. Kept small and conservative so non-credential params
/// (`clid=`, `cid=`, `sid=`, `cgid=`) pass through unchanged.
const CREDENTIAL_SUFFIXES: &[&str] = &["password", "secret", "token", "key"];

fn name_is_credential(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_entry_zero_exit_code_no_error_msg() {
        let e = AuditEntry::success(
            7,
            Some(3),
            Some(42),
            "clientlist -uid",
            Duration::from_millis(15),
        );
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

    // -----------------------------------------------------------------
    // PURA-83 — credential redaction.
    //
    // SecurityEngineer's contract (issue PURA-83): the value of any
    // `<name>=<value>` token whose name ends in (password|secret|token|key)
    // (case-insensitive) MUST be replaced with `<redacted>` before the line
    // is stored on `AuditEntry::command_line`. The `command_line` field is
    // the only place the wire line is kept, so once redaction lands at
    // construction time, neither `emit` nor `persist` can re-leak the secret.
    // -----------------------------------------------------------------

    #[test]
    fn redact_replaces_password_value_at_construction() {
        // The exact assertion from the issue's "Regression test" section.
        let e = AuditEntry::success(
            7,
            Some(3),
            Some(42),
            "clientupdate client_login_password=hunter2",
            Duration::from_millis(15),
        );
        assert!(e.command_line.contains("<redacted>"));
        assert!(!e.command_line.contains("hunter2"));
        assert_eq!(
            e.command_line,
            "clientupdate client_login_password=<redacted>"
        );
    }

    #[test]
    fn redact_clientadd_channel_and_server_password_pair() {
        // §10.4 line 3158-3170: clientadd carries two base64-sha1 password
        // params side-by-side. The base64 alphabet includes `=` padding, so
        // the redactor must terminate the value at whitespace, not at `=`.
        let r = redact_credentials(
            "clientadd client_default_channel_password=AAAA== client_server_password=BBBBB==",
        );
        assert!(!r.contains("AAAA"));
        assert!(!r.contains("BBBBB"));
        assert_eq!(
            r,
            "clientadd client_default_channel_password=<redacted> client_server_password=<redacted>",
        );
    }

    #[test]
    fn redact_tokendelete_tokenkey() {
        // §6.10 line 4164/4175: `tokenkey=…`. Name ends in `key` so the
        // value is redacted via the `key` suffix rule.
        let r = redact_credentials("tokendelete tokenkey=abcd1234secret");
        assert!(!r.contains("abcd1234secret"));
        assert_eq!(r, "tokendelete tokenkey=<redacted>");
    }

    #[test]
    fn redact_customset_key_param() {
        // `customset key=login_password value=…`. The `key=` token matches
        // the `key` suffix and gets redacted. The `value=` half is NOT
        // matched by the param-name rule alone (its name is `value`); per
        // PURA-83's "Out of scope" note we follow the literal name-suffix
        // rule. Command-specific value redaction (e.g. inferring the value
        // is a secret because `key=login_password`) is a separate follow-up
        // owned by SecurityEngineer if observed in practice.
        let r = redact_credentials("customset key=login_password value=hunter2");
        assert!(!r.contains("login_password"));
        assert!(r.contains("key=<redacted>"));
    }

    #[test]
    fn redact_value_with_internal_equals_stops_at_whitespace() {
        // Per §10.4 escape-applied wire lines never contain unescaped
        // whitespace inside a single value, so the boundary at the next
        // ASCII space is sufficient. A base64-padded value with `=` inside
        // must NOT cause the redactor to swallow the next param.
        let r = redact_credentials("a=1 password=AAA==BBB c=2");
        assert_eq!(r, "a=1 password=<redacted> c=2");
    }

    #[test]
    fn redact_is_case_insensitive_on_param_name() {
        let r = redact_credentials("clientupdate Client_Login_PASSWORD=hunter2");
        assert!(!r.contains("hunter2"));
        assert!(r.contains("Client_Login_PASSWORD=<redacted>"));
    }

    #[test]
    fn redact_leaves_innocuous_params_alone() {
        // Innocuous TS6 params that happen to use `=` must pass through.
        // `clid`, `cid`, `sid`, `cgid` — none ends in a credential suffix.
        let r = redact_credentials("clientmove clid=99 cid=2");
        assert_eq!(r, "clientmove clid=99 cid=2");
        let r2 = redact_credentials("use sid=1");
        assert_eq!(r2, "use sid=1");
    }

    #[test]
    fn redact_handles_empty_and_no_eq_tokens() {
        assert_eq!(redact_credentials(""), "");
        assert_eq!(redact_credentials("clientlist -uid"), "clientlist -uid");
        // A bare `=value` (no name) does NOT match — empty name is not a
        // credential suffix.
        assert_eq!(redact_credentials("foo =bar"), "foo =bar");
    }

    #[test]
    fn redact_applies_in_upstream_error_constructor() {
        let e = AuditEntry::upstream_error(
            1,
            None,
            None,
            "clientadd client_server_password=hunter2",
            2568,
            "no",
            Duration::from_millis(5),
        );
        assert!(!e.command_line.contains("hunter2"));
        assert!(e.command_line.contains("<redacted>"));
    }

    #[test]
    fn redact_applies_in_transport_constructor() {
        let e = AuditEntry::transport(
            1,
            None,
            None,
            "tokendelete tokenkey=abcd1234secret",
            "channel closed",
            Duration::from_millis(1),
        );
        assert!(!e.command_line.contains("abcd1234secret"));
        assert!(e.command_line.contains("<redacted>"));
    }

    #[test]
    fn redact_preserves_command_keyword_for_extract_command() {
        // After redaction the first whitespace token must still be the
        // command keyword so `extract_command` keeps grouping cheaply.
        let e = AuditEntry::success(
            1,
            None,
            None,
            "clientupdate client_login_password=hunter2",
            Duration::from_millis(1),
        );
        assert_eq!(e.command, "clientupdate");
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
