//! `Widget` repo (spec §4.2.15).
//!
//! `token` is the only credential a viewer needs to render the widget — it
//! must be a URL-safe random string with enough entropy to be unguessable.
//! The repo enforces uniqueness via the `widget_token_unique` index.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct Widget {
    pub id: i64,
    pub name: String,
    pub token: String,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub theme: String,
    pub showChannelTree: bool,
    pub showClients: bool,
    pub hideEmptyChannels: bool,
    pub maxChannelDepth: i64,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewWidget {
    pub name: String,
    pub token: String,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub theme: String,
    pub showChannelTree: bool,
    pub showClients: bool,
    pub hideEmptyChannels: bool,
    pub maxChannelDepth: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    token,
    serverConfigId,
    virtualServerId,
    theme,
    showChannelTree,
    showClients,
    hideEmptyChannels,
    maxChannelDepth,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewWidget) -> Result<Widget> {
    // SurrealDB v3 reserves `$token` as an internal variable, so we bind
    // the URL-safe widget token under `$tok` (same idiom as
    // refresh_tokens::insert).
    let sql = format!(
        "CREATE type::record('widget', sequence::nextval('widget_id'))
            CONTENT {{
                name: $name,
                token: $tok,
                serverConfigId: $serverConfigId,
                virtualServerId: $virtualServerId,
                theme: $theme,
                showChannelTree: $showChannelTree,
                showClients: $showClients,
                hideEmptyChannels: $hideEmptyChannels,
                maxChannelDepth: $maxChannelDepth
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("tok", new.token))
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("virtualServerId", new.virtualServerId))
        .bind(("theme", new.theme))
        .bind(("showChannelTree", new.showChannelTree))
        .bind(("showClients", new.showClients))
        .bind(("hideEmptyChannels", new.hideEmptyChannels))
        .bind(("maxChannelDepth", new.maxChannelDepth))
        .await
        .context("widget insert query failed")?
        .check()?;
    let row: Option<Widget> = resp.take(0)?;
    row.context("widget insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<Widget>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('widget', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<Widget>> {
    let sql = format!("SELECT {PROJECTION} FROM widget WHERE token = $tok LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<Widget>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM widget WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("sid", server_config_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<Widget>> {
    let sql = format!("SELECT {PROJECTION} FROM widget ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// Patch shape for [`update`]. Every field is optional; absent fields are
/// preserved by SurrealDB's `MERGE` semantics. `serverConfigId` and
/// `virtualServerId` are intentionally *not* patchable — re-pointing a
/// widget at a different upstream server is a new widget; recreate it.
#[derive(Debug, Clone, Default)]
pub struct WidgetUpdate {
    pub name: Option<String>,
    pub theme: Option<String>,
    pub showChannelTree: Option<bool>,
    pub showClients: Option<bool>,
    pub hideEmptyChannels: Option<bool>,
    pub maxChannelDepth: Option<i64>,
}

pub async fn update(db: &Database, id: i64, patch: WidgetUpdate) -> Result<Option<Widget>> {
    let mut merge = serde_json::Map::new();
    if let Some(v) = patch.name {
        merge.insert("name".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.theme {
        merge.insert("theme".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.showChannelTree {
        merge.insert("showChannelTree".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = patch.showClients {
        merge.insert("showClients".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = patch.hideEmptyChannels {
        merge.insert("hideEmptyChannels".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = patch.maxChannelDepth {
        merge.insert("maxChannelDepth".into(), serde_json::Value::Number(v.into()));
    }
    if merge.is_empty() {
        return find_by_id(db, id).await;
    }
    let sql = format!(
        "UPDATE type::record('widget', $id) MERGE $patch RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("patch", serde_json::Value::Object(merge)))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

/// Replace the widget's URL token. Used by `POST /api/widgets/{id}/regenerate-token`
/// (spec §7.27). The repo doesn't invalidate the public-data cache — the
/// route handler does that under the *old* token before calling `set_token`.
pub async fn set_token(db: &Database, id: i64, new_token: &str) -> Result<Option<Widget>> {
    let sql = format!(
        "UPDATE type::record('widget', $id) MERGE {{ token: $tok }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("tok", new_token.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('widget', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
