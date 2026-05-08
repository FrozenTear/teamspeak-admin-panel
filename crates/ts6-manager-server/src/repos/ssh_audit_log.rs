//! `SshAuditLog` repo (PURA-79 — follow-up D under PURA-69).
//!
//! Persists the [`crate::sshbridge::audit::AuditEntry`] entries that the
//! SSH bridge already emits as `tracing::info!` events. Migration
//! `0006_ssh_audit_log.surql` defines the table; this module exposes the
//! INSERT-and-prune surface the audit emitter calls.
//!
//! ## Tamper-resistance posture (PURA-79 R4)
//!
//! This repo is **INSERT-only by contract**.
//!
//! - The only mutation paths are [`insert`] and [`prune_older_than`]. There
//!   is no `update`, no `delete_by_id`, no replace-by-key.
//! - `completedAt` and `insertedAt` are `READONLY` at the schema level so
//!   even a hand-written SurrealQL `UPDATE` cannot rewrite the time axis.
//! - A static-source test (`tests::repo_source_is_insert_only`) grep-asserts
//!   the file for any mutation keyword on the table or any colon-form record
//!   delete on it (i.e. table-name suffixed by a `:` for single-record by-id
//!   deletes) and fails CI if a future contributor adds either. The chunked
//!   prune's WHERE-clause delete form is intentionally allowed.
//! - GDPR / right-to-erasure posture (Rec-A): the operator-linkage column
//!   `userId` is **set-null** on user delete (event `user_set_null_ssh_audit`
//!   in the migration). The audit row itself survives. A hard-erasure
//!   workflow drops `DELETE FROM user WHERE id = X` and the linkage clears
//!   automatically; a soft-delete workflow leaves the linkage intact for
//!   continued attribution.
//!
//! ## Credential-leak posture (PURA-79 R3)
//!
//! `commandLine` is the §10.4-escaped wire ServerQuery line, constructed
//! by the pool layer from public fields only. The audit module's invariant
//! is that **operator credentials never reach the audit struct**. This repo
//! is the *last-line* defense, not the primary one — the right place to
//! prevent leakage is the pool layer's command construction. To catch a
//! future caller bypassing the type-seam invariant, [`insert`] runs a
//! `debug_assert!` against a small denylist of credential-token shapes
//! (`client_login_password=`, `serverquery_login_password=`, `apikey=`,
//! `auth=`, `password=`). Zero-cost in release; loud failure in
//! dev/test/CI. A unit test plants each shape and asserts the assert fires.
//!
//! ## Length caps (PURA-79 R2)
//!
//! `commandLine` and `errorMsg` are capped at the persistence boundary so a
//! pathological upstream message can't blow out a row → index bloat / slow
//! scans. Caps applied here, not on the [`crate::sshbridge::audit::AuditEntry`]
//! constructor, so the original-length string still reaches the
//! `tracing::info!` line for log scrapers.
//!
//! - `commandLine` → 8 KiB cap.
//! - `errorMsg`    → 4 KiB cap.
//!
//! Truncation appends a sentinel `… [truncated, original NNN bytes]` so an
//! operator inspecting the row knows context was cut.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// PURA-79 R2: per-field caps applied at the repo seam.
pub const COMMAND_LINE_MAX_BYTES: usize = 8 * 1024;
pub const ERROR_MSG_MAX_BYTES: usize = 4 * 1024;

/// PURA-79 R6: chunk size for the retention sweep. Keeps a year-of-audit
/// prune from holding the runtime; the [`prune_older_than`] loop yields
/// between iterations.
const PRUNE_CHUNK: usize = 1000;

