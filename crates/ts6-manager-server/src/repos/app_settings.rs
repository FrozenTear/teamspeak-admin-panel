//! `AppSetting` repo (spec §4.2.9).
//!
//! String-keyed. The migration encodes the key in the SurrealDB record id
//! (`app_setting:<key>`), and stores `key` as a regular field so the
//! document round-trips through the JSON wire shape `{key, value, updatedAt}`
//! without aliasing. The migration also seeds `max_music_bots = "5"` per
//! §4.2.9 so an empty database still answers the operator-facing
//! "how many music bots can I run?" query.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct AppSetting {
    pub key: String,
    pub value: String,
    pub updatedAt: DateTime<Utc>,
}

const PROJECTION: &str = "
    key,
    value,
    updatedAt
";

pub async fn get(db: &Database, key: &str) -> Result<Option<AppSetting>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('app_setting', $key);");
    let mut resp = db
        .query(sql)
        .bind(("key", key.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<AppSetting>> {
    let sql = format!("SELECT {PROJECTION} FROM app_setting ORDER BY key ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// Upsert: insert if missing, update `value` if present. The trailing
/// `updatedAt` is bumped automatically by the field's VALUE expression.
pub async fn put(db: &Database, key: &str, value: &str) -> Result<AppSetting> {
    // UPSERT lands the row whether it existed or not. On insert we set both
    // `key` and `value`; on update the MERGE updates `value` only (the field
    // VALUE expression on `updatedAt` re-evaluates and stamps now()).
    let sql = format!(
        "UPSERT type::record('app_setting', $key)
            MERGE {{ key: $key, value: $value }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("key", key.to_string()))
        .bind(("value", value.to_string()))
        .await
        .context("app_setting upsert query failed")?
        .check()?;
    let row: Option<AppSetting> = resp.take(0)?;
    row.context("app_setting upsert returned no row")
}

pub async fn delete(db: &Database, key: &str) -> Result<()> {
    let sql = "DELETE type::record('app_setting', $key);";
    db.query(sql)
        .bind(("key", key.to_string()))
        .await?
        .check()?;
    Ok(())
}
