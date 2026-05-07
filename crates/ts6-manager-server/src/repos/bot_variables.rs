//! `BotVariable` repo (spec §4.2.6).

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct BotVariable {
    pub id: i64,
    pub flowId: i64,
    pub name: String,
    pub value: String,
    pub scope: String,
}

#[derive(Debug, Clone)]
pub struct NewBotVariable {
    pub flowId: i64,
    pub name: String,
    pub value: String,
    pub scope: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    flowId,
    name,
    value,
    scope
";

pub async fn insert(db: &Database, new: NewBotVariable) -> Result<BotVariable> {
    let sql = format!(
        "CREATE type::record('bot_variable', sequence::nextval('bot_variable_id'))
            CONTENT {{
                flowId: $flowId,
                name: $name,
                value: $value,
                scope: $scope
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("flowId", new.flowId))
        .bind(("name", new.name))
        .bind(("value", new.value))
        .bind(("scope", new.scope))
        .await
        .context("bot_variable insert query failed")?
        .check()?;
    let row: Option<BotVariable> = resp.take(0)?;
    row.context("bot_variable insert returned no row")
}

pub async fn find_by_flow_name_scope(
    db: &Database,
    flow_id: i64,
    name: &str,
    scope: &str,
) -> Result<Option<BotVariable>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_variable
            WHERE flowId = $fid AND name = $name AND scope = $scope LIMIT 1;"
    );
    let mut resp = db
        .query(sql)
        .bind(("fid", flow_id))
        .bind(("name", name.to_string()))
        .bind(("scope", scope.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_flow(db: &Database, flow_id: i64) -> Result<Vec<BotVariable>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM bot_variable WHERE flowId = $fid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("fid", flow_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn set_value(db: &Database, id: i64, value: String) -> Result<Option<BotVariable>> {
    let sql = format!(
        "UPDATE type::record('bot_variable', $id) MERGE {{ value: $value }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("value", value))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('bot_variable', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}

/// §4.2.6: temp-scoped variables MUST be deleted at the end of each flow
/// execution; only flow-scoped variables persist across executions.
pub async fn delete_temp_for_flow(db: &Database, flow_id: i64) -> Result<()> {
    let sql = "DELETE bot_variable WHERE flowId = $fid AND scope = 'temp';";
    db.query(sql).bind(("fid", flow_id)).await?.check()?;
    Ok(())
}
