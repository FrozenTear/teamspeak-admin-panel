//! `moderation_case` repo — Phase 9.0 moderation (PURA-285, migration 0011).
//!
//! One row per actioned subject on a virtual server (design brief §5).
//! Field names mirror the brief verbatim so `/api/moderation/cases`
//! handlers serialise rows through `serde_json` without renames.
//!
//! State machine (brief §7): `open → actioned → resolved`, plus
//! `resolved → open` (reopen). [`set_status`] is the single transition
//! primitive; the route layer is responsible for writing the paired
//! `moderation_case_action` + `admin_audit_log` rows around it.

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the 9.0-routes workstream (PURA-286)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// Valid `origin` values. `automod` is the Phase 9.1 automod origin;
/// `report` is the Phase 9.2 report-intake origin (PURA-308) — a case
/// promoted from a `moderation_report`.
pub const ORIGINS: &[&str] = &["operator", "complaint", "automod", "report"];

/// Valid `status` values. `appealed` is the Phase 9.2 state a case enters
/// when the subject files a public appeal (`actioned → appealed`,
/// PURA-307); an operator uphold/overturn decision moves it on to
/// `resolved` (PURA-308).
pub const STATUSES: &[&str] = &["open", "actioned", "resolved", "appealed"];

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationCase {
    pub id: i64,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub subjectUid: String,
    pub subjectNicknameSnapshot: String,
    pub origin: String,
    pub originRef: Option<String>,
    pub status: String,
    pub reason: String,
    pub resolutionNote: Option<String>,
    pub openedByUserId: Option<i64>,
    pub openedAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
    pub resolvedAt: Option<DateTime<Utc>>,
}

/// Caller-supplied fields on case open. `status` starts `open`; `id` and
/// the timestamps are server-managed.
#[derive(Debug, Clone)]
pub struct NewModerationCase {
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub subjectUid: String,
    pub subjectNicknameSnapshot: String,
    pub origin: String,
    pub originRef: Option<String>,
    pub reason: String,
    pub openedByUserId: Option<i64>,
}

