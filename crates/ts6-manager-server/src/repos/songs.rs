//! `Song` repo (spec §4.2.11).
//!
//! `filePath` is rooted at `MUSIC_DIR` (§5.1). The path-traversal guard is
//! the REST handler's responsibility — the repo treats `filePath` as a
//! plain string and stores it as-is.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct Song {
    pub id: i64,
    pub title: String,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    pub filePath: String,
    pub source: String,
    pub sourceUrl: Option<String>,
    pub fileSize: Option<i64>,
    pub serverConfigId: i64,
    pub createdAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSong {
    pub title: String,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    pub filePath: String,
    pub source: String,
    pub sourceUrl: Option<String>,
    pub fileSize: Option<i64>,
    pub serverConfigId: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    title,
    artist,
    duration,
    filePath,
    source,
    sourceUrl,
    fileSize,
    serverConfigId,
    createdAt
";

pub async fn insert(db: &Database, new: NewSong) -> Result<Song> {
    let sql = format!(
        "CREATE type::record('song', sequence::nextval('song_id'))
            CONTENT {{
                title: $title,
                artist: $artist,
                duration: $duration,
                filePath: $filePath,
                source: $source,
                sourceUrl: $sourceUrl,
                fileSize: $fileSize,
                serverConfigId: $serverConfigId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("title", new.title))
        .bind(("artist", new.artist))
        .bind(("duration", new.duration))
        .bind(("filePath", new.filePath))
        .bind(("source", new.source))
        .bind(("sourceUrl", new.sourceUrl))
        .bind(("fileSize", new.fileSize))
        .bind(("serverConfigId", new.serverConfigId))
        .await
        .context("song insert query failed")?
        .check()?;
    let row: Option<Song> = resp.take(0)?;
    row.context("song insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<Song>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('song', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<Song>> {
    let sql = format!("SELECT {PROJECTION} FROM song ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<Song>> {
    let sql = format!("SELECT {PROJECTION} FROM song WHERE serverConfigId = $sid ORDER BY id ASC;");
    let mut resp = db
        .query(sql)
        .bind(("sid", server_config_id))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

/// Delete a song. The `song_cascade` event removes any `playlist_song`
/// rows that reference this song.
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('song', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
