//! `moderation_note` repo — Phase 9.0 moderation (PURA-285, migration
//! 0011).
//!
//! Free-text moderator notes on a subject UID, independent of cases
//! (design brief §5). Unlike `moderation_case_action` this table is
//! mutable: a note can be edited and deleted.
//!
//! GDPR posture (docs/admin/moderation-data.md §4 — the open design item
//! the brief §8 delegates to 9.0-data):
//!   - **No automatic TTL.** Notes are moderation history retained under
//!     legitimate interest; an expiry janitor would silently destroy
//!     moderation context. Deliberately unlike `admin_audit_log`.
//!   - **Access / portability (Art. 15 / 20).** [`list_for_subject`]
//!     returns every note for a UID; it backs both the history pane and
//!     a subject-data export.
//!   - **Erasure (Art. 17).** [`purge_for_subject`] hard-deletes every
//!     note for a UID. Hard delete — a soft-deleted row would still hold
//!     the personal data. The route layer audits the purge via
//!     `admin_audit_log`.

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the 9.0-routes workstream (PURA-286)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationNote {
    pub id: i64,
    pub subjectUid: String,
    pub body: String,
    pub authorUserId: Option<i64>,
    pub authorUsernameSnapshot: String,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

/// Caller-supplied fields on note creation. `id` and the timestamps are
/// server-managed.
#[derive(Debug, Clone)]
pub struct NewModerationNote {
    pub subjectUid: String,
    pub body: String,
    pub authorUserId: Option<i64>,
    pub authorUsernameSnapshot: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    subjectUid,
    body,
    authorUserId,
    authorUsernameSnapshot,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewModerationNote) -> Result<ModerationNote> {
    let sql = format!(
        "CREATE type::record('moderation_note', sequence::nextval('moderation_note_id'))
            CONTENT {{
                subjectUid:             $subjectUid,
                body:                   $body,
                authorUserId:           $authorUserId,
                authorUsernameSnapshot: $authorUsernameSnapshot
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("subjectUid", new.subjectUid))
        .bind(("body", new.body))
        .bind(("authorUserId", new.authorUserId))
        .bind(("authorUsernameSnapshot", new.authorUsernameSnapshot))
        .await
        .context("moderation_note insert query failed")?
        .check()?;
    let row: Option<ModerationNote> = resp.take(0)?;
    row.context("moderation_note insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ModerationNote>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('moderation_note', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("moderation_note find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Every note for a subject UID, newest-first. Backs the per-user
/// history pane **and** the GDPR Art. 15 / 20 subject-data export.
pub async fn list_for_subject(db: &Database, subject_uid: &str) -> Result<Vec<ModerationNote>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_note
            WHERE subjectUid = $uid
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("uid", subject_uid.to_string()))
        .await
        .context("moderation_note list_for_subject query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Edit a note's body. Refreshes `updatedAt`. Returns the updated row,
/// or `None` when no note has that id.
pub async fn update_body(db: &Database, id: i64, body: String) -> Result<Option<ModerationNote>> {
    let sql = format!(
        "UPDATE type::record('moderation_note', $id) MERGE {{
            body:      $body,
            updatedAt: time::now()
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("body", body))
        .await
        .context("moderation_note update_body query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Delete one note. Returns `true` when a row was removed.
pub async fn delete(db: &Database, id: i64) -> Result<bool> {
    // SurrealDB's `DELETE` returns nothing by default and `RETURN BEFORE`
    // yields a raw record id, so existence is checked separately.
    let existed = find_by_id(db, id).await?.is_some();
    db.query("DELETE type::record('moderation_note', $id);")
        .bind(("id", id))
        .await
        .context("moderation_note delete query failed")?
        .check()?;
    Ok(existed)
}

/// GDPR Art. 17 erasure — hard-delete every note for a subject UID.
/// Returns the number of notes removed. The route layer is responsible
/// for the `admin_audit_log` row recording that an erasure happened.
pub async fn purge_for_subject(db: &Database, subject_uid: &str) -> Result<u64> {
    #[derive(Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct CountRow {
        count: i64,
    }
    let mut count_resp = db
        .query("SELECT count() AS count FROM moderation_note WHERE subjectUid = $uid GROUP ALL;")
        .bind(("uid", subject_uid.to_string()))
        .await
        .context("moderation_note purge count query failed")?
        .check()?;
    let counted: Option<CountRow> = count_resp.take(0)?;
    let n = counted.map(|c| c.count.max(0) as u64).unwrap_or(0);

    db.query("DELETE moderation_note WHERE subjectUid = $uid;")
        .bind(("uid", subject_uid.to_string()))
        .await
        .context("moderation_note purge_for_subject query failed")?
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

    fn note(uid: &str, body: &str) -> NewModerationNote {
        NewModerationNote {
            subjectUid: uid.into(),
            body: body.into(),
            authorUserId: Some(1),
            authorUsernameSnapshot: "mod1".into(),
        }
    }

    #[tokio::test]
    async fn insert_and_find_round_trip() {
        let db = fresh_db().await;
        let n = insert(&db, note("uid-a", "knows the rules")).await.unwrap();
        let got = find_by_id(&db, n.id).await.unwrap().expect("note exists");
        assert_eq!(got.body, "knows the rules");
        assert_eq!(got.subjectUid, "uid-a");
    }

    #[tokio::test]
    async fn update_body_refreshes_updated_at() {
        let db = fresh_db().await;
        let n = insert(&db, note("uid-b", "original")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let edited = update_body(&db, n.id, "revised".into())
            .await
            .unwrap()
            .expect("note exists");
        assert_eq!(edited.body, "revised");
        assert!(edited.updatedAt >= n.updatedAt);
        assert_eq!(edited.createdAt, n.createdAt, "createdAt is immutable");
    }

    #[tokio::test]
    async fn delete_removes_a_single_note() {
        let db = fresh_db().await;
        let n = insert(&db, note("uid-c", "x")).await.unwrap();
        assert!(delete(&db, n.id).await.unwrap());
        assert!(find_by_id(&db, n.id).await.unwrap().is_none());
        assert!(
            !delete(&db, n.id).await.unwrap(),
            "second delete is a no-op"
        );
    }

    #[tokio::test]
    async fn list_for_subject_scopes_to_uid_newest_first() {
        let db = fresh_db().await;
        insert(&db, note("uid-keep", "first")).await.unwrap();
        insert(&db, note("uid-keep", "second")).await.unwrap();
        insert(&db, note("uid-other", "elsewhere")).await.unwrap();

        let rows = list_for_subject(&db, "uid-keep").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].body, "second", "newest first");
    }

    #[tokio::test]
    async fn purge_for_subject_erases_only_that_subject() {
        let db = fresh_db().await;
        insert(&db, note("uid-erase", "a")).await.unwrap();
        insert(&db, note("uid-erase", "b")).await.unwrap();
        insert(&db, note("uid-survive", "c")).await.unwrap();

        let purged = purge_for_subject(&db, "uid-erase").await.unwrap();
        assert_eq!(purged, 2);
        assert!(list_for_subject(&db, "uid-erase").await.unwrap().is_empty());
        assert_eq!(
            list_for_subject(&db, "uid-survive").await.unwrap().len(),
            1,
            "erasure is subject-scoped"
        );
    }
}
