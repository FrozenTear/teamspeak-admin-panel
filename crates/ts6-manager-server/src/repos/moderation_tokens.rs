//! `moderation_token` repo — Phase 9.2 token store (PURA-311, migration
//! 0015).
//!
//! The storage substrate for the split-token flows described in the
//! [Token Store Security Spec](/PURA/issues/PURA-306#document-token-security-spec):
//! one unified table behind both the report-challenge and appeal tokens,
//! discriminated by `kind`.
//!
//! This repo owns **storage only**. Token mint (CSPRNG `lookup_id` +
//! `secret`), the SHA-256 hashing, and the `subtle` constant-time
//! compare are owned by SecurityEngineer ([PURA-306](/PURA/issues/PURA-306)).
//! The contract this repo upholds for that work:
//!
//!   - **Hashed at rest (spec §2 / §3).** Only `secretHash` — the
//!     lowercase-hex SHA-256 of the secret half — is ever persisted.
//!     The plaintext secret is never written, and this repo never logs
//!     or traces token material (no `tracing` call carries `secretHash`,
//!     `lookupId`, or the secret).
//!   - **Atomic single-use (spec §4).** [`consume`] is one conditional
//!     `UPDATE` whose `WHERE` carries the `usedAt IS NONE` and
//!     `expiresAt > time::now()` predicates. There is no read-then-write
//!     and so no TOCTOU window: if two requests race the same valid
//!     token, exactly one flips `usedAt`; the other matches zero rows.
//!   - **Compare before consume (spec §4).** [`find_by_lookup_id`]
//!     fetches the row for the constant-time `secretHash` compare that
//!     the caller performs *before* calling [`consume`] — a wrong secret
//!     must not burn a valid token.
//!   - **Bounded table (spec §6).** [`delete_expired`] is the periodic
//!     sweep, mirroring [`crate::repos::refresh_tokens::delete_expired`].

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by the PURA-306 mint/verify workstream

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// Valid `kind` discriminator values (spec §2). Exactly one binding
/// field is set per row: `report_challenge` rows carry `boundUid`,
/// `appeal` rows carry `caseId`.
pub const KINDS: &[&str] = &["report_challenge", "appeal"];

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModerationToken {
    pub id: i64,
    pub kind: String,
    pub lookupId: String,
    /// Lowercase-hex SHA-256 of the secret half. Never the plaintext.
    pub secretHash: String,
    pub boundUid: Option<String>,
    pub caseId: Option<i64>,
    pub expiresAt: DateTime<Utc>,
    /// `None` until the token is consumed; set exactly once by [`consume`].
    pub usedAt: Option<DateTime<Utc>>,
    pub createdAt: DateTime<Utc>,
}

