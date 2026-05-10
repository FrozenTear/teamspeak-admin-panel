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
mod command;
mod config;
mod event;
mod state;
mod supervisor;

pub use backoff::{BackoffConfig, ExponentialBackoff};
pub use command::{AudioCommand, AudioSource, BotCommand, ChannelId};
pub use config::{BotConfig, BotId};
pub use event::{BotError, BotEvent, DisconnectKind};
pub use state::{BotState, IllegalTransition};
pub use supervisor::{spawn_bot, BotHandle, BotInfo, BotSupervisor, SendError};
