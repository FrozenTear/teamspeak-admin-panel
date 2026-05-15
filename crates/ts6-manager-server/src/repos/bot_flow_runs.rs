//! `BotFlowRun` repo — v1.1 flow engine run history (PURA-241).
//!
//! Mirrors the `bot_flows` style: a sequence-driven int PK, camelCase
//! field names matching the spec / wire shape verbatim, all serdes via
//! `SurrealValue`. `trigger` and `actionResults` carry JSON documents
//! whose shape depends on the discriminant — we store them as JSON
//! strings to match the convention `bot_flow.flowData` set, and
//! decode/encode through `serde_json` at the repo boundary.
//!
//! Bounded-storage policy (brief §5.3):
//!   - [`enforce_per_flow_cap`] keeps at most [`PER_FLOW_RUN_CAP`] rows
//!     for a given `flowId`, deleting oldest by `startedAt` first. Called
//!     by the engine after every `insert`.
//!   - [`prune_older_than`] is the global TTL janitor; the engine's
//!     hourly tokio task calls it with `now - 30 days`.
//!
//! Failure model (brief §6.4): [`mark_in_flight_as_interrupted`] is the
//! boot-time sweep — it rewrites every still-`in_flight` row to
//! `interrupted` so the persistence layer never lies about what is
//! actually running.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;
use ts6_manager_shared::flows::{ActionResult, FlowRunStatus};

use crate::db::Database;

/// Brief §5.3 — per-flow row cap. Conservative for v1.1; a future
/// operator-tunable knob can land behind a `bot_flow.runRetentionRows`
/// column without breaking this repo's surface.
pub const PER_FLOW_RUN_CAP: usize = 200;

/// Truncation cap on the persisted `error` field. Brief §5.2.
pub const ERROR_MAX_BYTES: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
struct BotFlowRunRow {
    id: i64,
    flowId: i64,
    trigger: String,
    status: String,
    startedAt: DateTime<Utc>,
    finishedAt: Option<DateTime<Utc>>,
    error: Option<String>,
    actionResults: String,
}

/// Decoded `bot_flow_run` row — the repo-level projection. `trigger` and
/// `actionResults` are decoded from their JSON-string representations so
/// callers work in typed shapes.
#[derive(Debug, Clone)]
pub struct BotFlowRun {
    pub id: i64,
    pub flowId: i64,
    pub trigger: serde_json::Value,
    pub status: FlowRunStatus,
    pub startedAt: DateTime<Utc>,
    pub finishedAt: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub actionResults: Vec<ActionResult>,
}

#[derive(Debug, Clone)]
pub struct NewBotFlowRun {
    pub flowId: i64,
    pub trigger: serde_json::Value,
    /// Initial status. The engine inserts most rows as `InFlight`; the
    /// `skipped_disabled` path inserts the row already in its terminal
    /// state with `finishedAt = Some(now)` (handled by [`insert`] when
    /// `status != InFlight`).
    pub status: FlowRunStatus,
    /// Planned action results, in order. When the engine inserts the row
    /// at run-start every entry is `Skipped` with `duration_ms = 0`; the
    /// engine updates them as actions finish via [`update_action_result`].
    pub actionResults: Vec<ActionResult>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    flowId,
    trigger,
    status,
    startedAt,
    finishedAt,
    error,
    actionResults
";

fn truncate_error(err: &str) -> String {
    if err.len() <= ERROR_MAX_BYTES {
        return err.to_string();
    }
    // Char-boundary-safe truncation; the architecture brief caps at 2 KiB.
    let mut cut = ERROR_MAX_BYTES;
    while cut > 0 && !err.is_char_boundary(cut) {
        cut -= 1;
    }
    let original = err.len();
    format!("{}\n[truncated, original {original} bytes]", &err[..cut])
}

fn status_to_str(status: FlowRunStatus) -> &'static str {
    match status {
        FlowRunStatus::InFlight => "in_flight",
        FlowRunStatus::Ok => "ok",
        FlowRunStatus::Errored => "errored",
        FlowRunStatus::Interrupted => "interrupted",
        FlowRunStatus::SkippedDisabled => "skipped_disabled",
    }
}

fn status_from_str(s: &str) -> Result<FlowRunStatus> {
    Ok(match s {
        "in_flight" => FlowRunStatus::InFlight,
        "ok" => FlowRunStatus::Ok,
        "errored" => FlowRunStatus::Errored,
        "interrupted" => FlowRunStatus::Interrupted,
        "skipped_disabled" => FlowRunStatus::SkippedDisabled,
        other => anyhow::bail!("unknown bot_flow_run status `{other}`"),
    })
}

