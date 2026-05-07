//! `Playlist` repo (spec §4.2.12).
//!
//! Per §4.2.12 + §4.5 the `musicBotId` FK is *set null on delete* (not
//! cascade) — the migration encodes this with the
//! `music_bot_set_null_playlist` event. Deleting a playlist itself
//! cascades to `playlist_song`.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub musicBotId: Option<i64>,
    pub createdAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewPlaylist {
    pub name: String,
    pub musicBotId: Option<i64>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    musicBotId,
    createdAt
";

pub async fn insert(db: &Database, new: NewPlaylist) -> Result<Playlist> {
    let sql = format!(
        "CREATE type::record('playlist', sequence::nextval('playlist_id'))
            CONTENT {{
                name: $name,
                musicBotId: $musicBotId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("musicBotId", new.musicBotId))
        .await
        .context("playlist insert query failed")?
        .check()?;
    let row: Option<Playlist> = resp.take(0)?;
    row.context("playlist insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<Playlist>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('playlist', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<Playlist>> {
    let sql = format!("SELECT {PROJECTION} FROM playlist ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_music_bot(db: &Database, music_bot_id: i64) -> Result<Vec<Playlist>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM playlist WHERE musicBotId = $mid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("mid", music_bot_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn rename(db: &Database, id: i64, name: String) -> Result<Option<Playlist>> {
    let sql = format!(
        "UPDATE type::record('playlist', $id) MERGE {{ name: $name }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("name", name))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('playlist', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
