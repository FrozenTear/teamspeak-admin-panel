//! `UserServerAccess` repo (spec §4.2.3) — mapped to `server_user_grant`.
//!
//! Composite uniqueness on `(userId, serverConfigId)` is enforced by the
//! `server_user_grant_unique` index in `0001_baseline.surql`. Cascading
//! delete from either side is handled by the events in that migration.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ServerUserGrant {
    pub id: i64,
    pub userId: i64,
    pub serverConfigId: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    userId,
    serverConfigId
";

pub async fn insert(db: &Database, user_id: i64, server_config_id: i64) -> Result<ServerUserGrant> {
    let sql = format!(
        "CREATE type::record('server_user_grant', sequence::nextval('server_user_grant_id'))
            CONTENT {{ userId: $userId, serverConfigId: $serverConfigId }}
            RETURN {PROJECTION};"
    );

    let mut resp = db
        .query(sql)
        .bind(("userId", user_id))
        .bind(("serverConfigId", server_config_id))
        .await
        .context("server_user_grant insert query failed")?
        .check()?;
    let row: Option<ServerUserGrant> = resp.take(0)?;
    row.context("server_user_grant insert returned no row")
}

pub async fn exists(db: &Database, user_id: i64, server_config_id: i64) -> Result<bool> {
    // SELECT VALUE returns a flat list, and `record::id(id)` extracts the
    // integer record-id portion as `i64` so we don't need to drag in a
    // SurrealValue impl for `RecordId` just to count.
    let sql = "SELECT VALUE record::id(id) FROM server_user_grant
                WHERE userId = $uid AND serverConfigId = $sid LIMIT 1;";
    let mut resp = db
        .query(sql)
        .bind(("uid", user_id))
        .bind(("sid", server_config_id))
        .await?
        .check()?;
    let rows: Vec<i64> = resp.take(0)?;
    Ok(!rows.is_empty())
}

pub async fn list_for_user(db: &Database, user_id: i64) -> Result<Vec<ServerUserGrant>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM server_user_grant WHERE userId = $uid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("uid", user_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<ServerUserGrant>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM server_user_grant
            WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("sid", server_config_id))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, user_id: i64, server_config_id: i64) -> Result<()> {
    let sql = "DELETE server_user_grant
                WHERE userId = $uid AND serverConfigId = $sid;";
    db.query(sql)
        .bind(("uid", user_id))
        .bind(("sid", server_config_id))
        .await?
        .check()?;
    Ok(())
}
