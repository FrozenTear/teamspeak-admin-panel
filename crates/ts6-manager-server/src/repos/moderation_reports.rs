//! `moderation_report` repo — Phase 9.2 appeals (PURA-305, migration
//! 0014).
//!
//! The report intake queue: a reporter files a `moderation_report`, an
//! operator triages it, and the report is either *promoted* to a
//! `moderation_case` or *dismissed* (design brief / PURA-269 §9, plan §5).
//!
//! GDPR posture (mirrors `repos::moderation_notes`):
//!   - **No automatic TTL.** Reports are moderation history retained
//!     under legitimate interest.
//!   - **Access / portability (Art. 15 / 20).** [`export_for_subject`]
//!     returns every report filed *against* a UID/nickname.
//!   - **Erasure (Art. 17).** [`purge_for_subject`] hard-deletes every
//!     report filed against a UID/nickname.
//!
//! Both subject-scoped helpers key on `subjectUidOrNickname` — the
//! accused — consistent with `moderation_note` keying on `subjectUid`. A
//! report also names a `reporterUid`; reporter-initiated erasure is a
//! distinct concern left to the route layer, which can scope a purge on
//! `reporterUid` when a confidential reporter exercises their own rights.

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the 9.2 routes / triage workstreams

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationReport {
    pub id: i64,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub reporterUid: String,
    pub subjectUidOrNickname: String,
    pub category: String,
    pub statement: String,
    pub evidenceUrl: Option<String>,
    pub status: String,
    pub caseId: Option<i64>,
    pub triagedByUserId: Option<i64>,
    pub sourceIpHash: String,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

/// Caller-supplied fields on report creation. `id`, `status` (defaults to
/// `pending`), `caseId`, `triagedByUserId` and the timestamps are
/// server-managed.
#[derive(Debug, Clone)]
pub struct NewModerationReport {
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub reporterUid: String,
    pub subjectUidOrNickname: String,
    pub category: String,
    pub statement: String,
    pub evidenceUrl: Option<String>,
    pub sourceIpHash: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    serverConfigId,
    virtualServerId,
    reporterUid,
    subjectUidOrNickname,
    category,
    statement,
    evidenceUrl,
    status,
    caseId,
    triagedByUserId,
    sourceIpHash,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewModerationReport) -> Result<ModerationReport> {
    let sql = format!(
        "CREATE type::record('moderation_report', sequence::nextval('moderation_report_id'))
            CONTENT {{
                serverConfigId:       $serverConfigId,
                virtualServerId:      $virtualServerId,
                reporterUid:          $reporterUid,
                subjectUidOrNickname: $subjectUidOrNickname,
                category:             $category,
                statement:            $statement,
                evidenceUrl:          $evidenceUrl,
                sourceIpHash:         $sourceIpHash
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("virtualServerId", new.virtualServerId))
        .bind(("reporterUid", new.reporterUid))
        .bind(("subjectUidOrNickname", new.subjectUidOrNickname))
        .bind(("category", new.category))
        .bind(("statement", new.statement))
        .bind(("evidenceUrl", new.evidenceUrl))
        .bind(("sourceIpHash", new.sourceIpHash))
        .await
        .context("moderation_report insert query failed")?
        .check()?;
    let row: Option<ModerationReport> = resp.take(0)?;
    row.context("moderation_report insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ModerationReport>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('moderation_report', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("moderation_report find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Every report in a given triage `status` (`pending` / `promoted` /
/// `dismissed`), newest-first. Backs the operator triage queue.
pub async fn list_by_status(db: &Database, status: &str) -> Result<Vec<ModerationReport>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_report
            WHERE status = $status
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("status", status.to_string()))
        .await
        .context("moderation_report list_by_status query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Promote a report to a `moderation_case`: records the case FK, the
/// triaging operator, and flips `status` to `promoted`. Returns the
/// updated row, or `None` when no report has that id.
pub async fn promote(
    db: &Database,
    id: i64,
    case_id: i64,
    triaged_by_user_id: Option<i64>,
) -> Result<Option<ModerationReport>> {
    let sql = format!(
        "UPDATE type::record('moderation_report', $id) MERGE {{
            status:          'promoted',
            caseId:          $caseId,
            triagedByUserId: $triagedByUserId,
            updatedAt:       time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("caseId", case_id))
        .bind(("triagedByUserId", triaged_by_user_id))
        .await
        .context("moderation_report promote query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Dismiss a report without opening a case. Records the triaging
/// operator and flips `status` to `dismissed`. Returns the updated row,
/// or `None` when no report has that id.
pub async fn dismiss(
    db: &Database,
    id: i64,
    triaged_by_user_id: Option<i64>,
) -> Result<Option<ModerationReport>> {
    let sql = format!(
        "UPDATE type::record('moderation_report', $id) MERGE {{
            status:          'dismissed',
            triagedByUserId: $triagedByUserId,
            updatedAt:       time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("triagedByUserId", triaged_by_user_id))
        .await
        .context("moderation_report dismiss query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Delete one report. Returns `true` when a row was removed.
pub async fn delete(db: &Database, id: i64) -> Result<bool> {
    let existed = find_by_id(db, id).await?.is_some();
    db.query("DELETE type::record('moderation_report', $id);")
        .bind(("id", id))
        .await
        .context("moderation_report delete query failed")?
        .check()?;
    Ok(existed)
}

/// GDPR Art. 15 / 20 — every report filed against a subject
/// UID/nickname, newest-first. Backs a subject-data export.
pub async fn export_for_subject(db: &Database, subject: &str) -> Result<Vec<ModerationReport>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_report
            WHERE subjectUidOrNickname = $subject
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("subject", subject.to_string()))
        .await
        .context("moderation_report export_for_subject query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// GDPR Art. 17 erasure — hard-delete every report filed against a
/// subject UID/nickname. Returns the number of reports removed. The
/// route layer audits the purge via `admin_audit_log`.
pub async fn purge_for_subject(db: &Database, subject: &str) -> Result<u64> {
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let mut count_resp = db
        .query(
            "SELECT count() AS count FROM moderation_report
                WHERE subjectUidOrNickname = $subject GROUP ALL;",
        )
        .bind(("subject", subject.to_string()))
        .await
        .context("moderation_report purge count query failed")?
        .check()?;
    let counted: Option<CountRow> = count_resp.take(0)?;
    let n = counted.map(|c| c.count.max(0) as u64).unwrap_or(0);

    db.query("DELETE moderation_report WHERE subjectUidOrNickname = $subject;")
        .bind(("subject", subject.to_string()))
        .await
        .context("moderation_report purge_for_subject query failed")?
        .check()?;
    Ok(n)
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

    fn report(reporter: &str, subject: &str) -> NewModerationReport {
        NewModerationReport {
            serverConfigId: 1,
            virtualServerId: 1,
            reporterUid: reporter.into(),
            subjectUidOrNickname: subject.into(),
            category: "harassment".into(),
            statement: "was rude in voice".into(),
            evidenceUrl: None,
            sourceIpHash: "hash-abc".into(),
        }
    }

    #[tokio::test]
    async fn insert_defaults_status_to_pending() {
        let db = fresh_db().await;
        let r = insert(&db, report("rep-a", "subj-a")).await.unwrap();
        assert_eq!(r.status, "pending");
        assert!(r.caseId.is_none());
        assert!(r.triagedByUserId.is_none());
        let got = find_by_id(&db, r.id).await.unwrap().expect("report exists");
        assert_eq!(got.reporterUid, "rep-a");
        assert_eq!(got.subjectUidOrNickname, "subj-a");
    }

    #[tokio::test]
    async fn promote_records_case_and_operator() {
        let db = fresh_db().await;
        let r = insert(&db, report("rep-b", "subj-b")).await.unwrap();
        let promoted = promote(&db, r.id, 42, Some(7))
            .await
            .unwrap()
            .expect("report exists");
        assert_eq!(promoted.status, "promoted");
        assert_eq!(promoted.caseId, Some(42));
        assert_eq!(promoted.triagedByUserId, Some(7));
    }

    #[tokio::test]
    async fn dismiss_flips_status_without_a_case() {
        let db = fresh_db().await;
        let r = insert(&db, report("rep-c", "subj-c")).await.unwrap();
        let dismissed = dismiss(&db, r.id, Some(9))
            .await
            .unwrap()
            .expect("report exists");
        assert_eq!(dismissed.status, "dismissed");
        assert!(dismissed.caseId.is_none());
        assert_eq!(dismissed.triagedByUserId, Some(9));
    }

    #[tokio::test]
    async fn list_by_status_scopes_to_status_newest_first() {
        let db = fresh_db().await;
        insert(&db, report("r1", "subj-x")).await.unwrap();
        let second = insert(&db, report("r2", "subj-y")).await.unwrap();
        dismiss(&db, second.id, Some(1)).await.unwrap();

        let pending = list_by_status(&db, "pending").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].reporterUid, "r1");
        assert_eq!(list_by_status(&db, "dismissed").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_a_single_report() {
        let db = fresh_db().await;
        let r = insert(&db, report("rep-d", "subj-d")).await.unwrap();
        assert!(delete(&db, r.id).await.unwrap());
        assert!(find_by_id(&db, r.id).await.unwrap().is_none());
        assert!(
            !delete(&db, r.id).await.unwrap(),
            "second delete is a no-op"
        );
    }

    #[tokio::test]
    async fn export_and_purge_are_subject_scoped() {
        let db = fresh_db().await;
        insert(&db, report("rep-e", "subj-erase")).await.unwrap();
        insert(&db, report("rep-f", "subj-erase")).await.unwrap();
        insert(&db, report("rep-g", "subj-keep")).await.unwrap();

        assert_eq!(
            export_for_subject(&db, "subj-erase").await.unwrap().len(),
            2
        );

        let purged = purge_for_subject(&db, "subj-erase").await.unwrap();
        assert_eq!(purged, 2);
        assert!(
            export_for_subject(&db, "subj-erase")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            export_for_subject(&db, "subj-keep").await.unwrap().len(),
            1,
            "erasure is subject-scoped"
        );
    }
}
