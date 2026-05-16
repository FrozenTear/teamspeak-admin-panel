//! `moderation_appeal` repo — Phase 9.2 appeals (PURA-305, migration
//! 0014).
//!
//! One row per appeal lodged against a `moderation_case`. `caseId` is a
//! plain `int` FK (schema FK convention, see the 0011 / 0014 headers);
//! referential integrity for a deleted case is kept by the
//! `moderation_case_cascade_appeal` event in migration 0014.
//!
//! State machine: an appeal opens `pending` and reaches one terminal
//! state — `upheld` / `overturned` (an operator decision via [`decide`])
//! or `withdrawn` (the appellant's own action). [`decide`] is the single
//! transition primitive; the route layer writes the paired
//! `moderation_case_action` (`appeal_decided`) + `admin_audit_log` rows.
//!
//! GDPR posture (mirrors `repos::moderation_notes`):
//!   - **No automatic TTL.** Appeals are moderation history.
//!   - **Access / portability (Art. 15 / 20).** [`export_for_subject`]
//!     returns every appeal a UID submitted.
//!   - **Erasure (Art. 17).** [`purge_for_subject`] hard-deletes every
//!     appeal a UID submitted.

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the 9.2 routes / decision workstreams

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationAppeal {
    pub id: i64,
    pub caseId: i64,
    pub submitterUid: String,
    pub identityProof: String,
    pub statement: String,
    pub status: String,
    pub decidedByUserId: Option<i64>,
    pub decisionNote: Option<String>,
    pub sourceIpHash: String,
    pub createdAt: DateTime<Utc>,
    pub decidedAt: Option<DateTime<Utc>>,
}

