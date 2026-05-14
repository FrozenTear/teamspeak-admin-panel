//! `User` repo (spec §4.2.1, §6.5).
//!
//! Field names mirror Chapter 4 verbatim — they appear in JSON wire types
//! per Chapter 7 and the wire shape MUST match the document shape so REST
//! handlers can serialise rows through `serde_json` without renames.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct User {
    pub id: i64,
    pub username: String,
    pub passwordHash: String,
    pub displayName: String,
    pub role: String,
    pub enabled: bool,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
    pub lastLoginAt: Option<DateTime<Utc>>,
}

/// Fields supplied by the caller on insert. `id`, `createdAt`, and
/// `updatedAt` are server-managed; `lastLoginAt` starts null and is bumped
/// by `mark_login`.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub username: String,
    pub passwordHash: String,
    pub displayName: String,
    pub role: String,
    pub enabled: bool,
}

/// Mutable subset of `User` the API surface can update directly. Password
/// changes go through `set_password_hash` so the call site is explicit.
#[derive(Debug, Clone, Default)]
pub struct UserUpdate {
    pub displayName: Option<String>,
    pub role: Option<String>,
    pub enabled: Option<bool>,
}

/// Field list used both inside `SELECT … FROM …` and inside `RETURN …`
/// after CREATE/UPDATE — SurrealQL accepts the same `expr AS alias` form
/// in both contexts (`Output::Fields` in surrealdb-core).
const PROJECTION: &str = "
    record::id(id) AS id,
    username,
    passwordHash,
    displayName,
    role,
    enabled,
    createdAt,
    updatedAt,
    lastLoginAt
";

pub async fn insert(db: &Database, new: NewUser) -> Result<User> {
    let sql = format!(
        "CREATE type::record('user', sequence::nextval('user_id'))
            CONTENT {{
                username: $username,
                passwordHash: $passwordHash,
                displayName: $displayName,
                role: $role,
                enabled: $enabled
            }}
            RETURN {PROJECTION};"
    );

    let mut resp = db
        .query(sql)
        .bind(("username", new.username))
        .bind(("passwordHash", new.passwordHash))
        .bind(("displayName", new.displayName))
        .bind(("role", new.role))
        .bind(("enabled", new.enabled))
        .await
        .context("user insert query failed")?
        .check()
        .context("user insert reported an error")?;
    let row: Option<User> = resp.take(0).context("user insert deserialise failed")?;
    row.context("user insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<User>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('user', $id);");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .await
        .context("user find_by_id query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<User>> {
    let sql = format!("SELECT {PROJECTION} FROM user WHERE username = $username LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("username", username.to_string()))
        .await
        .context("user find_by_username query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<User>> {
    let sql = format!("SELECT {PROJECTION} FROM user ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// Total number of `user` rows. Used by `/api/setup/status` (spec §7.2 —
/// `needsSetup == (user_count == 0)`) and the `/api/setup/init` re-check
/// guard. `array::len` over the row set sidesteps SurrealDB-version
/// quirks around `count()` aggregation typing — the response is a single
/// integer wrapped in `Option`.
pub async fn count(db: &Database) -> Result<i64> {
    let mut resp = db
        .query("RETURN array::len(SELECT id FROM user);")
        .await
        .context("user count query failed")?
        .check()?;
    let n: Option<i64> = resp.take(0).context("user count: deserialise failed")?;
    Ok(n.unwrap_or(0))
}

pub async fn update(db: &Database, id: i64, patch: UserUpdate) -> Result<Option<User>> {
    // SurrealDB MERGE preserves keys not present in the patch payload, so
    // we build a sparse payload from the optional fields.
    let mut merge = serde_json::Map::new();
    if let Some(v) = patch.displayName {
        merge.insert("displayName".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.role {
        merge.insert("role".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.enabled {
        merge.insert("enabled".into(), serde_json::Value::Bool(v));
    }

    if merge.is_empty() {
        return find_by_id(db, id).await;
    }

    let sql = format!("UPDATE type::record('user', $id) MERGE $patch RETURN {PROJECTION};");
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("patch", serde_json::Value::Object(merge)))
        .await
        .context("user update query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn set_password_hash(db: &Database, id: i64, password_hash: String) -> Result<()> {
    let sql = "UPDATE type::record('user', $id) MERGE { passwordHash: $hash };";
    db.query(sql)
        .bind(("id", id))
        .bind(("hash", password_hash))
        .await
        .context("set_password_hash query failed")?
        .check()?;
    Ok(())
}

pub async fn mark_login(db: &Database, id: i64) -> Result<()> {
    let sql = "UPDATE type::record('user', $id) MERGE { lastLoginAt: time::now() };";
    db.query(sql)
        .bind(("id", id))
        .await
        .context("mark_login query failed")?
        .check()?;
    Ok(())
}

/// Delete a user. The `user_cascade` event in 0001_baseline.surql wipes
/// dependent `refresh_token` and `server_user_grant` rows for this user
/// (per spec §4.2 cascade rules), covering the R5 cleanup half.
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('user', $id);";
    db.query(sql)
        .bind(("id", id))
        .await
        .context("user delete query failed")?
        .check()?;
    Ok(())
}