/// Caller-supplied fields at mint. `id`, `usedAt` (starts `NONE`) and
/// `createdAt` are server-managed. The caller (PURA-306) has already
/// generated `lookupId` and hashed the secret into `secretHash`.
#[derive(Debug, Clone)]
pub struct NewModerationToken {
    pub kind: String,
    pub lookupId: String,
    pub secretHash: String,
    pub boundUid: Option<String>,
    pub caseId: Option<i64>,
    pub expiresAt: DateTime<Utc>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    kind,
    lookupId,
    secretHash,
    boundUid,
    caseId,
    expiresAt,
    usedAt,
    createdAt
";

/// Mint a token row. The `UNIQUE` index on `lookupId` turns a collision
/// (astronomically unlikely with 12 CSPRNG bytes) into a hard error
/// rather than a silent overwrite of a live token.
pub async fn insert(db: &Database, new: NewModerationToken) -> Result<ModerationToken> {
    let sql = format!(
        "CREATE type::record('moderation_token', sequence::nextval('moderation_token_id'))
            CONTENT {{
                kind:       $kind,
                lookupId:   $lookupId,
                secretHash: $secretHash,
                boundUid:   $boundUid,
                caseId:     $caseId,
                expiresAt:  $expiresAt
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("kind", new.kind))
        .bind(("lookupId", new.lookupId))
        .bind(("secretHash", new.secretHash))
        .bind(("boundUid", new.boundUid))
        .bind(("caseId", new.caseId))
        .bind(("expiresAt", new.expiresAt))
        .await
        .context("moderation_token insert query failed")?
        .check()?;
    let row: Option<ModerationToken> = resp.take(0)?;
    row.context("moderation_token insert returned no row")
}

/// Look up by integer id. Typed-CRUD completeness; the token flows
/// themselves key on `lookupId`.
pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ModerationToken>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('moderation_token', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("moderation_token find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Fetch the row for a `lookupId` (spec §4). This is the read the verify
/// path does *before* the constant-time `secretHash` compare — the
/// compare gates whether [`consume`] is issued at all, so a wrong secret
/// never burns a valid token. Returns the row regardless of `usedAt` /
/// `expiresAt` state; freshness is the caller's concern, and [`consume`]
/// re-checks both atomically.
pub async fn find_by_lookup_id(db: &Database, lookup_id: &str) -> Result<Option<ModerationToken>> {
    let sql =
        format!("SELECT {PROJECTION} FROM moderation_token WHERE lookupId = $lookup LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("lookup", lookup_id.to_string()))
        .await
        .context("moderation_token find_by_lookup_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Atomically consume a token (spec §4 — single-use, no TOCTOU).
///
/// One conditional `UPDATE`: the `usedAt IS NONE` and
/// `expiresAt > time::now()` predicates live *inside* the mutating
/// statement, so there is no read-then-write window. SurrealDB applies a
/// single-statement `UPDATE` atomically — if two requests race the same
/// valid token, exactly one flips `usedAt` from `NONE` and the other
/// matches zero rows.
///
/// Returns:
///   - `Some(token)` — this call won: `usedAt` was `NONE` and the token
///     was unexpired, and this call set `usedAt`. The returned row is
///     the **BEFORE** image (spec §4), so its `usedAt` is `None` and its
///     binding fields (`kind`, `boundUid`, `caseId`) reflect mint state.
///   - `None` — already used, expired, or no such `lookupId`. The three
///     collapse to one indistinguishable outcome so the caller's error
///     is not an enumeration oracle.
///
/// The caller MUST have already verified the `secretHash` constant-time
/// compare against the [`find_by_lookup_id`] row before calling this —
/// `consume` proves freshness and single-use, not secret correctness.
pub async fn consume(db: &Database, lookup_id: &str) -> Result<Option<ModerationToken>> {
    // `RETURN BEFORE`, projected through `record::id($before.id)` so the
    // returned shape matches `PROJECTION` (a raw `RETURN BEFORE` would
    // hand back the record id as a `RecordId`, not the `int` the typed
    // struct expects).
    let sql = "
        UPDATE moderation_token
           SET usedAt = time::now()
         WHERE lookupId = $lookup
           AND usedAt IS NONE
           AND expiresAt > time::now()
        RETURN
            record::id($before.id) AS id,
            $before.kind        AS kind,
            $before.lookupId    AS lookupId,
            $before.secretHash  AS secretHash,
            $before.boundUid    AS boundUid,
            $before.caseId      AS caseId,
            $before.expiresAt   AS expiresAt,
            $before.usedAt      AS usedAt,
            $before.createdAt   AS createdAt;
    ";
    let mut resp = db
        .query(sql)
        .bind(("lookup", lookup_id.to_string()))
        .await
        .context("moderation_token consume query failed")?
        .check()?;
    let rows: Vec<ModerationToken> = resp.take(0)?;
    Ok(rows.into_iter().next())
}

/// Every token bound to a case, newest-first. Backs appeal re-issue /
/// "one pending appeal per case" reads (spec §2); the `moderation_token_case`
/// index serves the `caseId` filter.
pub async fn list_for_case(db: &Database, case_id: i64) -> Result<Vec<ModerationToken>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM moderation_token
            WHERE caseId = $caseId
            ORDER BY createdAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("caseId", case_id))
        .await
        .context("moderation_token list_for_case query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Delete one token by id. Returns `true` when a row was removed.
pub async fn delete(db: &Database, id: i64) -> Result<bool> {
    let existed = find_by_id(db, id).await?.is_some();
    db.query("DELETE type::record('moderation_token', $id);")
        .bind(("id", id))
        .await
        .context("moderation_token delete query failed")?
        .check()?;
    Ok(existed)
}

/// Sweep tokens whose `expiresAt` has passed (spec §6 — keep the table
/// bounded). Mirrors [`crate::repos::refresh_tokens::delete_expired`];
/// the table is small enough that the unindexed scan is fine. Already
/// consumed-but-unexpired tokens are intentionally kept until they
/// expire — `usedAt` is moderation history a verify path may want to
/// distinguish "used" from "unknown" on, even though the public error
/// does not.
pub async fn delete_expired(db: &Database) -> Result<()> {
    db.query("DELETE moderation_token WHERE expiresAt < time::now();")
        .await
        .context("moderation_token delete_expired query failed")?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::moderation_cases::{self, NewModerationCase};
    use chrono::Duration;

    async fn fresh_db() -> std::sync::Arc<Database> {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        db
    }

    /// An appeal token bound to a case, expiring `ttl` from now.
    fn appeal_token(lookup: &str, case_id: i64, ttl: Duration) -> NewModerationToken {
        NewModerationToken {
            kind: "appeal".into(),
            lookupId: lookup.into(),
            secretHash: format!("sha256-of-{lookup}"),
            boundUid: None,
            caseId: Some(case_id),
            expiresAt: Utc::now() + ttl,
        }
    }

    #[tokio::test]
    async fn insert_round_trips_through_both_lookups() {
        let db = fresh_db().await;
        let t = insert(&db, appeal_token("lk-a", 1, Duration::days(30)))
            .await
            .unwrap();
        assert_eq!(t.kind, "appeal");
        assert_eq!(t.caseId, Some(1));
        assert!(t.boundUid.is_none());
        assert!(t.usedAt.is_none(), "a fresh token is unconsumed");

        let by_id = find_by_id(&db, t.id).await.unwrap().expect("by id");
        assert_eq!(by_id.lookupId, "lk-a");
        let by_lookup = find_by_lookup_id(&db, "lk-a")
            .await
            .unwrap()
            .expect("by lookup");
        assert_eq!(by_lookup.id, t.id);
        assert_eq!(by_lookup.secretHash, "sha256-of-lk-a");
    }

    #[tokio::test]
    async fn report_challenge_token_binds_uid_not_case() {
        let db = fresh_db().await;
        let t = insert(
            &db,
            NewModerationToken {
                kind: "report_challenge".into(),
                lookupId: "lk-rc".into(),
                secretHash: "hash".into(),
                boundUid: Some("reporter-uid".into()),
                caseId: None,
                expiresAt: Utc::now() + Duration::minutes(15),
            },
        )
        .await
        .unwrap();
        assert_eq!(t.boundUid.as_deref(), Some("reporter-uid"));
        assert!(t.caseId.is_none());
    }

    #[tokio::test]
    async fn consume_succeeds_once_then_is_single_use() {
        let db = fresh_db().await;
        insert(&db, appeal_token("lk-once", 7, Duration::days(30)))
            .await
            .unwrap();

        // First consume wins — returns the BEFORE image (usedAt still NONE).
        let first = consume(&db, "lk-once").await.unwrap().expect("first wins");
        assert_eq!(first.caseId, Some(7));
        assert!(
            first.usedAt.is_none(),
            "consume returns the BEFORE image — usedAt is NONE on it"
        );

        // The row is now marked used.
        let after = find_by_lookup_id(&db, "lk-once")
            .await
            .unwrap()
            .expect("row still present");
        assert!(after.usedAt.is_some(), "usedAt is set after consume");

        // Second consume of the same token matches zero rows.
        assert!(
            consume(&db, "lk-once").await.unwrap().is_none(),
            "single-use: a second consume returns None"
        );
    }

    #[tokio::test]
    async fn consume_rejects_expired_token() {
        let db = fresh_db().await;
        insert(&db, appeal_token("lk-exp", 1, Duration::seconds(-1)))
            .await
            .unwrap();
        assert!(
            consume(&db, "lk-exp").await.unwrap().is_none(),
            "an expired token cannot be consumed"
        );
        // ... and the row was not mutated — usedAt stays NONE.
        let row = find_by_lookup_id(&db, "lk-exp").await.unwrap().unwrap();
        assert!(row.usedAt.is_none(), "a failed consume burns nothing");
    }

    #[tokio::test]
    async fn consume_of_unknown_lookup_id_returns_none() {
        let db = fresh_db().await;
        assert!(consume(&db, "no-such-lookup").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_expired_sweeps_only_past_expiry() {
        let db = fresh_db().await;
        insert(&db, appeal_token("lk-live", 1, Duration::days(30)))
            .await
            .unwrap();
        insert(&db, appeal_token("lk-dead", 2, Duration::seconds(-10)))
            .await
            .unwrap();

        delete_expired(&db).await.unwrap();

        assert!(
            find_by_lookup_id(&db, "lk-dead").await.unwrap().is_none(),
            "expired token swept"
        );
        assert!(
            find_by_lookup_id(&db, "lk-live").await.unwrap().is_some(),
            "live token kept"
        );
    }

    #[tokio::test]
    async fn delete_removes_a_single_token() {
        let db = fresh_db().await;
        let t = insert(&db, appeal_token("lk-del", 1, Duration::days(1)))
            .await
            .unwrap();
        assert!(delete(&db, t.id).await.unwrap());
        assert!(find_by_id(&db, t.id).await.unwrap().is_none());
        assert!(
            !delete(&db, t.id).await.unwrap(),
            "second delete is a no-op"
        );
    }

    #[tokio::test]
    async fn list_for_case_scopes_to_case() {
        let db = fresh_db().await;
        insert(&db, appeal_token("lk-c1", 100, Duration::days(1)))
            .await
            .unwrap();
        insert(&db, appeal_token("lk-c2", 100, Duration::days(1)))
            .await
            .unwrap();
        insert(&db, appeal_token("lk-other", 200, Duration::days(1)))
            .await
            .unwrap();
        assert_eq!(list_for_case(&db, 100).await.unwrap().len(), 2);
        assert_eq!(list_for_case(&db, 200).await.unwrap().len(), 1);
        assert!(list_for_case(&db, 999).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_case_cascades_to_its_appeal_tokens() {
        let db = fresh_db().await;
        let case = moderation_cases::insert(
            &db,
            NewModerationCase {
                serverConfigId: 1,
                virtualServerId: 1,
                subjectUid: "subj".into(),
                subjectNicknameSnapshot: "Subj".into(),
                origin: "operator".into(),
                originRef: None,
                reason: "test".into(),
                openedByUserId: None,
            },
        )
        .await
        .unwrap();

        insert(&db, appeal_token("lk-cascade", case.id, Duration::days(30)))
            .await
            .unwrap();
        // A token bound to a different case must survive.
        insert(
            &db,
            appeal_token("lk-survivor", case.id + 999, Duration::days(30)),
        )
        .await
        .unwrap();

        // `moderation_cases` exposes no `delete` helper yet; delete the
        // record directly so the 0015 cascade event fires.
        db.query("DELETE type::record('moderation_case', $id);")
            .bind(("id", case.id))
            .await
            .unwrap()
            .check()
            .unwrap();

        assert!(
            find_by_lookup_id(&db, "lk-cascade")
                .await
                .unwrap()
                .is_none(),
            "the deleted case's appeal token is cascaded away"
        );
        assert!(
            find_by_lookup_id(&db, "lk-survivor")
                .await
                .unwrap()
                .is_some(),
            "a token bound to another case is untouched"
        );
    }
}
