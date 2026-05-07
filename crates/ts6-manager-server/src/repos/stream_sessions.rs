//! `StreamSession` repo (spec §4.2.17).
//!
//! Informational history. `musicBotId` is not a formal FK per spec, so
//! deleting a `MusicBot` does *not* sweep these rows; that's intentional —
//! historical sessions outlive the bot config.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct StreamSession {
    pub id: i64,
    pub musicBotId: i64,
    pub source: String,
    pub preset: String,
    pub startedAt: DateTime<Utc>,
    pub endedAt: Option<DateTime<Utc>>,
    pub peakViewers: i64,
}

#[derive(Debug, Clone)]
pub struct NewStreamSession {
    pub musicBotId: i64,
    pub source: String,
    pub preset: String,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    musicBotId,
    source,
    preset,
    startedAt,
    endedAt,
    peakViewers
";

pub async fn insert(db: &Database, new: NewStreamSession) -> Result<StreamSession> {
    let sql = format!(
        "CREATE type::record('stream_session', sequence::nextval('stream_session_id'))
            CONTENT {{
                musicBotId: $musicBotId,
                source: $source,
                preset: $preset
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("musicBotId", new.musicBotId))
        .bind(("source", new.source))
        .bind(("preset", new.preset))
        .await
        .context("stream_session insert query failed")?
        .check()?;
    let row: Option<StreamSession> = resp.take(0)?;
    row.context("stream_session insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<StreamSession>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('stream_session', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_music_bot(db: &Database, music_bot_id: i64) -> Result<Vec<StreamSession>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM stream_session
            WHERE musicBotId = $mid ORDER BY startedAt DESC;"
    );
    let mut resp = db.query(sql).bind(("mid", music_bot_id)).await?.check()?;
    Ok(resp.take(0)?)
}

/// Stamp the closing fields. `endedAt` is set to now(); `peakViewers`
/// records the high-water mark observed during the session.
pub async fn finish(
    db: &Database,
    id: i64,
    peak_viewers: i64,
) -> Result<Option<StreamSession>> {
    let sql = format!(
        "UPDATE type::record('stream_session', $id) MERGE {{
            endedAt: time::now(),
            peakViewers: $peakViewers
        }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("peakViewers", peak_viewers))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('stream_session', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
