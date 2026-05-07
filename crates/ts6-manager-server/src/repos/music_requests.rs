//! `MusicRequest` repo (spec §4.2.16).
//!
//! Composite uniqueness on `(serverConfigId, url)` is enforced by the
//! migration's `music_request_server_url_unique` index — re-requesting
//! the same URL on the same server returns `Err` from `insert`. Callers
//! who want "remember-or-noop" semantics should use [`record`] instead.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MusicRequest {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub serverConfigId: i64,
    pub requestedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMusicRequest {
    pub title: String,
    pub url: String,
    pub serverConfigId: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    title,
    url,
    serverConfigId,
    requestedAt
";

pub async fn insert(db: &Database, new: NewMusicRequest) -> Result<MusicRequest> {
    let sql = format!(
        "CREATE type::record('music_request', sequence::nextval('music_request_id'))
            CONTENT {{
                title: $title,
                url: $url,
                serverConfigId: $serverConfigId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("title", new.title))
        .bind(("url", new.url))
        .bind(("serverConfigId", new.serverConfigId))
        .await
        .context("music_request insert query failed")?
        .check()?;
    let row: Option<MusicRequest> = resp.take(0)?;
    row.context("music_request insert returned no row")
}

/// Insert-or-fetch: respects the §4.2.16 dedup contract. If the row already
/// exists, return the existing one without bumping `requestedAt`.
pub async fn record(db: &Database, new: NewMusicRequest) -> Result<MusicRequest> {
    if let Some(existing) = find_by_server_url(db, new.serverConfigId, &new.url).await? {
        return Ok(existing);
    }
    insert(db, new).await
}

pub async fn find_by_server_url(
    db: &Database,
    server_config_id: i64,
    url: &str,
) -> Result<Option<MusicRequest>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM music_request
            WHERE serverConfigId = $sid AND url = $url LIMIT 1;"
    );
    let mut resp = db
        .query(sql)
        .bind(("sid", server_config_id))
        .bind(("url", url.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<MusicRequest>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM music_request
            WHERE serverConfigId = $sid ORDER BY requestedAt DESC;"
    );
    let mut resp = db.query(sql).bind(("sid", server_config_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('music_request', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