/// Filters accepted by `GET /api/moderation/cases`. Each absent `Option`
/// is a missing query-string parameter.
#[derive(Debug, Clone, Default)]
pub struct CaseFilter {
    pub subjectUid: Option<String>,
    pub status: Option<String>,
    /// One of [`ORIGINS`] — `operator` / `complaint` / `automod`. Backs
    /// the Phase 9.1.4 automod-queue preset (`origin = automod`).
    pub origin: Option<String>,
    pub serverConfigId: Option<i64>,
    pub virtualServerId: Option<i64>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    serverConfigId,
    virtualServerId,
    subjectUid,
    subjectNicknameSnapshot,
    origin,
    originRef,
    status,
    reason,
    resolutionNote,
    openedByUserId,
    openedAt,
    updatedAt,
    resolvedAt
";

/// Open a new case. The row lands in `status = 'open'`.
pub async fn insert(db: &Database, new: NewModerationCase) -> Result<ModerationCase> {
    let sql = format!(
        "CREATE type::record('moderation_case', sequence::nextval('moderation_case_id'))
            CONTENT {{
                serverConfigId:          $serverConfigId,
                virtualServerId:         $virtualServerId,
                subjectUid:              $subjectUid,
                subjectNicknameSnapshot: $subjectNicknameSnapshot,
                origin:                  $origin,
                originRef:               $originRef,
                status:                  'open',
                reason:                  $reason,
                openedByUserId:          $openedByUserId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("virtualServerId", new.virtualServerId))
        .bind(("subjectUid", new.subjectUid))
        .bind(("subjectNicknameSnapshot", new.subjectNicknameSnapshot))
        .bind(("origin", new.origin))
        .bind(("originRef", new.originRef))
        .bind(("reason", new.reason))
        .bind(("openedByUserId", new.openedByUserId))
        .await
        .context("moderation_case insert query failed")?
        .check()?;
    let row: Option<ModerationCase> = resp.take(0)?;
    row.context("moderation_case insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ModerationCase>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('moderation_case', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("moderation_case find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Apply the [`CaseFilter`] bindings shared by the list + count queries.
/// SurrealDB ignores binds a statement does not reference, so the same
/// set is safe on both.
fn apply_filter_binds<'a>(
    mut q: surrealdb::method::Query<'a, surrealdb::engine::any::Any>,
    filter: &CaseFilter,
) -> surrealdb::method::Query<'a, surrealdb::engine::any::Any> {
    if let Some(ref v) = filter.subjectUid {
        q = q.bind(("subjectUid", v.clone()));
    }
    if let Some(ref v) = filter.status {
        q = q.bind(("status", v.clone()));
    }
    if let Some(ref v) = filter.origin {
        q = q.bind(("origin", v.clone()));
    }
    if let Some(v) = filter.serverConfigId {
        q = q.bind(("serverConfigId", v));
    }
    if let Some(v) = filter.virtualServerId {
        q = q.bind(("virtualServerId", v));
    }
    q
}

/// Paginated case list. Ordering is `openedAt DESC, id DESC` for stable
/// deep pagination. Returns the page plus the total count for the filter.
pub async fn list(
    db: &Database,
    filter: &CaseFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ModerationCase>, i64)> {
    let mut clauses: Vec<&str> = Vec::new();
    if filter.subjectUid.is_some() {
        clauses.push("subjectUid = $subjectUid");
    }
    if filter.status.is_some() {
        clauses.push("status = $status");
    }
    if filter.origin.is_some() {
        clauses.push("origin = $origin");
    }
    if filter.serverConfigId.is_some() {
        clauses.push("serverConfigId = $serverConfigId");
    }
    if filter.virtualServerId.is_some() {
        clauses.push("virtualServerId = $virtualServerId");
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let list_sql = format!(
        "SELECT {PROJECTION} FROM moderation_case
            {where_clause}
            ORDER BY openedAt DESC, id DESC
            LIMIT $limit START $offset;"
    );
    let count_sql = format!("RETURN array::len(SELECT id FROM moderation_case {where_clause});");

    let mut list_resp = apply_filter_binds(db.query(list_sql), filter)
        .bind(("limit", limit))
        .bind(("offset", offset))
        .await
        .context("moderation_case list query failed")?
        .check()?;
    let rows: Vec<ModerationCase> = list_resp.take(0)?;

    let mut count_resp = apply_filter_binds(db.query(count_sql), filter)
        .await
        .context("moderation_case count query failed")?
        .check()?;
    let n: Option<i64> = count_resp.take(0)?;
    Ok((rows, n.unwrap_or(0)))
}

/// Every case for a subject UID, newest first — backs the per-user
/// history pane (`GET /api/moderation/subjects/{uid}/history`).
pub async fn list_for_subject(db: &Database, subject_uid: &str) -> Result<Vec<ModerationCase>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_case
            WHERE subjectUid = $uid
            ORDER BY openedAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("uid", subject_uid.to_string()))
        .await
        .context("moderation_case list_for_subject query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// The single state-transition primitive. Sets `status`, refreshes
/// `updatedAt`, and manages `resolvedAt` / `resolutionNote`:
///
/// - moving **to** `resolved` stamps `resolvedAt = now`.
/// - moving **off** `resolved` (reopen) clears `resolvedAt`.
///
/// Returns the updated row, or `None` if no case has that id.
pub async fn set_status(
    db: &Database,
    id: i64,
    status: &str,
    resolution_note: Option<String>,
) -> Result<Option<ModerationCase>> {
    // `resolvedAt` is computed here rather than in SurrealQL so the
    // reopen path (`resolved → open`) reliably clears it.
    let resolved_at = if status == "resolved" {
        Some(Utc::now())
    } else {
        None
    };
    let sql = format!(
        "UPDATE type::record('moderation_case', $id) MERGE {{
            status:         $status,
            resolutionNote: $resolutionNote,
            resolvedAt:     $resolvedAt,
            updatedAt:      time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("status", status.to_string()))
        .bind(("resolutionNote", resolution_note))
        .bind(("resolvedAt", resolved_at))
        .await
        .context("moderation_case set_status query failed")?
        .check()?;
    Ok(resp.take(0)?)
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

    fn new_case(uid: &str) -> NewModerationCase {
        NewModerationCase {
            serverConfigId: 1,
            virtualServerId: 1,
            subjectUid: uid.into(),
            subjectNicknameSnapshot: "Troublemaker".into(),
            origin: "operator".into(),
            originRef: None,
            reason: "spam".into(),
            openedByUserId: Some(1),
        }
    }

    #[tokio::test]
    async fn insert_opens_case_in_open_status() {
        let db = fresh_db().await;
        let c = insert(&db, new_case("uid-a")).await.unwrap();
        assert_eq!(c.status, "open");
        assert_eq!(c.subjectUid, "uid-a");
        assert!(c.resolvedAt.is_none());
        assert!(c.openedAt <= Utc::now());
    }

    #[tokio::test]
    async fn set_status_resolve_then_reopen_manages_resolved_at() {
        let db = fresh_db().await;
        let c = insert(&db, new_case("uid-b")).await.unwrap();

        let resolved = set_status(&db, c.id, "resolved", Some("warned".into()))
            .await
            .unwrap()
            .expect("case exists");
        assert_eq!(resolved.status, "resolved");
        assert!(resolved.resolvedAt.is_some());
        assert_eq!(resolved.resolutionNote.as_deref(), Some("warned"));

        let reopened = set_status(&db, c.id, "open", None)
            .await
            .unwrap()
            .expect("case exists");
        assert_eq!(reopened.status, "open");
        assert!(reopened.resolvedAt.is_none(), "reopen clears resolvedAt");
    }

    #[tokio::test]
    async fn set_status_on_missing_case_returns_none() {
        let db = fresh_db().await;
        let got = set_status(&db, 9999, "resolved", None).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn list_filters_and_paginates_newest_first() {
        let db = fresh_db().await;
        for n in 0..4 {
            insert(&db, new_case(&format!("uid-{n}"))).await.unwrap();
        }

        let (rows, total) = list(&db, &CaseFilter::default(), 2, 0).await.unwrap();
        assert_eq!(total, 4);
        assert_eq!(rows.len(), 2);
        // Newest-first: uid-3 opened last.
        assert_eq!(rows[0].subjectUid, "uid-3");

        let f = CaseFilter {
            subjectUid: Some("uid-1".into()),
            ..Default::default()
        };
        let (rows, total) = list(&db, &f, 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].subjectUid, "uid-1");
    }

    #[tokio::test]
    async fn list_filters_by_origin() {
        let db = fresh_db().await;
        insert(&db, new_case("uid-op")).await.unwrap();
        insert(
            &db,
            NewModerationCase {
                origin: "automod".into(),
                originRef: Some("bad-name:7".into()),
                ..new_case("uid-auto")
            },
        )
        .await
        .unwrap();

        let f = CaseFilter {
            origin: Some("automod".into()),
            ..Default::default()
        };
        let (rows, total) = list(&db, &f, 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].subjectUid, "uid-auto");
        assert_eq!(rows[0].origin, "automod");
    }

    #[tokio::test]
    async fn list_for_subject_returns_only_that_subject() {
        let db = fresh_db().await;
        insert(&db, new_case("uid-keep")).await.unwrap();
        insert(&db, new_case("uid-keep")).await.unwrap();
        insert(&db, new_case("uid-other")).await.unwrap();

        let rows = list_for_subject(&db, "uid-keep").await.unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.subjectUid, "uid-keep");
        }
    }
}
