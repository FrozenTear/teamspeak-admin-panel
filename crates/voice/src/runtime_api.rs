//! Internal loopback control-plane DTOs for `ts6-manager-music`.
//!
//! Fullstack (Panel/API/Surreal) stays the public control channel.
//! This crate's music unit is the **only** owner of decode → Opus →
//! TS6 wire send. Types here are JSON over `127.0.0.1:3002`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::command::BotCommand;
use crate::config::{BotConfig, BotId};
use crate::event::BotEvent;
use crate::store::{
    LibraryEntry, LibraryEntryId, NewLibraryEntry, NewTrack, PlaylistName, StoreError, Track,
    TrackId,
};
use crate::supervisor::BotInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub bots: usize,
    /// Always `owned` — this process is the only send loop.
    pub send_loop: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub config: BotConfig,
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnResponse {
    pub id: BotId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse {
    pub bots: Vec<BotInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub command: BotCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsRequest {
    /// `Some(None)` clears; `Some(Some(path))` sets; `None` leaves unchanged.
    #[serde(default)]
    pub yt_cookie: Option<Option<PathBuf>>,
    #[serde(default)]
    pub yt_api_key: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugReportContextResponse {
    pub music_bot_latency: String,
    pub log_tail: String,
}

/// Supervisor mutations that also emit `BotEvent`s (playlist / library).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MutateOp {
    PlaylistCreate {
        bot: u64,
        name: String,
    },
    PlaylistRename {
        bot: u64,
        old: String,
        new: String,
    },
    PlaylistDelete {
        bot: u64,
        name: String,
    },
    PlaylistAddTrack {
        bot: u64,
        name: String,
        track: NewTrack,
    },
    PlaylistRemoveTrack {
        bot: u64,
        name: String,
        id: u64,
    },
    PlaylistList {
        bot: u64,
    },
    PlaylistListTracks {
        bot: u64,
        name: String,
    },
    EnqueuePlaylist {
        bot: u64,
        name: String,
    },
    LibraryAdd {
        bot: u64,
        entry: NewLibraryEntry,
    },
    LibraryRemove {
        bot: u64,
        id: u64,
    },
    LibraryLookup {
        bot: u64,
        id: u64,
    },
    LibraryList {
        bot: u64,
        tag: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreOp {
    QueueEnqueue {
        bot: u64,
        track: NewTrack,
    },
    QueueDequeueHead {
        bot: u64,
    },
    QueuePeek {
        bot: u64,
    },
    QueueClear {
        bot: u64,
    },
    QueueReorder {
        bot: u64,
        order: Vec<u64>,
    },
    QueueRemove {
        bot: u64,
        id: u64,
    },
    QueueCurrent {
        bot: u64,
    },
    QueueSetHeadTitle {
        bot: u64,
        title: String,
    },
    PlaylistCreate {
        bot: u64,
        name: String,
    },
    PlaylistRename {
        bot: u64,
        old: String,
        new: String,
    },
    PlaylistDelete {
        bot: u64,
        name: String,
    },
    PlaylistAddTrack {
        bot: u64,
        name: String,
        track: NewTrack,
    },
    PlaylistRemoveTrack {
        bot: u64,
        name: String,
        id: u64,
    },
    PlaylistListTracks {
        bot: u64,
        name: String,
    },
    PlaylistList {
        bot: u64,
    },
    EnqueuePlaylist {
        bot: u64,
        name: String,
    },
    LibraryAdd {
        bot: u64,
        entry: NewLibraryEntry,
    },
    LibraryRemove {
        bot: u64,
        id: u64,
    },
    LibraryLookup {
        bot: u64,
        id: u64,
    },
    LibraryList {
        bot: u64,
        tag: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub error: String,
}

pub fn store_err_to_wire(err: &StoreError) -> WireError {
    WireError {
        error: err.to_string(),
    }
}

pub fn store_err_from_wire(msg: &str) -> StoreError {
    StoreError::Backend(msg.to_string())
}

/// Helpers so remote clients do not re-implement newtype wrapping.
pub fn bot(id: u64) -> BotId {
    BotId(id)
}
pub fn playlist(name: impl Into<String>) -> PlaylistName {
    PlaylistName(name.into())
}
pub fn track_id(id: u64) -> TrackId {
    TrackId(id)
}
pub fn library_id(id: u64) -> LibraryEntryId {
    LibraryEntryId(id)
}

// Re-export store types used on the wire so the server crate can name them
// without a second path.
pub type WireTrack = Track;
pub type WireLibraryEntry = LibraryEntry;
pub type WireBotEvent = BotEvent;