/// Caller-supplied fields on appeal creation. `id`, `status` (defaults to
/// `pending`), the decision fields and the timestamps are server-managed.
#[derive(Debug, Clone)]
pub struct NewModerationAppeal {
    pub caseId: i64,
    pub submitterUid: String,
    pub identityProof: String,
    pub statement: String,
    pub sourceIpHash: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    caseId,
    submitterUid,
    identityProof,
    statement,
    status,
    decidedByUserId,
    decisionNote,
    sourceIpHash,
    createdAt,
    decidedAt
";

pub async fn insert(db: &Database, new: NewModerationAppeal) -> Result<ModerationAppeal> {
    let sql = format!(
        "CREATE type::record('moderation_appeal', sequence::nextval('moderation_appeal_id'))
            CONTENT {{
                caseId:        $caseId,
                submitterUid:  $submitterUid,
                identityProof: $identityProof,
                statement:     $statement,
                sourceIpHash:  $sourceIpHash
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("caseId", new.caseId))
        .bind(("submitterUid", new.submitterUid))
        .bind(("identityProof", new.identityProof))
        .bind(("statement", new.statement))
        .bind(("sourceIpHash", new.sourceIpHash))
        .await
        .context("moderation_appeal insert query failed")?
        .check()?;
    let row: Option<ModerationAppeal> = resp.take(0)?;
    row.context("moderation_appeal insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ModerationAppeal>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('moderation_appeal', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("moderation_appeal find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Every appeal lodged against a case, newest-first. Backs the per-case
/// appeals pane.
pub async fn list_for_case(db: &Database, case_id: i64) -> Result<Vec<ModerationAppeal>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_appeal
            WHERE caseId = $caseId
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("caseId", case_id))
        .await
        .context("moderation_appeal list_for_case query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Record a terminal decision on an appeal: sets `status`, the deciding
/// operator, an optional note, and stamps `decidedAt`. Withdrawal by the
/// appellant passes `status = 'withdrawn'` with `decided_by_user_id =
/// None`. Returns the updated row, or `None` when no appeal has that id.
pub async fn decide(
    db: &Database,
    id: i64,
    status: &str,
    decided_by_user_id: Option<i64>,
    decision_note: Option<String>,
) -> Result<Option<ModerationAppeal>> {
    let sql = format!(
        "UPDATE type::record('moderation_appeal', $id) MERGE {{
            status:          $status,
            decidedByUserId: $decidedByUserId,
            decisionNote:    $decisionNote,
            decidedAt:       time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("status", status.to_string()))
        .bind(("decidedByUserId", decided_by_user_id))
        .bind(("decisionNote", decision_note))
        .await
        .context("moderation_appeal decide query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Delete one appeal. Returns `true` when a row was removed.
pub async fn delete(db: &Database, id: i64) -> Result<bool> {
    let existed = find_by_id(db, id).await?.is_some();
    db.query("DELETE type::record('moderation_appeal', $id);")
        .bind(("id", id))
        .await
        .context("moderation_appeal delete query failed")?
        .check()?;
    Ok(existed)
}

/// GDPR Art. 15 / 20 — every appeal a subject UID submitted,
/// newest-first. Backs a subject-data export.
pub async fn export_for_subject(
    db: &Database,
    submitter_uid: &str,
) -> Result<Vec<ModerationAppeal>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_appeal
            WHERE submitterUid = $uid
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("uid", submitter_uid.to_string()))
        .await
        .context("moderation_appeal export_for_subject query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// GDPR Art. 17 erasure — hard-delete every appeal a subject UID
/// submitted. Returns the number of appeals removed. The route layer
/// audits the purge via `admin_audit_log`.
pub async fn purge_for_subject(db: &Database, submitter_uid: &str) -> Result<u64> {
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let mut count_resp = db
        .query(
            "SELECT count() AS count FROM moderation_appeal
                WHERE submitterUid = $uid GROUP ALL;",
        )
        .bind(("uid", submitter_uid.to_string()))
        .await
        .context("moderation_appeal purge count query failed")?
        .check()?;
    let counted: Option<CountRow> = count_resp.take(0)?;
    let n = counted.map(|c| c.count.max(0) as u64).unwrap_or(0);

    db.query("DELETE moderation_appeal WHERE submitterUid = $uid;")
        .bind(("uid", submitter_uid.to_string()))
        .await
        .context("moderation_appeal purge_for_subject query failed")?
        .check()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::moderation_cases::{self, NewModerationCase};

    async fn fresh_db() -> std::sync::Arc<Database> {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        db
    }

    /// Open a real `moderation_case` so appeals have a case to contest
    /// and the cascade event has something to fire on.
    async fn open_case(db: &Database) -> i64 {
        moderation_cases::insert(
            db,
            NewModerationCase {
                serverConfigId: 1,
                virtualServerId: 1,
                subjectUid: "subj-1".into(),
                subjectNicknameSnapshot: "Subj".into(),
                origin: "operator".into(),
                originRef: None,
                reason: "banned for spam".into(),
                openedByUserId: Some(1),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn appeal(case_id: i64, submitter: &str) -> NewModerationAppeal {
        NewModerationAppeal {
            caseId: case_id,
            submitterUid: submitter.into(),
            identityProof: "matches ban identity".into(),
            statement: "it was not me".into(),
            sourceIpHash: "hash-xyz".into(),
        }
    }

    #[tokio::test]
    async fn insert_defaults_status_to_pending() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        let a = insert(&db, appeal(case_id, "appellant-a")).await.unwrap();
        assert_eq!(a.status, "pending");
        assert!(a.decidedByUserId.is_none());
        assert!(a.decidedAt.is_none());
        let got = find_by_id(&db, a.id).await.unwrap().expect("appeal exists");
        assert_eq!(got.caseId, case_id);
        assert_eq!(got.submitterUid, "appellant-a");
    }

    #[tokio::test]
    async fn decide_records_operator_note_and_timestamp() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        let a = insert(&db, appeal(case_id, "appellant-b")).await.unwrap();
        let decided = decide(&db, a.id, "overturned", Some(5), Some("ban lifted".into()))
            .await
            .unwrap()
            .expect("appeal exists");
        assert_eq!(decided.status, "overturned");
        assert_eq!(decided.decidedByUserId, Some(5));
        assert_eq!(decided.decisionNote.as_deref(), Some("ban lifted"));
        assert!(decided.decidedAt.is_some());
    }

    #[tokio::test]
    async fn withdraw_is_a_decide_without_an_operator() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        let a = insert(&db, appeal(case_id, "appellant-c")).await.unwrap();
        let withdrawn = decide(&db, a.id, "withdrawn", None, None)
            .await
            .unwrap()
            .expect("appeal exists");
        assert_eq!(withdrawn.status, "withdrawn");
        assert!(withdrawn.decidedByUserId.is_none());
    }

    #[tokio::test]
    async fn list_for_case_scopes_to_case_newest_first() {
        let db = fresh_db().await;
        let case_a = open_case(&db).await;
        let case_b = open_case(&db).await;
        insert(&db, appeal(case_a, "first")).await.unwrap();
        insert(&db, appeal(case_a, "second")).await.unwrap();
        insert(&db, appeal(case_b, "elsewhere")).await.unwrap();

        let rows = list_for_case(&db, case_a).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].submitterUid, "second", "newest first");
        assert_eq!(list_for_case(&db, case_b).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_a_single_appeal() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        let a = insert(&db, appeal(case_id, "appellant-d")).await.unwrap();
        assert!(delete(&db, a.id).await.unwrap());
        assert!(find_by_id(&db, a.id).await.unwrap().is_none());
        assert!(
            !delete(&db, a.id).await.unwrap(),
            "second delete is a no-op"
        );
    }

    #[tokio::test]
    async fn export_and_purge_are_subject_scoped() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        insert(&db, appeal(case_id, "uid-erase")).await.unwrap();
        insert(&db, appeal(case_id, "uid-erase")).await.unwrap();
        insert(&db, appeal(case_id, "uid-keep")).await.unwrap();

        assert_eq!(export_for_subject(&db, "uid-erase").await.unwrap().len(), 2);

        let purged = purge_for_subject(&db, "uid-erase").await.unwrap();
        assert_eq!(purged, 2);
        assert!(
            export_for_subject(&db, "uid-erase")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            export_for_subject(&db, "uid-keep").await.unwrap().len(),
            1,
            "erasure is subject-scoped"
        );
    }

    #[tokio::test]
    async fn deleting_a_case_cascades_to_its_appeals() {
        let db = fresh_db().await;
        let case_id = open_case(&db).await;
        let other_case = open_case(&db).await;
        let doomed = insert(&db, appeal(case_id, "appellant-e")).await.unwrap();
        let survivor = insert(&db, appeal(other_case, "appellant-f"))
            .await
            .unwrap();

        // Deleting the case fires `moderation_case_cascade_appeal` (0014).
        db.query("DELETE type::record('moderation_case', $id);")
            .bind(("id", case_id))
            .await
            .unwrap()
            .check()
            .unwrap();

        assert!(
            find_by_id(&db, doomed.id).await.unwrap().is_none(),
            "appeal of the deleted case is cascaded away"
        );
        assert!(
            find_by_id(&db, survivor.id).await.unwrap().is_some(),
            "appeal of an unrelated case survives"
        );
    }
}
