//! `BotFlow` repo (spec §4.2.5).

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct BotFlow {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub flowData: String,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub enabled: bool,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBotFlow {
    pub name: String,
    pub description: Option<String>,
    pub flowData: String,
    pub serverConfigId: i64,
    pub virtualServerId: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BotFlowUpdate {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub flowData: Option<String>,
    pub virtualServerId: Option<i64>,
    pub enabled: Option<bool>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    description,
    flowData,
    serverConfigId,
    virtualServerId,
    enabled,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewBotFlow) -> Result<BotFlow> {
    let sql = format!(
        "CREATE type::record('bot_flow', sequence::nextval('bot_flow_id'))
            CONTENT {{
                name: $name,
                description: $description,
                flowData: $flowData,
                serverConfigId: $serverConfigId,
                virtualServerId: $virtualServerId,
                enabled: $enabled
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("description", new.description))
        .bind(("flowData", new.flowData))
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("virtualServerId", new.virtualServerId))
        .bind(("enabled", new.enabled))
        .await
        .context("bot_flow insert query failed")?
        .check()?;
    let row: Option<BotFlow> = resp.take(0)?;
    row.context("bot_flow insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<BotFlow>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('bot_flow', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<BotFlow>> {
    let sql = format!("SELECT {PROJECTION} FROM bot_flow ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<BotFlow>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_flow WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("sid", server_config_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn update(db: &Database, id: i64, patch: BotFlowUpdate) -> Result<Option<BotFlow>> {
    let mut merge = serde_json::Map::new();
    if let Some(v) = patch.name {
        merge.insert("name".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.description {
        merge.insert(
            "description".into(),
            v.map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(v) = patch.flowData {
        merge.insert("flowData".into(), serde_json::Value::String(v));
    }
    if let Some(v) = patch.virtualServerId {
        merge.insert("virtualServerId".into(), serde_json::Value::Number(v.into()));
    }
    if let Some(v) = patch.enabled {
        merge.insert("enabled".into(), serde_json::Value::Bool(v));
    }
    if merge.is_empty() {
        return find_by_id(db, id).await;
    }
    let sql = format!(
        "UPDATE type::record('bot_flow', $id) MERGE $patch RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("patch", serde_json::Value::Object(merge)))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('bot_flow', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
