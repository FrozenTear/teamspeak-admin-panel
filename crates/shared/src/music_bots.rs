//! Wire-format types for the music-bot REST surface (PURA-117 / PURA-123 WS-5).
//!
//! Mirrors the `music-bot` crate's domain types but stays WASM-clean (no
//! `tokio`, `tsclientlib`, or path-typed fields). The Dioxus FE (WS-6)
//! pulls these in via `ts6_manager_shared::music_bots`; the axum server
//! converts to the in-process `music_bot::*` types at the route boundary.
//!
//! Conventions match the rest of the crate:
//! - `#[serde(rename_all = "camelCase")]` on every struct so the wire is
//!   `botId` / `requestedAt`, while Rust source stays `bot_id` /
//!   `requested_at`.
//! - Newtype IDs (`BotId`, `TrackId`, `LibraryEntryId`) are
//!   `#[serde(transparent)]` over `u64`, matching the in-process
//!   `music_bot::{BotId, TrackId, LibraryEntryId}` shape so a stamped id
//!   round-trips one-for-one.
//! - `AudioSource` and `BotState` are externally-tagged enums with
//!   snake_case discriminants — the wire is
//!   `{ "kind": "url", "url": "https://..." }` and `"connected"`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bot identifier minted by the supervisor on `POST /music-bots`. Stable
/// for the bot's lifetime; reused by every nested resource path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BotId(pub u64);

/// Track identifier minted by the store on `enqueue` /
/// `playlist_add_track`. Survives snapshot/restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u64);

/// Library-entry identifier (also covers radio-station presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LibraryEntryId(pub u64);

/// Where the audio comes from. Externally tagged so the wire is
/// `{ "kind": "url", "url": "..." }` / `{ "kind": "libraryPath", "path": "..." }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AudioSource {
    /// Anything yt-dlp can resolve.
    Url { url: String },
    /// Local file inside the music library (path is relative to the
    /// configured library root).
    LibraryPath { path: String },
}

/// Bot lifecycle state. Snake-case discriminants on the wire (`"connected"`,
/// `"in_channel"`, …) so JS callers can branch with cheap string compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotState {
    Disconnected,
    Connecting,
    Connected,
    InChannel,
    Disconnecting,
    /// Bot reports it is currently playing a track. The lifecycle FSM in
    /// `music_bot::BotState` does not split `Playing` from `InChannel`
    /// today (audio dispatch is WS-2 territory) — the route layer
    /// synthesises this state when `now_playing` is `Some` so the FE has
    /// a single field to drive the now-playing indicator. Stays
    /// orthogonal to the underlying lifecycle FSM.
    Playing,
}

/// One queued (or playlisted) track. Field shape matches
/// `music_bot::store::Track` so the route layer is a pure rename.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: TrackId,
    pub source: AudioSource,
    pub title: String,
    pub duration_secs: Option<u64>,
    pub requested_by: Option<String>,
}

/// One library entry. `tags` carries free-form labels; the radio-station
/// shortcut (`POST /radio-stations`) writes the marker tag
/// [`RADIO_TAG`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub id: LibraryEntryId,
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Marker tag the server writes onto a [`LibraryEntry`] to expose it via
/// `/radio-stations`. Wire constant — clients comparing tags MUST use the
/// same string.
pub const RADIO_TAG: &str = "radio";

/// `POST /music-bots` body. `identityPath` defaults to
/// `<config-dir>/bot-{id}.identity` server-side when omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBotRequest {
    pub name: String,
    pub server_addr: String,
    #[serde(default)]
    pub identity_path: Option<String>,
    /// Defaults to `true` server-side — matches `BotConfig::auto_connect`.
    #[serde(default)]
    pub auto_connect: Option<bool>,
}

/// `POST /music-bots/{id}/join` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinChannelRequest {
    pub channel_id: u64,
}

/// `GET /music-bots` row. Trimmed view — the FE list page renders this
/// shape; `GET /music-bots/{id}` returns [`MusicBotDetail`] for per-bot
/// pages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicBotSummary {
    pub id: BotId,
    pub name: String,
    pub server_addr: String,
    pub state: BotState,
    /// Currently-playing track, if any. Mirrors
    /// `MusicBotStore::queue_current` when the bot is `Playing` /
    /// `Connected`; `None` when the queue is empty.
    pub now_playing: Option<Track>,
    /// PURA-347 — elapsed playback position of `now_playing`, in whole
    /// seconds. Truthful play clock (frames sent on the wire — stalls
    /// across a pause, never drifts), refreshed by `BotEvent::Progress`.
    /// `None` when nothing is playing or before the first one-second
    /// tick; the FE renders a left-anchored progress bar from it against
    /// `now_playing.durationSecs`. Additive — old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_playing_elapsed_secs: Option<u64>,
    /// PURA-261 — cause of the most recent playback failure, when the
    /// bot is *not* currently playing. Populated when an audio pipeline
    /// ended without producing audio (bad URL, bot-gated video, codec
    /// error); `None` while a track is playing or after a clean finish.
    /// Lets the UI show *why* playback stopped instead of a silently
    /// stuck `Playing` indicator. `#[serde(default)]` keeps it
    /// backward-compatible with older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `GET /music-bots/{id}` body. Adds the queue snapshot and the channel