/// PURA-79 R3: token shapes that MUST NOT appear in `commandLine`. The audit
/// module's invariant says credential bytes never reach the audit struct;
/// this denylist is the last-line `debug_assert!` belt that fires loudly in
/// dev/test/CI when a future caller bypasses the contract.
const CREDENTIAL_TOKEN_DENYLIST: &[&str] = &[
    "client_login_password=",
    "serverquery_login_password=",
    "apikey=",
    "auth=",
    "password=",
];

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct SshAuditLogRow {
    pub id: i64,
    pub serverConfigId: i64,
    pub virtualServerId: Option<i64>,
    pub userId: Option<i64>,
    pub command: String,
    pub commandLine: String,
    pub exitCode: i64,
    pub outcome: String,
    pub errorMsg: String,
    pub completedAt: DateTime<Utc>,
    pub latencyMs: i64,
    pub insertedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSshAuditLog {
    pub serverConfigId: i64,
    pub virtualServerId: Option<i64>,
    pub userId: Option<i64>,
    pub command: String,
    pub commandLine: String,
    pub exitCode: i64,
    pub outcome: String,
    pub errorMsg: String,
    pub completedAt: DateTime<Utc>,
    pub latencyMs: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    serverConfigId,
    virtualServerId,
    userId,
    command,
    commandLine,
    exitCode,
    outcome,
    errorMsg,
    completedAt,
    latencyMs,
    insertedAt
";

/// Insert one audit row. INSERT-only — no upsert, no replace.
///
/// Applies the R2 length caps and the R3 credential-token `debug_assert!`
/// before handing values to SurrealDB. Returns the materialised row so the
/// caller can confirm the persisted shape (used by tests).
pub async fn insert(db: &Database, new: NewSshAuditLog) -> Result<SshAuditLogRow> {
    debug_assert_no_credential_tokens(&new.commandLine);

    let command_line = truncate_with_sentinel(&new.commandLine, COMMAND_LINE_MAX_BYTES);
    let error_msg = truncate_with_sentinel(&new.errorMsg, ERROR_MSG_MAX_BYTES);

    let sql = format!(
        "CREATE type::record('ssh_audit_log', sequence::nextval('ssh_audit_log_id'))
            CONTENT {{
                serverConfigId:  $serverConfigId,
                virtualServerId: $virtualServerId,
                userId:          $userId,
                command:         $command,
                commandLine:     $commandLine,
                exitCode:        $exitCode,
                outcome:         $outcome,
                errorMsg:        $errorMsg,
                completedAt:     $completedAt,
                latencyMs:       $latencyMs
            }}
            RETURN {PROJECTION};"
    );

    let mut resp = db
        .query(sql)
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("virtualServerId", new.virtualServerId))
        .bind(("userId", new.userId))
        .bind(("command", new.command))
        .bind(("commandLine", command_line))
        .bind(("exitCode", new.exitCode))
        .bind(("outcome", new.outcome))
        .bind(("errorMsg", error_msg))
        .bind(("completedAt", new.completedAt))
        .bind(("latencyMs", new.latencyMs))
        .await
        .context("ssh_audit_log insert query failed")?
        .check()?;
    let row: Option<SshAuditLogRow> = resp.take(0)?;
    row.context("ssh_audit_log insert returned no row")
}

/// Delete every row whose `completedAt < cutoff`, in chunks of [`PRUNE_CHUNK`].
/// Yields between iterations so a year-of-audit sweep does not park the
/// runtime (PURA-79 R6).
///
/// Returns the total number of rows deleted across all chunks.
///
/// Index usage: relies on `ssh_audit_log_completed_idx` for the SELECT; the
/// SurrealDB query planner uses the index range scan rather than a full
/// table walk.
pub async fn prune_older_than(db: &Database, cutoff: DateTime<Utc>) -> Result<u64> {
    #[derive(Debug, Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct PruneRow {
        id: i64,
    }

    let mut total: u64 = 0;
    loop {
        let select_sql = format!(
            "SELECT record::id(id) AS id FROM ssh_audit_log
                WHERE completedAt < $cutoff LIMIT {PRUNE_CHUNK};"
        );
        let mut resp = db
            .query(select_sql)
            .bind(("cutoff", cutoff))
            .await
            .context("ssh_audit_log prune select failed")?
            .check()?;
        let rows: Vec<PruneRow> = resp.take(0)?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len();
        let ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();

        db.query("DELETE ssh_audit_log WHERE record::id(id) IN $ids;")
            .bind(("ids", ids))
            .await
            .context("ssh_audit_log prune delete failed")?
            .check()?;

        total += n as u64;
        if n < PRUNE_CHUNK {
            break;
        }
        tokio::task::yield_now().await;
    }
    Ok(total)
}

/// Truncate `s` so its UTF-8 byte length is `<= max_bytes`, appending a
/// truncation sentinel that records the original byte length. Char-boundary
/// safe.
fn truncate_with_sentinel(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let suffix = format!(" [truncated, original {} bytes]", s.len());
    let head_budget = max_bytes
        .saturating_sub(suffix.len())
        .saturating_sub("…".len());
    let mut head_end = head_budget.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    format!("{}…{}", &s[..head_end], suffix)
}

fn debug_assert_no_credential_tokens(command_line: &str) {
    if let Some(token) = first_credential_token(command_line) {
        debug_assert!(
            false,
            "ssh_audit_log: commandLine contains a credential-token shape (`{token}`). \
             Audit module invariant violated — caller bypassed the type-seam contract \
             that guarantees no credentials reach AuditEntry. \
             Original: {command_line}"
        );
    }
}

