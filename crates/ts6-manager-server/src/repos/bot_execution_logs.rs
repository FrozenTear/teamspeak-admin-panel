//! `BotExecutionLog` repo (spec §4.2.8).
//!
//! `executionId` is `Option<i64>` because §4.2.8 allows engine-level entries
//! that aren't attached to a specific execution. `serverConfigId`'s FK is
//! intentionally non-cascading per spec — see the migration's comment for
//! why deleting a server config doesn't sweep this table.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct BotExecutionLog {
    pub id: i64,
    pub executionId: Option<i64>,
    pub serverConfigId: i64,
    pub flowId: Option<i64>,
    pub nodeId: Option<String>,
    pub nodeName: Option<String>,
    pub level: String,
    pub message: String,
    pub data: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBotExecutionLog {
    pub executionId: Option<i64>,
    pub serverConfigId: i64,
    pub flowId: Option<i64>,
    pub nodeId: Option<String>,
    pub nodeName: Option<String>,
    pub level: String,
    pub message: String,
    pub data: Option<String>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    executionId,
    serverConfigId,
    flowId,
    nodeId,
    nodeName,
    level,
    message,
    data,
    timestamp
";

pub async fn insert(db: &Database, new: NewBotExecutionLog) -> Result<BotExecutionLog> {
    let sql = format!(
        "CREATE type::record('bot_execution_log', sequence::nextval('bot_execution_log_id'))
            CONTENT {{
                executionId: $executionId,
                serverConfigId: $serverConfigId,
                flowId: $flowId,
                nodeId: $nodeId,
                nodeName: $nodeName,
                level: $level,
                message: $message,
                data: $data
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("executionId", new.executionId))
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("flowId", new.flowId))
        .bind(("nodeId", new.nodeId))
        .bind(("nodeName", new.nodeName))
        .bind(("level", new.level))
        .bind(("message", new.message))
        .bind(("data", new.data))
        .await
        .context("bot_execution_log insert query failed")?
        .check()?;
    let row: Option<BotExecutionLog> = resp.take(0)?;
    row.context("bot_execution_log insert returned no row")
}

pub async fn list_for_execution(db: &Database, execution_id: i64) -> Result<Vec<BotExecutionLog>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_execution_log
            WHERE executionId = $eid ORDER BY timestamp ASC;"
    );
    let mut resp = db.query(sql).bind(("eid", execution_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_flow(db: &Database, flow_id: i64) -> Result<Vec<BotExecutionLog>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_execution_log
            WHERE flowId = $fid ORDER BY timestamp ASC;"
    );
    let mut resp = db.query(sql).bind(("fid", flow_id)).await?.check()?;
    Ok(resp.take(0)?)
}
