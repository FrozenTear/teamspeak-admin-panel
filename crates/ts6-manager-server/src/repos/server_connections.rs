//! `TsServerConfig` repo (spec §4.2.4) — plus the D-SSH-AUTH deviation
//! columns added in migration `0005_ssh_bridge_auth.surql` (PURA-77).
//!
//! Mapped to the `server_connection` table. The on-disk representation
//! stores `apiKey`, `sshPassword`, and `sshPrivateKey` as ciphertext
//! (`enc:<iv>:<tag>:<ct>`, spec §6.3.2); the repo treats them as opaque
//! strings — encryption / decryption sit in `crate::crypto` and are applied
//! by the REST handlers (or, for the future SSH transport, by the russh
//! consumer) that read or write these fields.
//!
//! `controlPath` and `sshAuthMethod` are stored as `string` (not Rust enums)
//! so adding new variants does not require a schema migration. Validation
//! lives at the wire boundary (the REST handler that admits writes); the
//! repo trusts what it persists.

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
    // D-SSH-AUTH (PURA-77) — see study-documents/ts6-manager-impl-deviations.md
    pub controlPath: String,
    pub sshAuthMethod: String,
    pub sshPrivateKey: Option<String>,
    pub sshKeyAgentSocket: Option<String>,
    pub sshHostKeyFingerprint: Option<String>,
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
    // D-SSH-AUTH (PURA-77). `None` lets the migration-side DEFAULT take
    // over for `controlPath` and `sshAuthMethod`, which is the right
    // behaviour for the existing `POST /api/servers` handler that has not
    // yet been taught about these fields (PURA-69 follow-up C scope).
    pub controlPath: Option<String>,
    pub sshAuthMethod: Option<String>,
    pub sshPrivateKey: Option<String>,
    pub sshKeyAgentSocket: Option<String>,
    pub sshHostKeyFingerprint: Option<String>,
}

pub(crate) const PROJECTION: &str = "
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
    updatedAt,
    controlPath,
    sshAuthMethod,
    sshPrivateKey,
    sshKeyAgentSocket,
    sshHostKeyFingerprint
";