/// the bot currently sits in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicBotDetail {
    pub id: BotId,
    pub name: String,
    pub server_addr: String,
    pub state: BotState,
    pub now_playing: Option<Track>,
    /// PURA-347 — elapsed playback position of `now_playing`, in whole
    /// seconds. See [`MusicBotSummary::now_playing_elapsed_secs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_playing_elapsed_secs: Option<u64>,
    pub queue: Vec<Track>,
    /// Channel the bot is currently in. `None` when `state` is
    /// `Disconnected` / `Connecting` / `Disconnecting`.
    pub channel_id: Option<u64>,
    /// PURA-261 — cause of the most recent playback failure. See
    /// [`MusicBotSummary::last_error`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `POST /music-library` body. Tags default to empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddLibraryEntryRequest {
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `PATCH /music-library/{trackId}` body — partial update of the
/// metadata. Either field may be omitted (no change).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchLibraryEntryRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// `POST /playlists` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePlaylistRequest {
    /// Bot the playlist lives under — playlists are per-bot.
    pub bot: BotId,
    pub name: String,
}

/// `PATCH /playlists/{name}` body — currently rename-only. Future fields
/// (description, ordering hints) extend this struct without breaking
/// callers because `#[serde(default)]` lets old clients omit them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchPlaylistRequest {
    #[serde(default)]
    pub new_name: Option<String>,
}

/// `POST /playlists/{name}/tracks` body. Same shape as the queue
/// `enqueue` body so callers can splat one into the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTrackRequest {
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub requested_by: Option<String>,
}

/// `GET /playlists?bot={id}` rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistSummary {
    pub bot: BotId,
    pub name: String,
    pub track_count: usize,
}

/// `GET /playlists/{name}?bot={id}` body — full track list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistDetail {
    pub bot: BotId,
    pub name: String,
    pub tracks: Vec<Track>,
}

/// `POST /music-bots/{id}/play` body — dispatches `Audio(Play{source})`
/// against the bot actor (PURA-126 WS-6 follow-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayRequest {
    pub source: AudioSource,
}

/// `POST /music-bots/{id}/volume` body. `gain` is the same dBFS-ish unit
/// that `music_bot::AudioCommand::SetVolume(f32)` uses (PURA-126 WS-6
/// follow-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVolumeRequest {
    pub gain: f32,
}

/// `POST /music-bots/{id}/queue` body — append a single track to the
/// bot's queue without going through a playlist (PURA-126 WS-6 follow-up).
/// Same shape as [`AddTrackRequest`] but renamed for the queue surface so
/// the FE can splat between the two without naming collisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueTrackRequest {
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub requested_by: Option<String>,
}

/// `POST /music-bots/{id}/queue/reorder` body — replace the queue order
/// with the given permutation of current ids (PURA-126 WS-6 follow-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderQueueRequest {
    pub track_ids: Vec<TrackId>,
}

/// `POST /radio-stations` body. The server stamps the [`RADIO_TAG`] tag
/// onto the persisted [`LibraryEntry`] so `/radio-stations` and
/// `/music-library?tag=radio` agree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRadioStationRequest {
    /// Bot the station is registered under — radio stations are per-bot,
    /// same as the library + playlists.
    pub bot: BotId,
    pub source: AudioSource,
    pub title: String,
    /// Extra tags besides `radio` — useful for "lo-fi" / "talk" / etc.
    /// Categorisation. The server always adds [`RADIO_TAG`] before the
    /// extras; duplicates are deduped server-side.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `GET /radio-stations?bot={id}` row. Same id space as
/// [`LibraryEntry`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioStation {
    pub id: LibraryEntryId,
    pub source: AudioSource,
    pub title: String,
    /// Verbatim tags from the underlying [`LibraryEntry`]. Includes
    /// [`RADIO_TAG`] by construction — clients may filter it out for
    /// presentation.
    pub tags: Vec<String>,
}

