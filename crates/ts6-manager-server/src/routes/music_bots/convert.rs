//! Wire ⇄ in-process conversions for the music-bot REST surface
//! (PURA-123 WS-5).
//!
//! `ts6-manager-shared::music_bots` is WASM-clean and uses externally-
//! tagged enums + camelCase fields; the in-process `music_bot` crate
//! uses Rust-idiomatic newtypes + path-typed audio sources. The route
//! handlers stay readable when the rename happens here once.

use std::path::PathBuf;

use music_bot::{
    AudioSource as DomainAudioSource, BotId as DomainBotId, BotState as DomainBotState,
    LibraryEntry as DomainLibraryEntry, LibraryEntryId as DomainLibraryEntryId,
    NewLibraryEntry as DomainNewLibraryEntry, NewTrack as DomainNewTrack,
    PlaylistName as DomainPlaylistName, Track as DomainTrack, TrackId as DomainTrackId,
};
use ts6_manager_shared::music_bots as wire;

// ---- IDs ----------------------------------------------------------------

pub fn bot_id_to_wire(id: DomainBotId) -> wire::BotId {
    wire::BotId(id.0)
}

pub fn bot_id_from_wire(id: wire::BotId) -> DomainBotId {
    DomainBotId(id.0)
}

pub fn track_id_to_wire(id: DomainTrackId) -> wire::TrackId {
    wire::TrackId(id.0)
}

pub fn library_entry_id_to_wire(id: DomainLibraryEntryId) -> wire::LibraryEntryId {
    wire::LibraryEntryId(id.0)
}

pub fn library_entry_id_from_wire(id: wire::LibraryEntryId) -> DomainLibraryEntryId {
    DomainLibraryEntryId(id.0)
}

pub fn track_id_from_wire(id: wire::TrackId) -> DomainTrackId {
    DomainTrackId(id.0)
}

pub fn playlist_name_from_str(name: &str) -> DomainPlaylistName {
    DomainPlaylistName(name.to_string())
}

// ---- AudioSource --------------------------------------------------------

pub fn audio_source_to_wire(src: &DomainAudioSource) -> wire::AudioSource {
    match src {
        DomainAudioSource::Url(u) => wire::AudioSource::Url { url: u.clone() },
        DomainAudioSource::LibraryPath(p) => wire::AudioSource::LibraryPath {
            path: p.to_string_lossy().into_owned(),
        },
    }
}

pub fn audio_source_from_wire(src: wire::AudioSource) -> DomainAudioSource {
    match src {
        wire::AudioSource::Url { url } => DomainAudioSource::Url(url),
        wire::AudioSource::LibraryPath { path } => {
            DomainAudioSource::LibraryPath(PathBuf::from(path))
        }
    }
}

// ---- Tracks -------------------------------------------------------------

pub fn track_to_wire(t: &DomainTrack) -> wire::Track {
    wire::Track {
        id: track_id_to_wire(t.id),
        source: audio_source_to_wire(&t.source),
        title: t.title.clone(),
        duration_secs: t.duration_secs,
        requested_by: t.requested_by.clone(),
    }
}

pub fn new_track_from_wire(req: wire::AddTrackRequest) -> DomainNewTrack {
    DomainNewTrack {
        source: audio_source_from_wire(req.source),
        title: req.title,
        duration_secs: req.duration_secs,
        requested_by: req.requested_by,
    }
}

// ---- Library ------------------------------------------------------------

pub fn library_entry_to_wire(e: &DomainLibraryEntry) -> wire::LibraryEntry {
    wire::LibraryEntry {
        id: library_entry_id_to_wire(e.id),
        source: audio_source_to_wire(&e.source),
        title: e.title.clone(),
        tags: e.tags.clone(),
    }
}

pub fn new_library_entry_from_wire(req: wire::AddLibraryEntryRequest) -> DomainNewLibraryEntry {
    DomainNewLibraryEntry {
        source: audio_source_from_wire(req.source),
        title: req.title,
        tags: req.tags,
    }
}

pub fn radio_station_to_wire(e: &DomainLibraryEntry) -> wire::RadioStation {
    wire::RadioStation {
        id: library_entry_id_to_wire(e.id),
        source: audio_source_to_wire(&e.source),
        title: e.title.clone(),
        tags: e.tags.clone(),
    }
}

// ---- BotState -----------------------------------------------------------

pub fn bot_state_to_wire(state: DomainBotState, has_now_playing: bool) -> wire::BotState {
    // The route layer surfaces the synthesised `Playing` state when the
    // bot is online AND has a head-of-queue track. Internal FSM stays
    // unchanged — see `wire::BotState::Playing` doc.
    if has_now_playing && matches!(state, DomainBotState::Connected | DomainBotState::InChannel) {
        return wire::BotState::Playing;
    }
    match state {
        DomainBotState::Disconnected => wire::BotState::Disconnected,
        DomainBotState::Connecting => wire::BotState::Connecting,
        DomainBotState::Connected => wire::BotState::Connected,
        DomainBotState::InChannel => wire::BotState::InChannel,
        DomainBotState::Disconnecting => wire::BotState::Disconnecting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_source_round_trips_through_wire() {
        let src = DomainAudioSource::Url("https://x".into());
        let back = audio_source_from_wire(audio_source_to_wire(&src));
        assert_eq!(back, src);

        let src = DomainAudioSource::LibraryPath(PathBuf::from("a/b.mp3"));
        let back = audio_source_from_wire(audio_source_to_wire(&src));
        assert_eq!(back, src);
    }

    #[test]
    fn bot_state_synthesises_playing_only_when_online_and_has_track() {
        for online in [DomainBotState::Connected, DomainBotState::InChannel] {
            assert_eq!(bot_state_to_wire(online, true), wire::BotState::Playing);
        }
        // Track set but offline → falls back to the underlying state.
        assert_eq!(
            bot_state_to_wire(DomainBotState::Disconnected, true),
            wire::BotState::Disconnected
        );
        // Online but no track → underlying state.
        assert_eq!(
            bot_state_to_wire(DomainBotState::Connected, false),
            wire::BotState::Connected
        );
    }
}