// Migration `0005_ssh_bridge_auth.surql` defines these as the canonical
// defaults. Mirroring them in the repo lets the existing `POST /api/servers`
// handler — which predates the SSHBridge follow-ups — keep building
// `NewServerConnection` without setting the new fields and still produce
// rows consistent with the migration's `DEFAULT` clause.
pub const DEFAULT_CONTROL_PATH: &str = "webquery";
pub const DEFAULT_SSH_AUTH_METHOD: &str = "password";

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
                enabled: $enabled,
                controlPath: $controlPath,
                sshAuthMethod: $sshAuthMethod,
                sshPrivateKey: $sshPrivateKey,
                sshKeyAgentSocket: $sshKeyAgentSocket,
                sshHostKeyFingerprint: $sshHostKeyFingerprint
            }}
            RETURN {PROJECTION};"
    );

    // `controlPath` and `sshAuthMethod` are non-option strings on the schema
    // side — passing `null` would fail validation. `unwrap_or_else` here gives
    // us the same effective behaviour as omitting the field and letting the
    // schema `DEFAULT` clause fire, but with the binder always sending a
    // concrete value so the SQL stays uniform.
    let control_path = new
        .controlPath
        .unwrap_or_else(|| DEFAULT_CONTROL_PATH.to_string());
    let ssh_auth_method = new
        .sshAuthMethod
        .unwrap_or_else(|| DEFAULT_SSH_AUTH_METHOD.to_string());

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
        .bind(("controlPath", control_path))
        .bind(("sshAuthMethod", ssh_auth_method))
        .bind(("sshPrivateKey", new.sshPrivateKey))
        .bind(("sshKeyAgentSocket", new.sshKeyAgentSocket))
        .bind(("sshHostKeyFingerprint", new.sshHostKeyFingerprint))
        .await
        .context("server_connection insert query failed")?
        .check()?;
    let row: Option<ServerConnection> = resp.take(0)?;
    row.context("server_connection insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<ServerConnection>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('server_connection', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<ServerConnection>> {
    let sql = format!("SELECT {PROJECTION} FROM server_connection ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// List server connections the user has been granted access to via
/// `server_user_grant` (spec §6.6 / §7.5). Used by `GET /api/servers`
/// for non-admin callers — admins see the full [`list`] above.
///
/// The inner subquery returns the integer `serverConfigId` set; the outer
/// projection compares against `record::id(id)` so the join works against
/// SurrealDB's record-id encoding rather than the typed `RecordId`.
pub async fn list_for_user(db: &Database, user_id: i64) -> Result<Vec<ServerConnection>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM server_connection
            WHERE record::id(id) IN (
                SELECT VALUE serverConfigId FROM server_user_grant WHERE userId = $uid
            )
            ORDER BY id ASC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("uid", user_id))
        .await
        .context("server_connection list_for_user query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Partial update for PATCH /api/servers/:id. Only fields wrapped in `Some`
/// are written; `None` fields leave the existing DB value unchanged.
/// Callers must pre-seal `api_key` and `ssh_password` before calling this.
/// Double-option pattern for clearable nullable fields:
/// - `None` → don't touch this field
/// - `Some(None)` → set to NULL in DB
/// - `Some(Some(v))` → set to `v`
type Nullable<T> = Option<Option<T>>;

pub struct PatchServerConnection {
    pub name: Option<String>,
    pub host: Option<String>,
    pub webquery_port: Option<i64>,
    /// Pre-sealed ciphertext if the caller is updating the key; `None` to preserve.
    pub api_key: Option<String>,
    pub use_https: Option<bool>,
    pub ssh_port: Option<i64>,
    pub ssh_username: Nullable<String>,
    /// Pre-sealed ciphertext, or `Some(None)` to clear, or `None` to preserve.
    pub ssh_password: Nullable<String>,
    pub control_path: Option<String>,
    pub ssh_auth_method: Option<String>,
    pub ssh_host_key_fingerprint: Nullable<String>,
}

pub async fn patch(
    db: &Database,
    id: i64,
    p: PatchServerConnection,
) -> Result<Option<ServerConnection>> {
    let mut parts: Vec<&str> = Vec::new();
    if p.name.is_some() {
        parts.push("name = $name");
    }
    if p.host.is_some() {
        parts.push("host = $host");
    }
    if p.webquery_port.is_some() {
        parts.push("webqueryPort = $webqueryPort");
    }
    if p.api_key.is_some() {
        parts.push("apiKey = $apiKey");
    }
    if p.use_https.is_some() {
        parts.push("useHttps = $useHttps");
    }
    if p.ssh_port.is_some() {
        parts.push("sshPort = $sshPort");
    }
    if p.ssh_username.is_some() {
        parts.push("sshUsername = $sshUsername");
    }
    if p.ssh_password.is_some() {
        parts.push("sshPassword = $sshPassword");
    }
    if p.control_path.is_some() {
        parts.push("controlPath = $controlPath");
    }
    if p.ssh_auth_method.is_some() {
        parts.push("sshAuthMethod = $sshAuthMethod");
    }
    if p.ssh_host_key_fingerprint.is_some() {
        parts.push("sshHostKeyFingerprint = $sshHostKeyFingerprint");
    }

    if parts.is_empty() {
        return find_by_id(db, id).await;
    }

    parts.push("updatedAt = time::now()");
    let set_clause = parts.join(", ");
    let sql = format!(
        "UPDATE type::record('server_connection', $id) SET {set_clause} RETURN {PROJECTION};"
    );

    let mut q = db.query(sql).bind(("id", id));
    if let Some(v) = p.name {
        q = q.bind(("name", v));
    }
    if let Some(v) = p.host {
        q = q.bind(("host", v));
    }
    if let Some(v) = p.webquery_port {
        q = q.bind(("webqueryPort", v));
    }
    if let Some(v) = p.api_key {
        q = q.bind(("apiKey", v));
    }
    if let Some(v) = p.use_https {
        q = q.bind(("useHttps", v));
    }
    if let Some(v) = p.ssh_port {
        q = q.bind(("sshPort", v));
    }
    if let Some(v) = p.ssh_username {
        q = q.bind(("sshUsername", v));
    }
    if let Some(v) = p.ssh_password {
        q = q.bind(("sshPassword", v));
    }
    if let Some(v) = p.control_path {
        q = q.bind(("controlPath", v));
    }
    if let Some(v) = p.ssh_auth_method {
        q = q.bind(("sshAuthMethod", v));
    }
    if let Some(v) = p.ssh_host_key_fingerprint {
        q = q.bind(("sshHostKeyFingerprint", v));
    }

    let mut resp = q
        .await
        .context("server_connection patch query failed")?
        .check()?;
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
