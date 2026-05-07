//! `BotExecution` repo (spec §4.2.7).

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct BotExecution {
    pub id: i64,
    pub flowId: i64,
    pub triggeredBy: String,
    pub triggerData: Option<String>,
    pub status: String,
    pub startedAt: DateTime<Utc>,
    pub endedAt: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewBotExecution {
    pub flowId: i64,
    pub triggeredBy: String,
    pub triggerData: Option<String>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    flowId,
    triggeredBy,
    triggerData,
    status,
    startedAt,
    endedAt,
    error
";

pub async fn insert(db: &Database, new: NewBotExecution) -> Result<BotExecution> {
    let sql = format!(
        "CREATE type::record('bot_execution', sequence::nextval('bot_execution_id'))
            CONTENT {{
                flowId: $flowId,
                triggeredBy: $triggeredBy,
                triggerData: $triggerData
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("flowId", new.flowId))
        .bind(("triggeredBy", new.triggeredBy))
        .bind(("triggerData", new.triggerData))
        .await
        .context("bot_execution insert query failed")?
        .check()?;
    let row: Option<BotExecution> = resp.take(0)?;
    row.context("bot_execution insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<BotExecution>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('bot_execution', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_flow(db: &Database, flow_id: i64) -> Result<Vec<BotExecution>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_execution WHERE flowId = $fid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("fid", flow_id)).await?.check()?;
    Ok(resp.take(0)?)
}

/// Mark a run as `completed` / `failed` / `cancelled` and stamp `endedAt`.
/// `error` is set only for `failed`.
pub async fn finish(
    db: &Database,
    id: i64,
    status: &str,
    error: Option<String>,
) -> Result<Option<BotExecution>> {
    let sql = format!(
        "UPDATE type::record('bot_execution', $id) MERGE {{
            status: $status,
            endedAt: time::now(),
            error: $error
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("status", status.to_string()))
        .bind(("error", error))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('bot_execution', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
