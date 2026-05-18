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

mod audio;
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
pub use chat::{ParseError as ChatParseError, ParsedCommand, parse as parse_chat_command};
pub use command::{AudioCommand, AudioSource, BotCommand, ChannelId, QueueCommand};
pub use config::{BotConfig, BotId};
pub use event::{BotError, BotEvent, DisconnectKind};
pub use state::{BotState, IllegalTransition};
pub use store::{
    InMemoryMusicBotStore, LibraryEntry, LibraryEntryId, MusicBotStore, NewLibraryEntry, NewTrack,
    PlaylistName, SNAPSHOT_VERSION, StoreError, StoreResult, Track, TrackId,
};
pub use supervisor::{BotHandle, BotInfo, BotSupervisor, SendError, spawn_bot};

/// PURA-359 — start the persistent yt-dlp resolver service so it is warm
/// (extractors imported) by the first `!play`. Call once at server boot.
/// Re-exported from `music-bot-audio` so the server crate can warm the
/// resolver without taking a direct dependency on the audio crate.
pub use music_bot_audio::resolver::warm_up as warm_resolver;