/// One row in the `/music-requests` log. Fired as a side-effect of any
/// route mutation that enqueued a track (queue enqueue, playlist
/// enqueue, radio play). WS-4 chat commands write into the same log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicRequest {
    pub id: u64,
    pub bot: BotId,
    /// `TrackId` the enqueue minted, when the request resolved into the
    /// bot's queue. `None` for radio-station plays that bypassed the
    /// queue and went straight to `AudioCommand::Play`.
    pub track_id: Option<TrackId>,
    pub source: AudioSource,
    pub title: String,
    /// Free-form caller identity. The chat bridge fills this with the TS
    /// nickname; the REST surface fills it from an optional `requestedBy`
    /// query field on the mutation.
    pub requested_by: Option<String>,
    pub requested_at: DateTime<Utc>,
}

/// JSON error envelope returned by every endpoint on a non-2xx response.
/// Mirrors the existing `routes::control::ErrorBody` shape so the FE has
/// one error-handling path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: String,
    /// Optional sub-classifier: `"not_found"`, `"conflict"`, `"validation"`.
    /// Stable strings the FE branches on; unknown values render as a
    /// generic error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Optional free-form details. Avoid leaking server-side internals
    /// here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ErrorBody {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: None,
            details: None,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

/// `BotEvent` projection for SSE clients. The native `music_bot::BotEvent`
/// uses untagged variants (`{"StateChanged":{...}}`); this projection adds
/// a `"type"` discriminant so JS callers can `switch(event.type)`. The
/// route layer synthesises one of these per broadcast item before
/// emitting on the SSE stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BotEventWire {
    StateChanged {
        from: BotState,
        to: BotState,
    },
    Connected {
        client_id: u16,
        default_channel: u64,
    },
    Disconnected {
        kind: String,
        reason: String,
    },
    JoinedChannel {
        channel_id: u64,
    },
    LeftChannel,
    QueueChanged {
        len: usize,
        current: Option<Track>,
    },
    NowPlaying {
        track: Track,
    },
    QueueEmpty,
    /// PURA-154 — the audio pipeline drained. Mirrors
    /// `music_bot::BotEvent::AudioFinished`; subscribers use the
    /// `reason` to distinguish a clean EOF from a `Stop` / `SkipNext`.
    AudioFinished {
        reason: String,
    },
    /// PURA-347 — coarse playback-progress tick, emitted roughly once per
    /// second while a track plays. `elapsed_secs` is the truthful play
    /// clock (frames sent on the wire, not wall time — it stalls across a
    /// pause and never drifts). The FE reduces these into the now-playing
    /// progress bar; a fresh `NowPlaying` restarts the clock from 0. The
    /// field stays snake_case to match the other `BotEventWire` variants
    /// (`Connected`, `JoinedChannel`).
    Progress {
        elapsed_secs: u64,
    },
    PlaylistChanged {
        name: String,
    },
    LibraryChanged,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_source_round_trips_with_kind_discriminator() {
        let url = AudioSource::Url {
            url: "https://example.com/x.mp3".into(),
        };
        let json = serde_json::to_string(&url).unwrap();
        assert!(json.contains(r#""kind":"url""#), "got: {json}");
        assert!(json.contains(r#""url":"https://example.com/x.mp3""#));
        let back: AudioSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, url);

        let lib = AudioSource::LibraryPath {
            path: "lo-fi/track.mp3".into(),
        };
        let json = serde_json::to_string(&lib).unwrap();
        assert!(json.contains(r#""kind":"libraryPath""#), "got: {json}");
        assert!(json.contains(r#""path":"lo-fi/track.mp3""#));
    }

    #[test]
    fn camel_case_keys_at_the_wire_for_every_request_body() {
        let body = CreateBotRequest {
            name: "DJ".into(),
            server_addr: "127.0.0.1:9987".into(),
            identity_path: Some("./bot1.identity".into()),
            auto_connect: Some(true),
        };
        let json = serde_json::to_string(&body).unwrap();
        for forbidden in ["server_addr", "identity_path", "auto_connect"] {
            assert!(
                !json.contains(forbidden),
                "snake_case `{forbidden}` leaked: {json}"
            );
        }
        for required in ["serverAddr", "identityPath", "autoConnect"] {
            assert!(json.contains(required), "missing `{required}`: {json}");
        }
    }

    #[test]
    fn bot_state_serialises_snake_case() {
        for (state, expected) in [
            (BotState::Disconnected, "\"disconnected\""),
            (BotState::InChannel, "\"in_channel\""),
            (BotState::Playing, "\"playing\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), expected);
        }
    }

    #[test]
    fn newtype_ids_are_transparent() {
        let id = BotId(7);
        assert_eq!(serde_json::to_string(&id).unwrap(), "7");
        let back: BotId = serde_json::from_str("7").unwrap();
        assert_eq!(back, BotId(7));
    }

    #[test]
    fn music_request_round_trip_preserves_camel_case_timestamp() {
        let now = chrono::Utc::now();
        let req = MusicRequest {
            id: 11,
            bot: BotId(1),
            track_id: Some(TrackId(42)),
            source: AudioSource::Url {
                url: "https://example.com/song.mp3".into(),
            },
            title: "Song".into(),
            requested_by: Some("alice".into()),
            requested_at: now,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("trackId"));
        assert!(json.contains("requestedBy"));
        assert!(json.contains("requestedAt"));
        assert!(!json.contains("track_id"));
        let back: MusicRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn radio_tag_marker_constant_is_stable() {
        // Wire constant — flip this and you break every existing FE / chat
        // bridge that filters by the marker.
        assert_eq!(RADIO_TAG, "radio");
    }

    #[test]
    fn error_body_omits_optional_fields_on_the_wire() {
        let err = ErrorBody::new("not found");
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(json, r#"{"error":"not found"}"#);
        let err = err.with_code("not_found");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains(r#""code":"not_found""#));
    }

    #[test]
    fn ws6_followup_request_bodies_use_camel_case() {
        // EnqueueTrackRequest — keys must be camelCase on the wire.
        let body = EnqueueTrackRequest {
            source: AudioSource::Url {
                url: "https://x".into(),
            },
            title: "t".into(),
            duration_secs: Some(10),
            requested_by: Some("alice".into()),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("durationSecs"), "got: {json}");
        assert!(json.contains("requestedBy"), "got: {json}");
        assert!(!json.contains("duration_secs"));

        // ReorderQueueRequest — `trackIds` on the wire, `Vec<TrackId>` on
        // the inside, transparent over `u64`.
        let body = ReorderQueueRequest {
            track_ids: vec![TrackId(7), TrackId(8)],
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""trackIds":[7,8]"#), "got: {json}");

        // SetVolumeRequest — single `gain` field.
        let body = SetVolumeRequest { gain: 0.75 };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""gain":0.75"#), "got: {json}");

        // PlayRequest — wraps the externally-tagged AudioSource.
        let body = PlayRequest {
            source: AudioSource::LibraryPath {
                path: "lo-fi/a.mp3".into(),
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""kind":"libraryPath""#), "got: {json}");
    }

    #[test]
    fn bot_event_wire_uses_tagged_type_discriminator() {
        let ev = BotEventWire::StateChanged {
            from: BotState::Disconnected,
            to: BotState::Connecting,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"stateChanged""#), "got: {json}");
        assert!(json.contains(r#""from":"disconnected""#));
        assert!(json.contains(r#""to":"connecting""#));
    }

    #[test]
    fn progress_event_carries_elapsed_secs() {
        // `BotEventWire`'s enum-level `rename_all` camelCases the variant
        // *tag* (`"progress"`), not struct-variant fields — so `Progress`
        // keeps the snake_case `elapsed_secs` field, matching the
        // existing `Connected { client_id, default_channel }` shape.
        let ev = BotEventWire::Progress { elapsed_secs: 42 };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"progress""#), "got: {json}");
        assert!(json.contains(r#""elapsed_secs":42"#), "got: {json}");
        let back: BotEventWire = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, BotEventWire::Progress { elapsed_secs: 42 }));
    }

    #[test]
    fn music_bot_detail_now_playing_elapsed_is_camel_case_and_optional() {
        // Omitted when `None` — additive, old clients see no new key.
        let detail = MusicBotDetail {
            id: BotId(1),
            name: "DJ".into(),
            server_addr: "127.0.0.1:9987".into(),
            state: BotState::InChannel,
            now_playing: None,
            now_playing_elapsed_secs: None,
            queue: Vec::new(),
            channel_id: None,
            last_error: None,
        };
        let json = serde_json::to_string(&detail).unwrap();
        assert!(!json.contains("nowPlayingElapsedSecs"), "got: {json}");

        let detail = MusicBotDetail {
            now_playing_elapsed_secs: Some(12),
            ..detail
        };
        let json = serde_json::to_string(&detail).unwrap();
        assert!(
            json.contains(r#""nowPlayingElapsedSecs":12"#),
            "got: {json}"
        );
        let back: MusicBotDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back, detail);
    }
}