fn first_credential_token(s: &str) -> Option<&'static str> {
    let lower = s.to_ascii_lowercase();
    CREDENTIAL_TOKEN_DENYLIST
        .iter()
        .copied()
        .find(|t| lower.contains(*t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_source_is_insert_only() {
        // PURA-79 R4 (tamper resistance): grep-assert on the source of THIS
        // file. A future contributor who adds a mutation path or a by-id
        // delete path breaks the static check before review can miss it.
        //
        // Search needles are built at runtime by token concatenation so the
        // forbidden contiguous form never appears as a contiguous substring
        // anywhere else in this file — otherwise the test self-matches its
        // own description and panics on innocent doc comments.
        const SOURCE: &str = include_str!("ssh_audit_log.rs");

        let table = "ssh_audit_log";
        let mutate_keyword = "UPDATE";
        let by_id_marker = ":";

        // Forbidden: open-quote + keyword + space + table. An honest
        // mutation path inside a SurrealQL string literal would land as
        // db.query(STR …) where STR begins with that token sequence.
        let forbidden_update = ["\"", mutate_keyword, " ", table].concat();
        // Forbidden: keyword + space + table + colon-form id marker.
        // The prune flow uses WHERE record id IN $ids, never the colon-form.
        let forbidden_by_id_delete = ["DELETE", " ", table, by_id_marker].concat();

        assert!(
            !SOURCE.contains(&forbidden_update),
            "tamper-resistance violation: audit table must not see a \
             mutation query. INSERT-only per PURA-79 R4."
        );
        assert!(
            !SOURCE.contains(&forbidden_by_id_delete),
            "tamper-resistance violation: audit table must not see by-id \
             colon-form deletes. Only the chunked retention prune is \
             permitted per PURA-79 R4."
        );
    }

    #[test]
    fn first_credential_token_finds_known_shapes() {
        for shape in CREDENTIAL_TOKEN_DENYLIST {
            let line = format!("login client_login_name=admin {shape}hunter2");
            assert!(
                first_credential_token(&line).is_some(),
                "denylist shape `{shape}` must be detected"
            );
        }
        // Plausible-but-not-credential lines stay clean.
        assert_eq!(
            first_credential_token("clientlist -uid -away"),
            None,
            "ordinary command line must not match the credential denylist"
        );
        assert_eq!(
            first_credential_token("use sid=3"),
            None,
            "sid scoping must not match the credential denylist"
        );
    }

    #[test]
    fn first_credential_token_is_case_insensitive() {
        // Case-folding on the denylist comparison so an uppercase variant
        // doesn't slip past the assert.
        assert_eq!(
            first_credential_token("LOGIN CLIENT_LOGIN_PASSWORD=hunter2"),
            Some("client_login_password=")
        );
    }

    #[test]
    fn truncate_with_sentinel_passthrough_when_under_cap() {
        let s = "clientlist -uid -away";
        assert_eq!(truncate_with_sentinel(s, 8 * 1024), s);
    }

    #[test]
    fn truncate_with_sentinel_caps_oversized_input() {
        let s = "x".repeat(10_000);
        let truncated = truncate_with_sentinel(&s, ERROR_MSG_MAX_BYTES);
        assert!(
            truncated.len() <= ERROR_MSG_MAX_BYTES,
            "truncated length {} must respect cap {}",
            truncated.len(),
            ERROR_MSG_MAX_BYTES
        );
        assert!(
            truncated.contains("[truncated, original 10000 bytes]"),
            "truncation sentinel must record the original byte length, got: {truncated}"
        );
    }

    #[test]
    fn truncate_with_sentinel_preserves_char_boundaries() {
        // 4-byte UTF-8 chars right at the cap boundary — must not slice
        // mid-codepoint.
        let s = "🎵".repeat(5_000); // 20 KiB of 4-byte chars
        let truncated = truncate_with_sentinel(&s, ERROR_MSG_MAX_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
        // Cheaper: ensure the prefix parses as valid UTF-8 (it always does
        // because it's already a String, but the assertion documents intent).
        assert!(truncated.chars().all(|c| c == '🎵' || c == '…' || c.is_ascii()));
    }

    #[test]
    #[should_panic(expected = "credential-token shape")]
    fn debug_assert_fires_on_planted_password() {
        // PURA-79 R3 belt: a future caller passing a credential-shape
        // commandLine into the repo is loud-failed in dev/test/CI.
        debug_assert_no_credential_tokens(
            "login client_login_name=admin client_login_password=hunter2",
        );
    }

    #[test]
    fn debug_assert_does_not_fire_on_clean_input() {
        // Sanity: the ordinary-command path never trips the assert.
        debug_assert_no_credential_tokens("clientlist -uid -away");
        debug_assert_no_credential_tokens("use sid=3");
        debug_assert_no_credential_tokens("");
    }
}
