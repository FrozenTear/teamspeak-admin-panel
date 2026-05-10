//! Bot command surface — PURA-118 WS-1.
//!
//! `BotCommand` is the control-plane vocabulary the supervisor and outer
//! callers use to drive a bot. Audio variants are stubbed here so WS-2's
//! audio pipeline can plug in without churning the dispatch surface or
//! the public API.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store::{NewTrack, PlaylistName, TrackId};

/// Channel ID on the TS6 server. Mirrors `tsclientlib::ChannelId`'s newtype
/// over `u64` but we keep the public surface plain so callers don't need a
/// `tsclientlib` dep just to enqueue a `JoinChannel`.
pub type ChannelId = u64;

/// Commands the supervisor (or any holder of the bot's command sender) can
/// send to a bot actor. Variants are split so that the WS-2 audio pipeline
/// can extend `Audio(...)` without touching lifecycle dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BotCommand {
    /// Initiate connection. No-op when already online or `Connecting`.
    Connect,
    /// Clean disconnect. No-op when already `Disconnected`.
    Disconnect,
    /// Move into the given channel. Requires `Connected` or `InChannel`.
    JoinChannel(ChannelId),
    /// Move out of the current channel — back to whatever the server's
    /// default is. Requires `InChannel`.
    LeaveChannel,
    /// Tear the bot down for good. The actor exits after the disconnect
    /// completes. The supervisor drops the handle.
    Shutdown,
    /// Audio sub-command. WS-2 fills these in; WS-1 acknowledges and logs
    /// only.
    Audio(AudioCommand),
    /// PURA-121 WS-3 — queue mutation. The dispatcher mutates the
    /// `MusicBotStore` and emits `QueueChanged` / `NowPlaying` /
    /// `QueueEmpty` accordingly. Valid in every lifecycle state — you
    /// can stage a queue while the bot is `Disconnected` and the audio
    /// task will pick it up on connect.
    Queue(QueueCommand),
}

/// Audio sub-commands. Stubbed for WS-1 — the bot actor logs them and
/// emits `BotEvent::Error(BotError::AudioNotImplemented)` so that callers
/// can light up the wiring (REST surface in WS-5, chat bridge in WS-4)
/// against the real dispatcher today.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioCommand {
    /// Play a track from a URL or local path. WS-2 wires this to the
    /// yt-dlp / FFmpeg / Opus pipeline.
    Play { source: AudioSource },
    /// Halt playback; clear the active track.
    Stop,
    /// Pause playback at current offset.
    Pause,
    /// Resume from a previous `Pause`.
    Resume,
    /// Skip to the next queued track. Queue lives in WS-3.
    SkipNext,
    /// Skip to the previous track in the queue.
    SkipPrev,
    /// Set output volume in dBFS-ish floating-point gain. WS-2 picks the
    /// exact unit.
    SetVolume(f32),
    /// Update the now-playing metadata (used by ICY tag / chat surface).
    NowPlaying(String),
}

/// Where to fetch audio from. WS-2 will widen this; WS-1 only needs the
/// shape so command dispatch compiles end-to-end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AudioSource {
    /// Anything yt-dlp can resolve — YouTube, SoundCloud, Vimeo, etc.
    Url(String),
    /// Local file inside the music library (path is relative to the
    /// configured library root in WS-3).
    LibraryPath(PathBuf),
}

/// PURA-121 WS-3 — queue-mutation sub-commands the dispatcher routes
/// into the `MusicBotStore`. Playlist + library CRUD goes through
/// `BotSupervisor` direct methods rather than the dispatcher (browser /
/// chat surfaces don't need lifecycle-state coupling for those).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueueCommand {
    /// Append a track to the bot's queue. Emits `QueueChanged`. If the
    /// queue was empty, also emits `NowPlaying(track)` so subscribers
    /// can wire UI off "playback started" without polling.
    Enqueue(NewTrack),
    /// Append every track in the named playlist to the bot's queue.
    /// Emits `QueueChanged`. If the queue was empty before, also emits
    /// `NowPlaying(first_appended)`.
    EnqueuePlaylist(PlaylistName),
    /// Remove a track by id. Emits `QueueChanged`. If the removed track
    /// WAS the head, also emits `NowPlaying(new_head)` or `QueueEmpty`.
    Remove(TrackId),
    /// Replace the queue order with a permutation of the current ids.
    /// Emits `QueueChanged`. If the head changed as a result, also emits
    /// `NowPlaying(new_head)`.
    Reorder(Vec<TrackId>),
    /// Drop every track. Emits `QueueChanged` and `QueueEmpty`.
    Clear,
    /// Pop the head of the queue. WS-2's audio task will send this on
    /// EndOfStream once it's wired into the dispatcher; for now it's
    /// also what `AudioCommand::SkipNext` lowers to. Emits
    /// `QueueChanged` plus either `NowPlaying(new_head)` or `QueueEmpty`.
    Advance,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BotCommand` round-trips through serde so the REST surface in WS-5
    /// can deserialise straight into the dispatch enum without a separate
    /// wire-format type.
    #[test]
    fn bot_command_serde_round_trip() {
        let cmds = [
            BotCommand::Connect,
            BotCommand::Disconnect,
            BotCommand::JoinChannel(42),
            BotCommand::LeaveChannel,
            BotCommand::Shutdown,
            BotCommand::Audio(AudioCommand::Play {
                source: AudioSource::Url("https://example.com/stream".into()),
            }),
            BotCommand::Audio(AudioCommand::Stop),
            BotCommand::Audio(AudioCommand::SetVolume(0.5)),
            BotCommand::Queue(QueueCommand::Enqueue(NewTrack::url(
                "demo",
                "https://example.com/demo.mp3",
            ))),
            BotCommand::Queue(QueueCommand::EnqueuePlaylist("lo-fi".into())),
            BotCommand::Queue(QueueCommand::Remove(TrackId(7))),
            BotCommand::Queue(QueueCommand::Reorder(vec![TrackId(7), TrackId(8)])),
            BotCommand::Queue(QueueCommand::Clear),
            BotCommand::Queue(QueueCommand::Advance),
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: BotCommand = serde_json::from_str(&json).unwrap();
            // Cheap check: re-serialise and compare strings — round-trip
            // stable without needing PartialEq on every variant.
            assert_eq!(json, serde_json::to_string(&back).unwrap());
        }
    }
}
