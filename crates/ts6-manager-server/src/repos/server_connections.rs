//! `TsServerConfig` repo (spec §4.2.4).
//!
//! Mapped to the `server_connection` table. The on-disk representation
//! stores `apiKey` and `sshPassword` as ciphertext (`enc:<iv>:<tag>:<ct>`,
//! spec §6.3.2); the repo treats them as opaque strings — encryption /
//! decryption sit in `crate::crypto` and are applied by the REST handlers
//! that read or write these fields.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ServerConnection {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub webqueryPort: i64,
    pub apiKey: String,
    pub useHttps: bool,
    pub sshPort: i64,
    pub sshUsername: Option<String>,
    pub sshPassword: Option<String>,
    pub queryBotChannel: Option<String>,
    pub queryBotNickname: Option<String>,
    pub sshBotNickname: Option<String>,
    pub enabled: bool,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone)]
pub struct NewServerConnection {
    pub name: String,
    pub host: String,
    pub webqueryPort: i64,
    pub apiKey: String,
    pub useHttps: bool,
    pub sshPort: i64,
    pub sshUsername: Option<String>,
    pub sshPassword: Option<String>,
    pub queryBotChannel: Option<String>,
    pub queryBotNickname: Option<String>,
    pub sshBotNickname: Option<String>,
    pub enabled: bool,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    host,
    webqueryPort,
    apiKey,
    useHttps,
    sshPort,
    sshUsername,
    sshPassword,
    queryBotChannel,
    queryBotNickname,
    sshBotNickname,
    enabled,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewServerConnection) -> Result<ServerConnection> {
    let sql = format!(
        "CREATE type::record('server_connection', sequence::nextval('server_connection_id'))
            CONTENT {{
                name: $name,
                host: $host,
                webqueryPort: $webqueryPort,
                apiKey: $apiKey,
                useHttps: $useHttps,
                sshPort: $sshPort,
                sshUsername: $sshUsername,
                sshPassword: $sshPassword,
                queryBotChannel: $queryBotChannel,
                queryBotNickname: $queryBotNickname,
                sshBotNickname: $sshBotNickname,
                enabled: $enabled
            }}
            RETURN {PROJECTION};"
    );

    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("host", new.host))
        .bind(("webqueryPort", new.webqueryPort))
        .bind(("apiKey", new.apiKey))
        .bind(("useHttps", new.useHttps))
        .bind(("sshPort", new.sshPort))
        .bind(("sshUsername", new.sshUsername))
        .bind(("sshPassword", new.sshPassword))
        .bind(("queryBotChannel", new.queryBotChannel))
        .bind(("queryBotNickname", new.queryBotNickname))
        .bind(("sshBotNickname", new.sshBotNickname))
        .bind(("enabled", new.enabled))
        .await
        .context("server_connection insert query failed")?
        .check()?;
    let row: Option<ServerConnection> = resp.take(0)?;
    row.context("server_connection insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ServerConnection>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM type::record('server_connection', $id);"
    );
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<ServerConnection>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM server_connection ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// Delete a server connection. The `server_connection_cascade` event in
/// 0001_baseline.surql wipes dependent `server_user_grant` rows for this
/// connection (per spec §4.2 cascade rules).
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('server_connection', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
