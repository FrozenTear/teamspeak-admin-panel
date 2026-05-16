//! `moderation_case_action` repo — Phase 9.0 moderation (PURA-285,
//! migration 0011).
//!
//! Append-only per-case action timeline (design brief §5 / §7). Every
//! case state transition and every kick/ban/mute/note writes one row
//! here. The module is **INSERT-only by contract** — the only deletion
//! path is the `moderation_case_cascade` event in migration 0011, which
//! removes a case's actions when the case itself is deleted.
//!
//! `caseId` is a plain `int` FK into `moderation_case` (see the FK-
//! convention note in `0011_moderation.surql`).

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the 9.0-routes workstream (PURA-286)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// Valid `actionKind` values — the schema mirror of migration
/// `0016_moderation_appeal_filed_action_kind`'s `ASSERT`. `warn` is the
/// Phase 9.1 automod effect kind (PURA-297 §4.3); `unban` is the Phase
/// 9.1.4 automod revert kind (PURA-303); `ban_ip` is the operator IP-ban
/// kind (`routes/moderation/actions.rs`); `appeal_filed` is the Phase 9.2
/// public-appeal marker (PURA-307). Keep this in sync with that ASSERT.
pub const ACTION_KINDS: &[&str] = &[
    "warn",
    "kick",
    "ban",
    "ban_ip",
    "mute",
    "unmute",
    "unban",
    "note",
    "resolve",
    "reopen",
    "appeal_filed",
];

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationCaseAction {
    pub id: i64,
    pub caseId: i64,
    pub actorUserId: Option<i64>,
    pub actorUsernameSnapshot: String,
    pub actionKind: String,
    pub reason: String,
    pub tsRef: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub createdAt: DateTime<Utc>,
}

/// Caller-supplied fields on action append. `id` and `createdAt` are
/// server-managed.
#[derive(Debug, Clone)]
pub struct NewModerationCaseAction {
    pub caseId: i64,
    pub actorUserId: Option<i64>,
    pub actorUsernameSnapshot: String,
    pub actionKind: String,
    pub reason: String,
    pub tsRef: Option<String>,
    pub payload: Option<serde_json::Value>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    caseId,
    actorUserId,
    actorUsernameSnapshot,
    actionKind,
    reason,
    tsRef,
    payload,
    createdAt
";

/// Append one action to a case's timeline.
pub async fn insert(db: &Database, new: NewModerationCaseAction) -> Result<ModerationCaseAction> {
    let sql = format!(
        "CREATE type::record('moderation_case_action', sequence::nextval('moderation_case_action_id'))
            CONTENT {{
                caseId:                $caseId,
                actorUserId:           $actorUserId,
                actorUsernameSnapshot: $actorUsernameSnapshot,
                actionKind:            $actionKind,
                reason:                $reason,
                tsRef:                 $tsRef,
                payload:               $payload
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("caseId", new.caseId))
        .bind(("actorUserId", new.actorUserId))
        .bind(("actorUsernameSnapshot", new.actorUsernameSnapshot))
        .bind(("actionKind", new.actionKind))
        .bind(("reason", new.reason))
        .bind(("tsRef", new.tsRef))
        .bind(("payload", new.payload))
        .await
        .context("moderation_case_action insert query failed")?
        .check()?;
    let row: Option<ModerationCaseAction> = resp.take(0)?;
    row.context("moderation_case_action insert returned no row")
}

/// The timeline for one case, oldest-first (chronological — a timeline
/// reads forward).
pub async fn list_for_case(db: &Database, case_id: i64) -> Result<Vec<ModerationCaseAction>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_case_action
            WHERE caseId = $cid
            ORDER BY createdAt ASC, id ASC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("cid", case_id))
        .await
        .context("moderation_case_action list_for_case query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Actions across several cases, newest-first — backs the per-user
/// history pane, which fans in over every case for a subject UID.
pub async fn list_for_cases(db: &Database, case_ids: &[i64]) -> Result<Vec<ModerationCaseAction>> {
    if case_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_case_action
            WHERE caseId IN $cids
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("cids", case_ids.to_vec()))
        .await
        .context("moderation_case_action list_for_cases query failed")?
        .check()?;
    Ok(resp.take(0)?)
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

    async fn open_case(db: &Database, uid: &str) -> i64 {
        moderation_cases::insert(
            db,
            NewModerationCase {
                serverConfigId: 1,
                virtualServerId: 1,
                subjectUid: uid.into(),
                subjectNicknameSnapshot: "Nick".into(),
                origin: "operator".into(),
                originRef: None,
                reason: "spam".into(),
                openedByUserId: Some(1),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn action(case_id: i64, kind: &str) -> NewModerationCaseAction {
        NewModerationCaseAction {
            caseId: case_id,
            actorUserId: Some(1),
            actorUsernameSnapshot: "mod1".into(),
            actionKind: kind.into(),
            reason: "policy".into(),
            tsRef: None,
            payload: None,
        }
    }

    #[tokio::test]
    async fn insert_round_trips_with_flexible_payload() {
        let db = fresh_db().await;
        let cid = open_case(&db, "uid-a").await;
        let row = insert(
            &db,
            NewModerationCaseAction {
                tsRef: Some("ban-42".into()),
                payload: Some(serde_json::json!({ "durationSecs": 600, "ip": false })),
                ..action(cid, "ban")
            },
        )
        .await
        .unwrap();
        assert_eq!(row.actionKind, "ban");
        assert_eq!(row.tsRef.as_deref(), Some("ban-42"));
        assert_eq!(
            row.payload
                .as_ref()
                .and_then(|p| p.get("durationSecs"))
                .and_then(|v| v.as_i64()),
            Some(600)
        );
    }

    #[tokio::test]
    async fn list_for_case_is_chronological() {
        let db = fresh_db().await;
        let cid = open_case(&db, "uid-b").await;
        insert(&db, action(cid, "kick")).await.unwrap();
        insert(&db, action(cid, "ban")).await.unwrap();
        insert(&db, action(cid, "resolve")).await.unwrap();

        let timeline = list_for_case(&db, cid).await.unwrap();
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0].actionKind, "kick");
        assert_eq!(timeline[2].actionKind, "resolve");
    }

    #[tokio::test]
    async fn list_for_cases_fans_in_newest_first() {
        let db = fresh_db().await;
        let c1 = open_case(&db, "uid-c").await;
        let c2 = open_case(&db, "uid-c").await;
        insert(&db, action(c1, "kick")).await.unwrap();
        insert(&db, action(c2, "ban")).await.unwrap();

        let rows = list_for_cases(&db, &[c1, c2]).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].actionKind, "ban", "newest first");

        assert!(list_for_cases(&db, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_case_cascades_its_actions() {
        // Pins the `moderation_case_cascade` event in migration 0011.
        let db = fresh_db().await;
        let cid = open_case(&db, "uid-d").await;
        insert(&db, action(cid, "kick")).await.unwrap();
        insert(&db, action(cid, "ban")).await.unwrap();
        assert_eq!(list_for_case(&db, cid).await.unwrap().len(), 2);

        db.query("DELETE type::record('moderation_case', $id);")
            .bind(("id", cid))
            .await
            .unwrap()
            .check()
            .unwrap();

        assert!(
            list_for_case(&db, cid).await.unwrap().is_empty(),
            "case delete cascades to its action timeline"
        );
    }
}
