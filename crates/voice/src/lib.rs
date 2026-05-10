//! Music-bot product crate — PURA-117 / PURA-118 WS-1.
//!
//! Owns the **bot lifecycle skeleton** for the broader voice bots track.
//! No audio dispatch yet — WS-2 plugs the yt-dlp / FFmpeg / Opus pipeline
//! into `BotCommand::Audio(...)` here, WS-3 layers the queue / playlist /
//! library on top, WS-4 bridges TS6 chat into commands, WS-5 surfaces
//! REST endpoints, and WS-6 wires the FE-PAGES Dioxus UI.
//!
//! See `docs/voice/music-bot-lifecycle.md` for the state diagram and
//! command/event tables (renormalised every WS).

mod backoff;
mod bot;
mod chat;
mod command;
mod config;
mod event;
mod state;
mod store;
mod supervisor;

pub use backoff::{BackoffConfig, ExponentialBackoff};
pub use chat::{parse as parse_chat_command, ParseError as ChatParseError, ParsedCommand};
pub use command::{AudioCommand, AudioSource, BotCommand, ChannelId, QueueCommand};
pub use config::{BotConfig, BotId};
pub use event::{BotError, BotEvent, DisconnectKind};
pub use state::{BotState, IllegalTransition};
pub use store::{
    InMemoryMusicBotStore, LibraryEntry, LibraryEntryId, MusicBotStore, NewLibraryEntry,
    NewTrack, PlaylistName, StoreError, StoreResult, Track, TrackId, SNAPSHOT_VERSION,
};
pub use supervisor::{spawn_bot, BotHandle, BotInfo, BotSupervisor, SendError};
