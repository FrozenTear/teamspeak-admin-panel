//! `PlaylistSong` repo (spec §4.2.13).
//!
//! The composite uniqueness index `(playlistId, songId)` lives in the
//! migration; insert returns `Err` if the same song is added twice to one
//! playlist (covered by the §4.5 composite-unique test).

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct PlaylistSong {
    pub id: i64,
    pub playlistId: i64,
    pub songId: i64,
    pub position: i64,
}

#[derive(Debug, Clone)]
pub struct NewPlaylistSong {
    pub playlistId: i64,
    pub songId: i64,
    pub position: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    playlistId,
    songId,
    position
";

pub async fn insert(db: &Database, new: NewPlaylistSong) -> Result<PlaylistSong> {
    let sql = format!(
        "CREATE type::record('playlist_song', sequence::nextval('playlist_song_id'))
            CONTENT {{
                playlistId: $playlistId,
                songId: $songId,
                position: $position
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("playlistId", new.playlistId))
        .bind(("songId", new.songId))
        .bind(("position", new.position))
        .await
        .context("playlist_song insert query failed")?
        .check()?;
    let row: Option<PlaylistSong> = resp.take(0)?;
    row.context("playlist_song insert returned no row")
}

pub async fn list_for_playlist(db: &Database, playlist_id: i64) -> Result<Vec<PlaylistSong>> {
    // Spec §4.2.13: position ties allowed, sort stable. Adding the
    // secondary `id ASC` makes the tie-break deterministic at the repo
    // layer so callers don't see flaky order on equal positions.
    let sql = format!(
        "SELECT {PROJECTION} FROM playlist_song
            WHERE playlistId = $pid ORDER BY position ASC, id ASC;"
    );
    let mut resp = db.query(sql).bind(("pid", playlist_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn set_position(db: &Database, id: i64, position: i64) -> Result<Option<PlaylistSong>> {
    let sql = format!(
        "UPDATE type::record('playlist_song', $id) MERGE {{ position: $position }} RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("id", id))
        .bind(("position", position))
        .await?
        .check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('playlist_song', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
