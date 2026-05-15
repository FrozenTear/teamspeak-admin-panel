//! `admin_audit_log` repo (PURA-228 / PURA-234 — migration 0009).
//!
//! Append-only persistence for the v1.1 admin-mutation audit trail per
//! `docs/admin/audit-shape.md`. The shape mirrors `ssh_audit_log`
//! (`repos::ssh_audit_log`) deliberately: snapshot fields, set-null on
//! user-delete, READONLY timestamps. Tamper-resistance posture (PURA-79
//! R4) carries over — this repo is **INSERT-only by contract** with the
//! exception of [`prune_older_than`] which the retention janitor calls.
//!
//! PURA-235 lands the inserts the eight new admin routes need plus the
//! filter-and-page read used by `GET /api/audit`. The hard-blocklist
//! redaction + retention janitor + write hooks on the **other** mutating
//! routes (auth/password, setup/init, …) belong to PURA-236.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// Soft cap for the redacted JSON `payload` per audit-shape.md §2.3.
pub const PAYLOAD_MAX_BYTES: usize = 2 * 1024;
pub const ERROR_MSG_MAX_BYTES: usize = 4 * 1024;
pub const USER_AGENT_MAX_BYTES: usize = 1024;

/// `GET /api/audit` clamps the operator-supplied limit at this value
/// (`docs/admin/http-api.md` §3.4). Default lives in the route module.
pub const MAX_LIMIT: i64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct AdminAuditLogRow {
    pub id: i64,
    pub actorUserId: Option<i64>,
    pub actorUsername: String,
    pub kind: String,
    pub targetKind: Option<String>,
    pub targetId: Option<i64>,
    pub targetLabel: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub outcome: String,
    pub errorMsg: Option<String>,
    pub requestIp: Option<String>,
    pub requestUserAgent: Option<String>,
    pub occurredAt: DateTime<Utc>,
    pub insertedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewAdminAuditLog {
    pub actorUserId: Option<i64>,
    pub actorUsername: String,
    pub kind: String,
    pub targetKind: Option<String>,
    pub targetId: Option<i64>,
    pub targetLabel: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub outcome: String,
    pub errorMsg: Option<String>,
    pub requestIp: Option<String>,
    pub requestUserAgent: Option<String>,
}

/// Filters accepted by `GET /api/audit`. Each `Option` represents an
/// absent query-string parameter. `from` / `to` bound `occurredAt`.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub actorUserId: Option<i64>,
    pub kind: Option<String>,
    pub targetKind: Option<String>,
    pub targetId: Option<i64>,
    pub outcome: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    actorUserId,
    actorUsername,
    kind,
    targetKind,
    targetId,
    targetLabel,
    payload,
    outcome,
    errorMsg,
    requestIp,
    requestUserAgent,
    occurredAt,
    insertedAt
";

/// INSERT one row. Applies the §2.3 truncation caps for `payload`,
/// `errorMsg`, and `requestUserAgent` before handing the values to
/// SurrealDB so a pathological upstream can't bloat an audit row.
///
/// PURA-235 inlines this from the route layer; PURA-236 will add the
/// hard-blocklist `debug_assert!` (mirroring `ssh_audit_log`'s
/// credential-token denylist) and the row-cap cull on top.
pub async fn insert(db: &Database, new: NewAdminAuditLog) -> Result<AdminAuditLogRow> {
    let payload = new.payload.map(soft_redact_payload);
    let error_msg = new
        .errorMsg
        .as_deref()
        .map(|s| truncate_with_sentinel(s, ERROR_MSG_MAX_BYTES));
    let user_agent = new
        .requestUserAgent
        .as_deref()
        .map(|s| truncate_with_sentinel(s, USER_AGENT_MAX_BYTES));

    let sql = format!(
        "CREATE type::record('admin_audit_log', sequence::nextval('admin_audit_log_id'))
            CONTENT {{
                actorUserId:      $actorUserId,
                actorUsername:    $actorUsername,
                kind:             $kind,
                targetKind:       $targetKind,
                targetId:         $targetId,
                targetLabel:      $targetLabel,
                payload:          $payload,
                outcome:          $outcome,
                errorMsg:         $errorMsg,
                requestIp:        $requestIp,
                requestUserAgent: $requestUserAgent
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("actorUserId", new.actorUserId))
        .bind(("actorUsername", new.actorUsername))
        .bind(("kind", new.kind))
        .bind(("targetKind", new.targetKind))
        .bind(("targetId", new.targetId))
        .bind(("targetLabel", new.targetLabel))
        .bind(("payload", payload))
        .bind(("outcome", new.outcome))
        .bind(("errorMsg", error_msg))
        .bind(("requestIp", new.requestIp))
        .bind(("requestUserAgent", user_agent))
        .await
        .context("admin_audit_log insert query failed")?
        .check()?;
    let row: Option<AdminAuditLogRow> = resp.take(0)?;
    row.context("admin_audit_log insert returned no row")
}

/// Paginated read backing `GET /api/audit`. Returns the page of matching
/// rows plus the total count for the same filter set so the operator UI
/// can render the pagination control.
///
/// Ordering: `occurredAt DESC, id DESC` for stable deep pagination
/// (`docs/admin/http-api.md` §3.4).
pub async fn list(
    db: &Database,
    filter: &ListFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<AdminAuditLogRow>, i64)> {
    let (where_clause, bindings) = build_where(filter);

    let list_sql = format!(
        "SELECT {PROJECTION} FROM admin_audit_log
            {where_clause}
            ORDER BY occurredAt DESC, id DESC
            LIMIT $limit START $offset;"
    );
    let count_sql = format!("RETURN array::len(SELECT id FROM admin_audit_log {where_clause});");

    let mut list_q = db.query(list_sql);
    for (k, v) in &bindings {
        list_q = bind_filter_value(list_q, k, v);
    }
    let mut list_resp = list_q
        .bind(("limit", limit))
        .bind(("offset", offset))
        .await
        .context("admin_audit_log list query failed")?
        .check()?;
    let rows: Vec<AdminAuditLogRow> = list_resp.take(0)?;

    let mut count_q = db.query(count_sql);
    for (k, v) in &bindings {
        count_q = bind_filter_value(count_q, k, v);
    }
    let mut count_resp = count_q
        .await
        .context("admin_audit_log count query failed")?
        .check()?;
    let n: Option<i64> = count_resp.take(0)?;
    Ok((rows, n.unwrap_or(0)))
}

#[derive(Debug, Clone)]
enum FilterValue {
    Int(i64),
    Str(String),
    Time(DateTime<Utc>),
}

fn bind_filter_value<'a>(
    q: surrealdb::method::Query<'a, surrealdb::engine::any::Any>,
    key: &'static str,
    v: &FilterValue,
) -> surrealdb::method::Query<'a, surrealdb::engine::any::Any> {
    match v {
        FilterValue::Int(n) => q.bind((key, *n)),
        FilterValue::Str(s) => q.bind((key, s.clone())),
        FilterValue::Time(t) => q.bind((key, *t)),
    }
}

fn build_where(filter: &ListFilter) -> (String, Vec<(&'static str, FilterValue)>) {
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut bindings: Vec<(&'static str, FilterValue)> = Vec::new();
    if let Some(v) = filter.actorUserId {
        clauses.push("actorUserId = $actorUserId");
        bindings.push(("actorUserId", FilterValue::Int(v)));
    }
    if let Some(ref v) = filter.kind {
        clauses.push("kind = $kind");
        bindings.push(("kind", FilterValue::Str(v.clone())));
    }
    if let Some(ref v) = filter.targetKind {
        clauses.push("targetKind = $targetKind");
        bindings.push(("targetKind", FilterValue::Str(v.clone())));
    }
    if let Some(v) = filter.targetId {
        clauses.push("targetId = $targetId");
        bindings.push(("targetId", FilterValue::Int(v)));
    }
    if let Some(ref v) = filter.outcome {
        clauses.push("outcome = $outcome");
        bindings.push(("outcome", FilterValue::Str(v.clone())));
    }
    if let Some(v) = filter.from {
        clauses.push("occurredAt >= $from");
        bindings.push(("from", FilterValue::Time(v)));
    }
    if let Some(v) = filter.to {
        clauses.push("occurredAt <= $to");
        bindings.push(("to", FilterValue::Time(v)));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_clause, bindings)
}

/// Soft-redact a payload to the §2.3 cap. Oversized payloads degrade to
/// `{"_truncated": true, "_byteCount": <n>}` so the row still indexes.
fn soft_redact_payload(value: serde_json::Value) -> serde_json::Value {
    let raw = match serde_json::to_string(&value) {
        Ok(s) => s,
        Err(_) => return value,
    };
    if raw.len() <= PAYLOAD_MAX_BYTES {
        return value;
    }
    serde_json::json!({
        "_truncated": true,
        "_byteCount": raw.len(),
    })
}

/// Char-boundary-safe truncation with a sentinel that records the original
/// byte length. Mirrors `ssh_audit_log::truncate_with_sentinel` so the two
/// audit surfaces present truncated rows in the same shape.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};

    async fn fresh_db() -> std::sync::Arc<Database> {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        db
    }

    fn evt(kind: &str, actor: &str) -> NewAdminAuditLog {
        NewAdminAuditLog {
            actorUserId: Some(1),
            actorUsername: actor.into(),
            kind: kind.into(),
            targetKind: Some("user".into()),
            targetId: Some(2),
            targetLabel: Some("mod1".into()),
            payload: None,
            outcome: "success".into(),
            errorMsg: None,
            requestIp: None,
            requestUserAgent: None,
        }
    }

    #[tokio::test]
    async fn insert_round_trips_a_row() {
        let db = fresh_db().await;
        let row = insert(&db, evt("userCreated", "admin")).await.unwrap();
        assert_eq!(row.kind, "userCreated");
        assert_eq!(row.actorUsername, "admin");
        assert_eq!(row.targetKind.as_deref(), Some("user"));
        assert!(row.occurredAt <= chrono::Utc::now());
    }

    #[tokio::test]
    async fn insert_round_trips_a_json_payload() {
        let db = fresh_db().await;
        let row = insert(
            &db,
            NewAdminAuditLog {
                payload: Some(serde_json::json!({ "fields": ["role", "enabled"] })),
                ..evt("userPatched", "admin")
            },
        )
        .await
        .unwrap();
        let payload = row.payload.expect("payload round-trips");
        assert_eq!(
            payload
                .get("fields")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn list_returns_newest_first_and_paginates() {
        let db = fresh_db().await;
        for n in 0..5 {
            insert(&db, evt(&format!("kind{n}"), "admin"))
                .await
                .unwrap();
        }
        let (rows, total) = list(&db, &ListFilter::default(), 2, 0).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "kind4");
        assert_eq!(rows[1].kind, "kind3");

        let (page2, _) = list(&db, &ListFilter::default(), 2, 2).await.unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].kind, "kind2");
    }

    #[tokio::test]
    async fn list_filters_by_kind_and_target() {
        let db = fresh_db().await;
        insert(&db, evt("userCreated", "admin")).await.unwrap();
        insert(&db, evt("userDisabled", "admin")).await.unwrap();
        insert(
            &db,
            NewAdminAuditLog {
                targetId: Some(99),
                ..evt("userDisabled", "admin")
            },
        )
        .await
        .unwrap();

        let f = ListFilter {
            kind: Some("userDisabled".into()),
            ..Default::default()
        };
        let (rows, total) = list(&db, &f, 50, 0).await.unwrap();
        assert_eq!(total, 2);
        for row in &rows {
            assert_eq!(row.kind, "userDisabled");
        }

        let f = ListFilter {
            kind: Some("userDisabled".into()),
            targetKind: Some("user".into()),
            targetId: Some(2),
            ..Default::default()
        };
        let (rows, total) = list(&db, &f, 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].targetId, Some(2));
    }

    #[tokio::test]
    async fn user_delete_nulls_actor_and_target_on_historic_rows() {
        // Migration 0009 declares the `user_set_null_admin_audit` event;
        // pin it here so a future schema refactor that drops the event
        // can't silently break forensic durability.
        let db = fresh_db().await;
        let uid = crate::repos::users::insert(
            &db,
            crate::repos::users::NewUser {
                username: "actor".into(),
                passwordHash: "$argon2id$v=19$test".into(),
                displayName: "Actor".into(),
                role: "admin".into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id;
        insert(
            &db,
            NewAdminAuditLog {
                actorUserId: Some(uid),
                targetId: Some(uid),
                ..evt("userDisabled", "actor")
            },
        )
        .await
        .unwrap();

        crate::repos::users::delete(&db, uid).await.unwrap();

        let (rows, _) = list(&db, &ListFilter::default(), 50, 0).await.unwrap();
        assert_eq!(rows.len(), 1, "audit row survives user delete");
        assert!(
            rows[0].actorUserId.is_none(),
            "actor set-null on user delete"
        );
        assert!(rows[0].targetId.is_none(), "target set-null on user delete");
    }

    #[test]
    fn soft_redact_passes_small_payloads_through() {
        let v = serde_json::json!({ "fields": ["enabled"] });
        let red = soft_redact_payload(v.clone());
        assert_eq!(red, v);
    }

    #[test]
    fn soft_redact_substitutes_oversized_payloads() {
        let big = "x".repeat(PAYLOAD_MAX_BYTES + 1);
        let v = serde_json::json!({ "blob": big });
        let red = soft_redact_payload(v);
        assert_eq!(red.get("_truncated").and_then(|v| v.as_bool()), Some(true));
        assert!(red.get("_byteCount").and_then(|v| v.as_i64()).unwrap() > PAYLOAD_MAX_BYTES as i64);
    }

    #[test]
    fn truncate_with_sentinel_passthrough_under_cap() {
        let s = "small";
        assert_eq!(truncate_with_sentinel(s, 100), s);
    }

    #[test]
    fn truncate_with_sentinel_caps_oversized_input() {
        let s = "x".repeat(10_000);
        let t = truncate_with_sentinel(&s, ERROR_MSG_MAX_BYTES);
        assert!(t.len() <= ERROR_MSG_MAX_BYTES);
        assert!(t.contains("[truncated, original 10000 bytes]"));
    }
}
