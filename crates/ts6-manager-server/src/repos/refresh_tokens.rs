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
use sha2::{Digest, Sha256};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// SHA-256 hex digest of a refresh-token bearer string.
///
/// The plaintext is what the client holds. Surreal stores only this digest
/// in `token` and `replacedBy`, so a database read cannot be replayed as a
/// live session and the admin sessions endpoint cannot return a successor.
pub fn token_at_rest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn is_stored_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

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
        .bind(("tok", token_at_rest(&new.token)))
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
        .bind(("tok", token_at_rest(token)))
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
        .bind(("tok", token_at_rest(successor_token)))
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
        .bind(("old", token_at_rest(old_token)))
        .bind(("new", token_at_rest(new_token)))
        .await?
        .check()?;
    let rows: Vec<RefreshToken> = resp.take(0)?;
    Ok(rows.into_iter().next())
}

/// Compare-and-swap the predecessor and insert the successor in one
/// SurrealDB transaction (L4). A revocation that lands between the two
/// writes used to leave the successor as an orphan live token.
///
/// `Ok(None)` means the CAS lost (the predecessor was already rotated or
/// deleted). Any other failure, including a rolled-back insert, is `Err`.
pub async fn commit_rotation(
    db: &Database,
    old_token: &str,
    new: NewRefreshToken,
) -> Result<Option<()>> {
    let sql = "
        BEGIN TRANSACTION;
        LET $updated = (UPDATE refresh_token MERGE { replacedBy: $newh }
            WHERE token = $oldh AND replacedBy IS NONE);
        IF array::len($updated) = 0 {
            THROW \"rotation-lost\";
        };
        CREATE type::record('refresh_token', sequence::nextval('refresh_token_id'))
            CONTENT {
                token: $newh,
                userId: $userId,
                expiresAt: $expiresAt,
                family: $family
            };
        COMMIT TRANSACTION;
    ";
    let result = db
        .query(sql)
        .bind(("oldh", token_at_rest(old_token)))
        .bind(("newh", token_at_rest(&new.token)))
        .bind(("userId", new.userId))
        .bind(("expiresAt", new.expiresAt))
        .bind(("family", new.family))
        .await;
    match result {
        Ok(resp) => match resp.check() {
            Ok(_) => Ok(Some(())),
            Err(err) if err.to_string().contains("rotation-lost") => Ok(None),
            Err(err) => Err(err).context("refresh_token rotation transaction failed"),
        },
        Err(err) if err.to_string().contains("rotation-lost") => Ok(None),
        Err(err) => Err(err).context("refresh_token rotation transaction failed"),
    }
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<()> {
    let sql = "DELETE refresh_token WHERE token = $tok;";
    db.query(sql)
        .bind(("tok", token_at_rest(token)))
        .await?
        .check()?;
    Ok(())
}

/// Delete one row by its integer id. Used when the caller already holds
/// the stored row (whose `token` column is a digest, not a bearer).
pub async fn delete_by_id(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('refresh_token', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}

/// One-shot upgrade: hash any `token` / `replacedBy` values that are still
/// plaintext. Digests (64 lowercase hex chars) are left alone. Safe to run
/// on every boot.
pub async fn rehash_plaintext_tokens(db: &Database) -> Result<u64> {
    #[allow(non_snake_case)]
    #[derive(Debug, Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct Row {
        id: i64,
        token: String,
        replacedBy: Option<String>,
    }
    let sql = "SELECT record::id(id) AS id, token, replacedBy FROM refresh_token;";
    let mut resp = db.query(sql).await?.check()?;
    let rows: Vec<Row> = resp.take(0)?;
    let mut n = 0u64;
    for row in rows {
        let token = if is_stored_digest(&row.token) {
            None
        } else {
            Some(token_at_rest(&row.token))
        };
        let replaced = match row.replacedBy.as_deref() {
            Some(value) if !is_stored_digest(value) => Some(token_at_rest(value)),
            _ => None,
        };
        if token.is_none() && replaced.is_none() {
            continue;
        }
        let mut q = String::from("UPDATE type::record('refresh_token', $id) MERGE {");
        if token.is_some() {
            q.push_str(" token: $tok");
        }
        if replaced.is_some() {
            if token.is_some() {
                q.push(',');
            }
            q.push_str(" replacedBy: $rep");
        }
        q.push_str(" };");
        let mut query = db.query(q).bind(("id", row.id));
        if let Some(tok) = token {
            query = query.bind(("tok", tok));
        }
        if let Some(rep) = replaced {
            query = query.bind(("rep", rep));
        }
        query.await?.check()?;
        n += 1;
    }
    Ok(n)
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
