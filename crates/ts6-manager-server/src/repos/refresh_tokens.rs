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
pub async fn set_replaced_by(
    db: &Database,
    old_token: &str,
    new_token: &str,
) -> Result<Option<RefreshToken>> {
    let sql = format!(
        "UPDATE refresh_token MERGE {{ replacedBy: $new }} WHERE token = $old
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

/// Sweep tokens whose `expiresAt < now`. Useful as a periodic cleanup task
/// later; not on the critical path for slice 1.
pub async fn delete_expired(db: &Database) -> Result<()> {
    let sql = "DELETE refresh_token WHERE expiresAt < time::now();";
    db.query(sql).await?.check()?;
    Ok(())
}