fn decode(row: BotFlowRunRow) -> Result<BotFlowRun> {
    let trigger: serde_json::Value =
        serde_json::from_str(&row.trigger).context("decode bot_flow_run.trigger")?;
    let actionResults: Vec<ActionResult> =
        serde_json::from_str(&row.actionResults).context("decode bot_flow_run.actionResults")?;
    Ok(BotFlowRun {
        id: row.id,
        flowId: row.flowId,
        trigger,
        status: status_from_str(&row.status)?,
        startedAt: row.startedAt,
        finishedAt: row.finishedAt,
        error: row.error,
        actionResults,
    })
}

pub async fn insert(db: &Database, new: NewBotFlowRun) -> Result<BotFlowRun> {
    let trigger_json = serde_json::to_string(&new.trigger).context("encode trigger")?;
    let action_results_json =
        serde_json::to_string(&new.actionResults).context("encode actionResults")?;
    let status_str = status_to_str(new.status);
    let terminal = !matches!(new.status, FlowRunStatus::InFlight);
    let sql = format!(
        "CREATE type::record('bot_flow_run', sequence::nextval('bot_flow_run_id'))
            CONTENT {{
                flowId: $flowId,
                trigger: $trigger,
                status: $status,
                actionResults: $actionResults,
                finishedAt: $finishedAt
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("flowId", new.flowId))
        .bind(("trigger", trigger_json))
        .bind(("status", status_str.to_string()))
        .bind(("actionResults", action_results_json))
        .bind(("finishedAt", if terminal { Some(Utc::now()) } else { None }))
        .await
        .context("bot_flow_run insert query failed")?
        .check()?;
    let row: Option<BotFlowRunRow> = resp.take(0)?;
    let row = row.context("bot_flow_run insert returned no row")?;
    decode(row)
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<BotFlowRun>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('bot_flow_run', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    let row: Option<BotFlowRunRow> = resp.take(0)?;
    row.map(decode).transpose()
}

/// Run-history list for the FE. Brief §5.2 — ordering is
/// `startedAt DESC, id DESC` with a keyset cursor on `id`.
pub async fn list_for_flow(
    db: &Database,
    flow_id: i64,
    limit: usize,
    cursor: Option<i64>,
) -> Result<Vec<BotFlowRun>> {
    let limit = limit.clamp(1, 200);
    let sql = if cursor.is_some() {
        format!(
            "SELECT {PROJECTION} FROM bot_flow_run
                WHERE flowId = $fid AND record::id(id) < $cursor
                ORDER BY startedAt DESC, id DESC
                LIMIT $limit;"
        )
    } else {
        format!(
            "SELECT {PROJECTION} FROM bot_flow_run
                WHERE flowId = $fid
                ORDER BY startedAt DESC, id DESC
                LIMIT $limit;"
        )
    };
    let mut q = db.query(sql).bind(("fid", flow_id));
    if let Some(c) = cursor {
        q = q.bind(("cursor", c));
    }
    let mut resp = q.bind(("limit", limit as i64)).await?.check()?;
    let rows: Vec<BotFlowRunRow> = resp.take(0)?;
    rows.into_iter().map(decode).collect()
}

/// Latest run for the given flow — `Flow.lastRun` in the list response.
pub async fn latest_for_flow(db: &Database, flow_id: i64) -> Result<Option<BotFlowRun>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_flow_run
            WHERE flowId = $fid
            ORDER BY startedAt DESC, id DESC
            LIMIT 1;"
    );
    let mut resp = db.query(sql).bind(("fid", flow_id)).await?.check()?;
    let rows: Vec<BotFlowRunRow> = resp.take(0)?;
    rows.into_iter().next().map(decode).transpose()
}

#[derive(Debug, Clone)]
pub struct FinishRun {
    pub status: FlowRunStatus,
    /// Truncated to [`ERROR_MAX_BYTES`] at the repo boundary.
    pub error: Option<String>,
    pub actionResults: Vec<ActionResult>,
}

/// Stamp the terminal state onto a previously-in-flight row. Sets
/// `finishedAt = now`.
pub async fn finish(db: &Database, id: i64, finish: FinishRun) -> Result<Option<BotFlowRun>> {
    let error = finish.error.as_deref().map(truncate_error);
    let action_results_json =
        serde_json::to_string(&finish.actionResults).context("encode actionResults")?;
    let sql = format!(
        "UPDATE type::record('bot_flow_run', $id) MERGE {{
            status: $status,
            error: $error,
            actionResults: $actionResults,
            finishedAt: time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("status", status_to_str(finish.status).to_string()))
        .bind(("error", error))
        .bind(("actionResults", action_results_json))
        .await?
        .check()?;
    let row: Option<BotFlowRunRow> = resp.take(0)?;
    row.map(decode).transpose()
}

/// Boot-time sweep — every row still in `in_flight` is rewritten to
/// `interrupted` before the engine starts accepting new triggers. Brief
/// §6.4. Returns the number of rows rewritten.
pub async fn mark_in_flight_as_interrupted(db: &Database) -> Result<u64> {
    let sql = "
        SELECT count() FROM bot_flow_run WHERE status = 'in_flight' GROUP ALL;
        UPDATE bot_flow_run SET
            status = 'interrupted',
            error = 'manager restart',
            finishedAt = time::now()
        WHERE status = 'in_flight';
    ";
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let mut resp = db.query(sql).await?.check()?;
    let counted: Option<CountRow> = resp.take(0)?;
    Ok(counted.map(|c| c.count.max(0) as u64).unwrap_or(0))
}

/// After every successful [`insert`], the engine calls this to enforce
/// the per-flow cap of [`PER_FLOW_RUN_CAP`] rows. Deletes the oldest
/// rows by `startedAt` first; returns the count deleted.
pub async fn enforce_per_flow_cap(db: &Database, flow_id: i64) -> Result<u64> {
    let cap = PER_FLOW_RUN_CAP as i64;
    let sql = "
        SELECT count() FROM bot_flow_run WHERE flowId = $fid GROUP ALL;
    ";
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let mut resp = db.query(sql).bind(("fid", flow_id)).await?.check()?;
    let counted: Option<CountRow> = resp.take(0)?;
    let total = counted.map(|c| c.count).unwrap_or(0);
    if total <= cap {
        return Ok(0);
    }
    let overflow = (total - cap) as usize;

    // Identify the oldest `overflow` ids for this flow and delete them.
    // Two-step (select-then-delete) keeps the DELETE statement portable;
    // SurrealDB does not yet support `DELETE … LIMIT … ORDER BY` in
    // every version we target.
    // SurrealDB v3 requires every `ORDER BY` idiom to appear in the
    // projection — `startedAt` is selected here even though the caller
    // only consumes `id`.
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct IdRow {
        id: i64,
        startedAt: DateTime<Utc>,
    }
    let pick_sql = "
        SELECT record::id(id) AS id, startedAt FROM bot_flow_run
            WHERE flowId = $fid
            ORDER BY startedAt ASC, id ASC
            LIMIT $limit;
    ";
    let mut resp = db
        .query(pick_sql)
        .bind(("fid", flow_id))
        .bind(("limit", overflow as i64))
        .await?
        .check()?;
    let victims: Vec<IdRow> = resp.take(0)?;
    let mut removed = 0u64;
    for v in victims {
        let del_sql = "DELETE type::record('bot_flow_run', $id);";
        db.query(del_sql).bind(("id", v.id)).await?.check()?;
        removed += 1;
    }
    Ok(removed)
}

/// Global TTL janitor. Brief §5.3 — every hour the engine task calls
/// this with `now - 30 days`. Only terminal rows are pruned; an
/// in-flight row whose `finishedAt` is still NULL is never targeted by
/// the TTL pass.
pub async fn prune_older_than(db: &Database, cutoff: DateTime<Utc>) -> Result<u64> {
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let count_sql = "
        SELECT count() FROM bot_flow_run
            WHERE finishedAt != NONE AND finishedAt < $cutoff
            GROUP ALL;
    ";
    let mut resp = db
        .query(count_sql)
        .bind(("cutoff", cutoff))
        .await?
        .check()?;
    let counted: Option<CountRow> = resp.take(0)?;
    let total = counted.map(|c| c.count.max(0) as u64).unwrap_or(0);
    let del_sql = "
        DELETE bot_flow_run
            WHERE finishedAt != NONE AND finishedAt < $cutoff;
    ";
    db.query(del_sql).bind(("cutoff", cutoff)).await?.check()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_error_keeps_short_messages_intact() {
        assert_eq!(truncate_error("hello"), "hello");
    }

    #[test]
    fn truncate_error_appends_sentinel_when_oversize() {
        let big = "x".repeat(ERROR_MAX_BYTES + 100);
        let out = truncate_error(&big);
        assert!(
            out.contains("[truncated, original"),
            "expected sentinel, got {} bytes",
            out.len()
        );
        assert!(
            out.len() <= ERROR_MAX_BYTES + 64,
            "truncated payload should stay near the cap, got {} bytes",
            out.len()
        );
    }
}
