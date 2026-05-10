//! Bot command surface — PURA-118 WS-1.
//!
//! `BotCommand` is the control-plane vocabulary the supervisor and outer
//! callers use to drive a bot. Audio variants are stubbed here so WS-2's
//! audio pipeline can plug in without churning the dispatch surface or
//! the public API.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioSource {
    /// Anything yt-dlp can resolve — YouTube, SoundCloud, Vimeo, etc.
    Url(String),
    /// Local file inside the music library (path is relative to the
    /// configured library root in WS-3).
    LibraryPath(PathBuf),
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
