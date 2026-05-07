//! `MusicBot` repo (spec §4.2.10).
//!
//! Holds *configuration* only — runtime state (current track, queue,
//! status) lives in memory per spec. `identityData` is sensitive (TS3
//! private key blob) and SHOULD be stripped from list responses by the
//! REST layer (§7.5); the repo returns it like any other field.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MusicBot {
    pub id: i64,
    pub name: String,
    pub serverConfigId: i64,
    pub nickname: String,
    pub serverPassword: Option<String>,
    pub defaultChannel: Option<String>,
    pub channelPassword: Option<String>,
    pub nowPlayingChannelId: Option<String>,
    pub voicePort: i64,
    pub volume: i64,
    pub identityData: Option<String>,
    pub autoStart: bool,
    pub streamPreset: String,
    pub sidecarPort: i64,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMusicBot {
    pub name: String,
    pub serverConfigId: i64,
    pub nickname: String,
    pub serverPassword: Option<String>,
    pub defaultChannel: Option<String>,
    pub channelPassword: Option<String>,
    pub nowPlayingChannelId: Option<String>,
    pub voicePort: i64,
    pub volume: i64,
    pub identityData: Option<String>,
    pub autoStart: bool,
    pub streamPreset: String,
    pub sidecarPort: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    serverConfigId,
    nickname,
    serverPassword,
    defaultChannel,
    channelPassword,
    nowPlayingChannelId,
    voicePort,
    volume,
    identityData,
    autoStart,
    streamPreset,
    sidecarPort,
    createdAt,
    updatedAt
";

pub async fn insert(db: &Database, new: NewMusicBot) -> Result<MusicBot> {
    let sql = format!(
        "CREATE type::record('music_bot', sequence::nextval('music_bot_id'))
            CONTENT {{
                name: $name,
                serverConfigId: $serverConfigId,
                nickname: $nickname,
                serverPassword: $serverPassword,
                defaultChannel: $defaultChannel,
                channelPassword: $channelPassword,
                nowPlayingChannelId: $nowPlayingChannelId,
                voicePort: $voicePort,
                volume: $volume,
                identityData: $identityData,
                autoStart: $autoStart,
                streamPreset: $streamPreset,
                sidecarPort: $sidecarPort
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("serverConfigId", new.serverConfigId))
        .bind(("nickname", new.nickname))
        .bind(("serverPassword", new.serverPassword))
        .bind(("defaultChannel", new.defaultChannel))
        .bind(("channelPassword", new.channelPassword))
        .bind(("nowPlayingChannelId", new.nowPlayingChannelId))
        .bind(("voicePort", new.voicePort))
        .bind(("volume", new.volume))
        .bind(("identityData", new.identityData))
        .bind(("autoStart", new.autoStart))
        .bind(("streamPreset", new.streamPreset))
        .bind(("sidecarPort", new.sidecarPort))
        .await
        .context("music_bot insert query failed")?
        .check()?;
    let row: Option<MusicBot> = resp.take(0)?;
    row.context("music_bot insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<MusicBot>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('music_bot', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list(db: &Database) -> Result<Vec<MusicBot>> {
    let sql = format!("SELECT {PROJECTION} FROM music_bot ORDER BY id ASC;");
    let mut resp = db.query(sql).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<MusicBot>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM music_bot WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("sid", server_config_id)).await?.check()?;
    Ok(resp.take(0)?)
}

/// Delete a music bot. The `music_bot_set_null_playlist` event in
/// 0004 nulls out `playlist.musicBotId` for any playlists that reference
/// this bot — playlists themselves survive (§4.2.12, §4.5).
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('music_bot', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
