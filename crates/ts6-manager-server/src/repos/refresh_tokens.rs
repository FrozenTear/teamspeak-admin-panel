//! `RefreshToken` repo (spec §4.2.2 + §6.5).
//!
//! Carries the columns SecurityEngineer needs for reuse-detection-by-family
//! ([PURA-4](/PURA/issues/PURA-4)): `family`, `replacedBy`, plus the bearer
//! `token` value, owning `userId`, and `expiresAt`. Cascade-on-user-delete
//! is wired in `0001_baseline.surql` so this repo doesn't need to chase
//! orphans manually.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct RefreshToken {
    pub id: i64,
    pub token: String,
    pub userId: i64,
    pub expiresAt: DateTime<Utc>,
    pub createdAt: DateTime<Utc>,
    pub family: Option<String>,
    pub replacedBy: Option<String>,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone)]
pub struct NewRefreshToken {
    pub token: String,
    pub userId: i64,
    pub expiresAt: DateTime<Utc>,
    pub family: Option<String>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    token,
    userId,
    expiresAt,
    createdAt,
    family,
    replacedBy
";

pub async fn insert(db: &Database, new: NewRefreshToken) -> Result<RefreshToken> {
    let sql = format!(
        "CREATE type::record('refresh_token', sequence::nextval('refresh_token_id'))
            CONTENT {{
                token: $tok,
                userId: $userId,
                expiresAt: $expiresAt,
                family: $family
            }}
            RETURN {PROJECTION};"
    );

    let mut resp = db
        .query(sql)
        // SurrealDB v3 reserves `$token` as an internal variable; use `$tok`
        // at the bind layer and reference it that way in the SurrealQL.
        .bind(("tok", new.token))
        .bind(("userId", new.userId))
        .bind(("expiresAt", new.expiresAt))
        .bind(("family", new.family))
        .await
        .context("refresh_token insert query failed")?
        .check()?;
    let row: Option<RefreshToken> = resp.take(0)?;
    row.context("refresh_token insert returned no row")
}

/// Look up by integer id. Used by the admin `/api/users/{id}/sessions/{sid}`
/// route to confirm a single session exists and resolve its `userId` +
/// `family` before deleting the family-wide cohort.
pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<RefreshToken>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('refresh_token', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<RefreshToken>> {
    let sql = format!("SELECT {PROJECTION} FROM refresh_token WHERE token = $tok LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

/// Spec §6.5.4 reuse signal — find the row whose `replacedBy` records the
/// supplied (already-rotated) token. Returning the row gives the caller
/// the `userId` they need for the family-wide revocation.
pub async fn find_predecessor_by_replaced_by(
    db: &Database,
    successor_token: &str,
) -> Result<Option<RefreshToken>> {
    let sql = format!("SELECT {PROJECTION} FROM refresh_token WHERE replacedBy = $tok LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("tok", successor_token.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_user(db: &Database, user_id: i64) -> Result<Vec<RefreshToken>> {
    let sql =
        format!("SELECT {PROJECTION} FROM refresh_token WHERE userId = $uid ORDER BY id ASC;");
    let mut resp = db.query(sql).bind(("uid", user_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_family(db: &Database, family: &str) -> Result<Vec<RefreshToken>> {
    let sql =
        format!("SELECT {PROJECTION} FROM refresh_token WHERE family = $fam ORDER BY id ASC;");
    let mut resp = db
        .query(sql)
        .bind(("fam", family.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

/// Set `replacedBy` for the row that owns `old_token`. Spec §6.5.3 step 4.
///
/// R5 (THE-1010): this is a **compare-and-swap** — the `replacedBy IS NONE`
/// guard means only the *first* rotation of a given token can stamp a
/// successor. A second concurrent rotation that already passed the
/// application-level `replacedBy.is_some()` check (because it read the row
/// before the first writer committed) finds 0 matching rows here and gets
/// `Ok(None)`. The caller MUST treat `None` as "lost the rotation race" and
/// refuse to insert a successor — otherwise two live tokens fork out of one,
/// and the loser's successor is a valid refresh token that never trips
/// reuse-detection. A single SurrealDB `UPDATE … WHERE` runs as one atomic
/// statement, so the guard is the authoritative gate even when the prior
/// read was stale.
pub async fn set_replaced_by(
    db: &Database,
    old_token: &str,
    new_token: &str,
) -> Result<Option<RefreshToken>> {
    let sql = format!(
        "UPDATE refresh_token MERGE {{ replacedBy: $new }}
            WHERE token = $old AND replacedBy IS NONE
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("old", old_token.to_string()))
        .bind(("new", new_token.to_string()))
        .await?
        .check()?;
    let rows: Vec<RefreshToken> = resp.take(0)?;
    Ok(rows.into_iter().next())
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<()> {
    let sql = "DELETE refresh_token WHERE token = $tok;";
    db.query(sql)
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Spec §6.5.4 — revoke every token for a user. Used both on confirmed
/// reuse and on password change (§6.2.3).
pub async fn delete_all_for_user(db: &Database, user_id: i64) -> Result<()> {
    let sql = "DELETE refresh_token WHERE userId = $uid;";
    db.query(sql).bind(("uid", user_id)).await?.check()?;
    Ok(())
}

/// PURA-235 — admin-driven family-wide session revoke per
/// `docs/admin/http-api.md` §3.3. Returns the row count deleted so the
/// caller can surface it in the audit payload.
pub async fn delete_by_family(db: &Database, family: &str) -> Result<u64> {
    let pre = list_for_family(db, family).await?;
    let n = pre.len() as u64;
    let sql = "DELETE refresh_token WHERE family = $fam;";
    db.query(sql)
        .bind(("fam", family.to_string()))
        .await?
        .check()?;
    Ok(n)
}

/// PURA-235 — admin-page `activeSessionCount` field per
/// `docs/admin/http-api.md` §2.1. Counts rows where `replacedBy IS NONE`
/// and `expiresAt > now` — i.e. the live successor of each family.
pub async fn count_active_for_user(db: &Database, user_id: i64) -> Result<i64> {
    let sql = "RETURN array::len(SELECT id FROM refresh_token
        WHERE userId = $uid AND replacedBy IS NONE AND expiresAt > time::now());";
    let mut resp = db.query(sql).bind(("uid", user_id)).await?.check()?;
    let n: Option<i64> = resp.take(0)?;
    Ok(n.unwrap_or(0))
}

/// Sweep tokens whose `expiresAt < now`. Useful as a periodic cleanup task
/// later; not on the critical path for slice 1.
pub async fn delete_expired(db: &Database) -> Result<()> {
    let sql = "DELETE refresh_token WHERE expiresAt < time::now();";
    db.query(sql).await?.check()?;
    Ok(())
}

/// PURA-226 — boot-time refresh-token volume snapshot.
///
/// Reports the total live (non-expired) refresh-token row count + the
/// number of distinct user ids those rows cover. Used by `run_serve` to
/// warn when the DB volume looks ephemeral: an enabled-users count > 0
/// paired with zero refresh-token rows means every operator who was
/// logged in before the restart will be bounced to `/login` on their
/// next request, which is one of the four PURA-225 candidate failure
/// modes.
///
/// SurrealDB v3 doesn't accept `count(distinct …)`, so distinct-counting
/// happens in Rust: the query yields one row per live token, the caller
/// dedupes via `HashSet`. The boot-time path runs once at startup and
/// the row count is bounded by the number of *concurrently signed-in*
/// operator sessions, which is far below the threshold where pulling
/// the userIds matters.
pub async fn boot_snapshot(db: &Database) -> Result<RefreshTokenBootSnapshot> {
    let sql = "SELECT userId FROM refresh_token WHERE expiresAt > time::now();";
    let mut resp = db.query(sql).await?.check()?;
    let rows: Vec<BootSnapshotRow> = resp.take(0)?;
    let total = rows.len() as u64;
    let distinct_users = rows
        .iter()
        .map(|r| r.userId)
        .collect::<std::collections::HashSet<_>>()
        .len() as u64;
    Ok(RefreshTokenBootSnapshot {
        total,
        distinct_users,
    })
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
struct BootSnapshotRow {
    userId: i64,
}

#[derive(Debug, Default, Clone)]
pub struct RefreshTokenBootSnapshot {
    pub total: u64,
    pub distinct_users: u64,
}

#[cfg(test)]
mod boot_snapshot_tests {
    //! PURA-226 — the boot-time snapshot powers the "DB volume looks
    //! ephemeral" warning in [`crate::server_entry::run_serve`]. Pin its
    //! shape so a query refactor that loses the `expiresAt > now()`
    //! filter or stops returning a row when the table is empty doesn't
    //! silently degrade the warning.

    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::users;
    use chrono::Duration;

    async fn make_user(db: &Database, username: &str) -> i64 {
        users::insert(
            db,
            users::NewUser {
                username: username.into(),
                passwordHash: "$argon2id$v=19$test".into(),
                displayName: username.into(),
                role: "viewer".into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn boot_snapshot_zero_on_empty_table() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let snap = boot_snapshot(&db).await.unwrap();
        assert_eq!(snap.total, 0);
        assert_eq!(snap.distinct_users, 0);
    }

    #[tokio::test]
    async fn boot_snapshot_counts_live_rows_not_expired() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let alice = make_user(&db, "alice").await;
        let bob = make_user(&db, "bob").await;

        // Two live tokens for alice, one expired for bob — distinct
        // user count must be 1 (alice), total must be 2 (live only).
        let now = chrono::Utc::now();
        insert(
            &db,
            NewRefreshToken {
                token: "alice-1".into(),
                userId: alice,
                expiresAt: now + Duration::days(1),
                family: Some("fam-a".into()),
            },
        )
        .await
        .unwrap();
        insert(
            &db,
            NewRefreshToken {
                token: "alice-2".into(),
                userId: alice,
                expiresAt: now + Duration::days(1),
                family: Some("fam-a2".into()),
            },
        )
        .await
        .unwrap();
        insert(
            &db,
            NewRefreshToken {
                token: "bob-expired".into(),
                userId: bob,
                expiresAt: now - Duration::seconds(1),
                family: Some("fam-b".into()),
            },
        )
        .await
        .unwrap();

        let snap = boot_snapshot(&db).await.unwrap();
        assert_eq!(snap.total, 2, "live row count");
        assert_eq!(snap.distinct_users, 1, "alice only — bob's row is expired");
    }
}
