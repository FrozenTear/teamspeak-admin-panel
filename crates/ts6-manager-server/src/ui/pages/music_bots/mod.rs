//! `/music-bots/...` — operator-facing music-bot UI (PURA-124 WS-6).
//!
//! Five pages drive the music-bot product:
//!
//! - [`BotsIndexPage`] — `/music-bots` — list, spawn, connect, delete.
//! - [`BotDetailPage`] — `/music-bots/:bot_id` — connection + queue + SSE.
//! - [`MusicLibraryPage`] — `/music-bots/:bot_id/library` — per-bot saved
//!   sources.
//! - [`MusicPlaylistsPage`] — `/music-bots/:bot_id/playlists` — playlist
//!   CRUD + enqueue-to-bot.
//! - [`RadioStationsPage`] — `/music-bots/:bot_id/radio` — radio presets +
//!   one-shot play.
//!
//! Pages share their REST + SSE plumbing through
//! [`crate::client::music_bots`]; on-screen styling reuses the existing
//! design tokens (`stack-md`, `data-table`, `card`, `empty`, `crumb`,
//! `page-header`) so no new design language is introduced.

mod detail;
mod index;
mod library;
mod playlists;
mod radio_stations;
mod shared;

pub use detail::BotDetailPage;
pub use index::BotsIndexPage;
pub use library::MusicLibraryPage;
pub use playlists::MusicPlaylistsPage;
pub use radio_stations::RadioStationsPage;
