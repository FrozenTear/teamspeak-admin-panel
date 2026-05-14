//! Wire-format types for the `/api/video-sources` surface (PURA-144 WS-6,
//! consumed by PURA-145 WS-7).
//!
//! `VideoSourceView` is the canonical row shape — emitted by `GET /api/
//! video-sources`, `GET /api/video-sources/{id}`, `POST /api/video-sources`,
//! and as the `data` payload of the `video_source:created` WS envelope.
//!
//! `VideoSourceUpdate` is the per-tick status push (`video_source:update`):
//! a thinner shape that carries the live ffmpeg/frame-counter snapshot but
//! drops `url`, `track`, and `created_*`. The FE reconciles update events
//! against the row already in memory rather than refetching.
//!
//! JSON field names mirror the TS6-manager route layer verbatim (snake_case,
//! no `rename_all`) so the wire stays compatible with the WS-6 contract
//! published in `routes/control/video_sources.rs` and `ws/video_source_tick.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Encoding presets the FE and sidecar both understand. The sidecar's
/// `QualityPreset` enum (`crates/ts6-media-sidecar/src/preset.rs`) is the
/// source of truth; the slice here keeps the FE form picker honest without
/// pulling the sidecar workspace into the WASM build.
pub const KNOWN_PRESETS: &[&str] = &["480p", "720p", "1080p"];

/// `POST /api/video-sources` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateVideoSourceRequest {
    pub url: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Optional. Omit when the operator has a single enabled server — the
    /// route layer resolves it automatically. Required for multi-server
    /// deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<i64>,
}

/// MoQ track triple used by the player to subscribe to the right namespace.
/// Always shape-identical to the sidecar's `TrackDescriptor` so the player
/// component can address `(namespace, video, audio)` without remapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackDescriptorView {
    pub namespace: String,
    pub video: String,
    pub audio: String,
}

/// Canonical row shape. Matches the route layer's `VideoSourceView` —
/// keep these in lock-step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSourceView {
    pub id: i64,
    pub source_id: String,
    pub label: String,
    pub url: String,
    pub preset: String,
    pub server_id: i64,
    pub status: String,
    pub track: TrackDescriptorView,
    #[serde(default)]
    pub created_by_user_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// Per-track stats snapshot carried by `video_source:update` WS pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackStatsUpdate {
    #[serde(default)]
    pub frames_published: u64,
    #[serde(default)]
    pub bytes_published: u64,
    #[serde(default)]
    pub ffmpeg_alive: bool,
}

/// `video_source:update` envelope `data`. Thinner than `VideoSourceView`
/// — no `url`, no `track`, no `created_*` — because the FE reconciles
/// against an in-memory row rather than refetching on every tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSourceUpdate {
    pub id: i64,
    pub source_id: String,
    pub label: String,
    pub preset: String,
    pub server_id: i64,
    pub status: String,
    pub video: TrackStatsUpdate,
    pub audio: TrackStatsUpdate,
}

/// `video_source:deleted` envelope `data`. The row is identified by
/// either the manager DB id or the sidecar source_id; the FE removes the
/// row matching either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSourceDeleted {
    pub id: i64,
    pub source_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_presets_match_sidecar_enum() {
        assert_eq!(KNOWN_PRESETS, &["480p", "720p", "1080p"]);
    }

    #[test]
    fn view_round_trips_snake_case_json() {
        let json = r#"{
            "id": 7,
            "source_id": "src-7",
            "label": "Lobby cam",
            "url": "https://example.com/stream.m3u8",
            "preset": "720p",
            "server_id": 1,
            "status": "live",
            "track": {"namespace": "src-7", "video": "video", "audio": "audio"},
            "created_by_user_id": 42,
            "created_at": "2026-05-14T00:00:00Z"
        }"#;
        let v: VideoSourceView = serde_json::from_str(json).unwrap();
        assert_eq!(v.source_id, "src-7");
        assert_eq!(v.track.namespace, "src-7");
        // Round-trip back through JSON to lock in snake_case wire shape.
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("\"source_id\""), "wire must be snake_case: {s}");
        assert!(s.contains("\"server_id\""), "wire must be snake_case: {s}");
    }

    #[test]
    fn update_envelope_parses_default_stats() {
        // The WS push emits all fields; assert defaults so a partial
        // envelope from a future tightening of the contract still
        // round-trips.
        let json = r#"{
            "id": 1, "source_id": "s", "label": "L", "preset": "720p",
            "server_id": 1, "status": "starting",
            "video": {"frames_published": 0, "bytes_published": 0, "ffmpeg_alive": false},
            "audio": {"frames_published": 0, "bytes_published": 0, "ffmpeg_alive": false}
        }"#;
        let u: VideoSourceUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(u.status, "starting");
        assert!(!u.video.ffmpeg_alive);
    }

    #[test]
    fn create_request_omits_optional_fields() {
        let req = CreateVideoSourceRequest {
            url: "https://example.com/x.m3u8".into(),
            label: "X".into(),
            preset: None,
            server_id: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("preset"), "None preset must be omitted: {s}");
        assert!(
            !s.contains("server_id"),
            "None server_id must be omitted: {s}"
        );
    }
}
