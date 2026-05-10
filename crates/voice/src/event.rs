//! Bot event surface — PURA-118 WS-1.
//!
//! `BotEvent` is the bot → world broadcast vocabulary. Many subscribers
//! (UI, REST, chat-bridge) need a fan-out, so the supervisor exposes the
//! events as a `tokio::sync::broadcast` channel — see `supervisor.rs`.

use serde::{Deserialize, Serialize};

use crate::command::ChannelId;
use crate::state::BotState;
use crate::store::{PlaylistName, Track};

/// Events emitted by a bot actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BotEvent {
    /// Lifecycle transition. `from` is the previous state; `to` is the
    /// state the bot has just moved into.
    StateChanged { from: BotState, to: BotState },
    /// Handshake completed against the TS6 server.
    /// `default_channel` is the channel the server placed us in (callers
    /// can pass it back via `BotCommand::JoinChannel` to verify the join
    /// machinery without poking at the underlying tsclientlib book).
    Connected {
        client_id: u16,
        default_channel: ChannelId,
    },
    /// Disconnected from the TS6 server. `kind` separates a clean
    /// `Shutdown` / `Disconnect` from an unexpected drop that the actor
    /// will auto-reconnect from.
    Disconnected {
        kind: DisconnectKind,
        reason: String,
    },
    /// Bot is now in `channel_id`. Fires once with the default channel
    /// right after `Connected`, and once per successful `JoinChannel` /
    /// `LeaveChannel`.
    JoinedChannel { channel_id: ChannelId },
    /// Bot is no longer in a specific channel (left to the default).
    LeftChannel,
    /// PURA-121 WS-3 — queue mutated. `len` is the upcoming queue length
    /// (head included); `current` is the head if any. Fires after every
    /// successful `BotCommand::Queue(...)` op AND after auto-advance
    /// (`AudioCommand::SkipNext` / WS-2 EndOfStream wiring).
    QueueChanged {
        len: usize,
        current: Option<Track>,
    },
    /// PURA-121 WS-3 — a fresh track is now at the head of the queue
    /// (about to play / playing). Fires when `queue_dequeue_head` exposes
    /// a new head, or when `enqueue` lands a track into a previously
    /// empty queue. The full track is included so subscribers don't need
    /// a separate `queue_current` round-trip.
    NowPlaying(Track),
    /// PURA-121 WS-3 — the queue drained. Fires when the last track
    /// finishes (auto-advance returns `None`) or `queue_clear` empties
    /// a non-empty queue.
    QueueEmpty,
    /// PURA-121 WS-3 — a playlist was created / renamed / deleted, or
    /// its contents changed. Subscribers refetch via
    /// `MusicBotStore::playlist_list_tracks`. `name` is the post-mutation
    /// name (after a rename, this is the new name).
    PlaylistChanged(PlaylistName),
    /// PURA-121 WS-3 — the library was mutated. Coarse-grained because
    /// most consumers refetch the whole list on change.
    LibraryChanged,
    /// Recoverable error during dispatch — illegal command for current
    /// state, transient send failure, audio-not-implemented stub, etc.
    Error(BotError),
}

/// Why the bot is no longer connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectKind {
    /// Clean shutdown — `Disconnect` or `Shutdown` command from the
    /// supervisor / outer caller. The actor will not auto-reconnect.
    Clean,
    /// Unexpected drop — network loss, server kick, handshake timeout.
    /// The actor enters its retry loop and will emit `StateChanged` to
    /// `Connecting` again after the backoff sleep.
    Dropped,
    /// Bot is being torn down for good (`Shutdown` flow). Implies
    /// `Clean` plus actor-task exit.
    ShutdownRequested,
}

/// Error variants surfaced to callers via `BotEvent::Error`.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum BotError {
    /// Command was rejected because the current state cannot accept it.
    /// The dispatcher logs the offending pair and stays in the same
    /// state.
    #[error("command {command} rejected in state {state:?}")]
    CommandRejected { command: String, state: BotState },

    /// Audio command received but the WS-2 audio pipeline is not wired
    /// up yet. WS-2 removes this branch.
    #[error("audio command {0} not implemented in WS-1")]
    AudioNotImplemented(String),

    /// PURA-121 WS-3 — a queue / playlist / library command failed at
    /// the store layer. The leaf message is the `StoreError`'s display.
    #[error("store error in {op}: {message}")]
    Store { op: String, message: String },

    /// tsclientlib returned an error during connect / channel-switch /
    /// disconnect. We surface the leaf message; the full chain is logged
    /// at `error!` level by the actor.
    #[error("connection error: {0}")]
    Connection(String),

    /// Connect attempt timed out.
    #[error("handshake timed out after {timeout_secs}s (attempt #{attempt})")]
    HandshakeTimeout { timeout_secs: u64, attempt: u32 },

    /// Internal dispatch / channel-closed errors. Most callers won't see
    /// this — it implies the bot actor is going away.
    #[error("internal: {0}")]
    Internal(String),
}
