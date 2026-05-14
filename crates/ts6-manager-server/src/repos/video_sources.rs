//! `VideoSource` repo (PURA-144 WS-6).
//!
//! The operator-facing catalogue of video sources currently published
//! through `ts6-media-sidecar`. The row is the manager's persistent
//! record of intent; the sidecar owns the live FFmpeg pipeline keyed by
//! `sourceId` (which doubles as the moq-lite namespace, see the
//! [`ts6_media_sidecar::control::TrackDescriptor`] companion type).
//!
//! Status field semantics (mirrored by the polling task in
//! [`crate::ws::video_source_tick`]):
//! - `starting` — `POST /source` succeeded, sidecar `/stats` has not yet
//!   reported `ffmpeg_alive: true` for any track.
//! - `live`     — at least one track is ffmpeg-alive.
//! - `failed`   — sidecar previously reported `ffmpeg_alive: true` but
//!   then dropped to false on both tracks (FFmpeg died or upstream cut).
//! - `stopped`  — `POST /source/stop` returned and the row will be
//!   deleted by [`delete_by_id`]; reserved for the brief window before
//!   the row is gone.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct VideoSource {
    pub id: i64,
    pub sourceId: String,
    pub label: String,
    pub url: String,
    pub preset: String,
    pub serverConfigId: i64,
    pub createdByUserId: Option<i64>,
    pub status: String,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewVideoSource {
    pub sourceId: String,
    pub label: String,
    pub url: String,
    pub preset: String,
    pub serverConfigId: i64,
    pub createdByUserId: Option<i64>,
    pub status: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    sourceId,
    label,
    url,
    preset,
    serverConfigId,
    createdByUserId,
    status,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewVideoSource) -> Result<VideoSource> {
    let sql = format!(
        "CREATE type::record('video_source', sequence::nextval('video_source_id'))
            CONTENT {{
                sourceId: $sourceId,
                label: $label,
                url: $url,
                preset: $preset,
                serverConfigId: $serverConfigId,
                createdByUserId: $createdByUserId,
                status: $status
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("sourceId", new.sourceId))
        .bind(("label", new.label))
        .bind(("url", new.url))
        .bind(("preset", new.preset))
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("createdByUserId", new.createdByUserId))
        .bind(("status", new.status))
        .await
        .context("video_source insert query failed")?
        .check()?;
    let row: Option<VideoSource> = resp.take(0)?;
    row.context("video_source insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<VideoSource>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('video_source', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn find_by_source_id(db: &Database, source_id: &str) -> Result<Option<VideoSource>> {
    let sql = format!("SELECT {PROJECTION} FROM video_source WHERE sourceId = $sid LIMIT 1;");
    let mut resp = db
        .query(sql)
        .bind(("sid", source_id.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<VideoSource>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM video_source WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("sid", server_config_id))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn list_all(db: &Database) -> Result<Vec<VideoSource>> {
    let sql = format!("SELECT {PROJECTION} FROM video_source ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

/// Update the live status string. Called by the polling task in
/// [`crate::ws::video_source_tick`] each time it observes a transition.
/// Returns the updated row (or `None` if it was deleted concurrently).
pub async fn update_status(db: &Database, id: i64, status: &str) -> Result<Option<VideoSource>> {
    let sql = format!(
        "UPDATE type::record('video_source', $id) MERGE {{ status: $status }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("status", status.to_string()))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete_by_id(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('video_source', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
