//! Typed REST client for `/api/music-bots` and friends (PURA-124 WS-6).
//!
//! Wraps the music-bot REST surface published by WS-5 (PURA-123) so the
//! Dioxus pages don't sprinkle URL strings or `serde_json::Value` parsing
//! through their bodies. Every call goes through the shared
//! [`crate::client::api`] helpers so the single-flight refresh contract
//! holds.
//!
//! Wire types come straight from [`ts6_manager_shared::music_bots`] —
//! the FE never redefines a JSON shape.
//!
//! The SSE subscription (`use_bot_events`) lives in this module too so
//! every music-bot endpoint sits behind one path. It is wasm-only at
//! runtime — the native build returns a stream that never yields, which
//! lets SSR snapshot tests render bot pages without spawning browser
//! futures.

use std::sync::Arc;

use ts6_manager_shared::music_bots as wire;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

// ---- Bots ---------------------------------------------------------------

pub async fn list_bots(gate: Arc<RefreshGate>) -> Result<Vec<wire::MusicBotSummary>, ApiError> {
    api::authorized_get_json::<Vec<wire::MusicBotSummary>>(
        &gate,
        &api::api_base(),
        "/api/music-bots",
    )
    .await
}

pub async fn get_bot(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
) -> Result<wire::MusicBotDetail, ApiError> {
    let path = format!("/api/music-bots/{}", bot.0);
    api::authorized_get_json::<wire::MusicBotDetail>(&gate, &api::api_base(), &path).await
}

pub async fn create_bot(
    gate: Arc<RefreshGate>,
    body: &wire::CreateBotRequest,
) -> Result<wire::MusicBotSummary, ApiError> {
    api::authorized_post_json::<_, wire::MusicBotSummary>(
        &gate,
        &api::api_base(),
        "/api/music-bots",
        Some(body),
    )
    .await
}

pub async fn delete_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}", bot.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn connect_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/connect", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn disconnect_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/disconnect", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn join_channel(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    channel_id: u64,
) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/join", bot.0);
    let body = wire::JoinChannelRequest { channel_id };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

pub async fn leave_channel(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/leave", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

// ---- Library ------------------------------------------------------------

pub async fn list_library(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    tag: Option<&str>,
) -> Result<Vec<wire::LibraryEntry>, ApiError> {
    let mut path = format!("/api/music-library?bot={}", bot.0);
    if let Some(t) = tag {
        path.push_str("&tag=");
        path.push_str(&urlencoding::encode(t));
    }
    api::authorized_get_json::<Vec<wire::LibraryEntry>>(&gate, &api::api_base(), &path).await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AddLibraryBody<'a> {
    bot: wire::BotId,
    source: &'a wire::AudioSource,
    title: &'a str,
    tags: &'a [String],
}

pub async fn add_library_entry(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    source: &wire::AudioSource,
    title: &str,
    tags: &[String],
) -> Result<wire::LibraryEntry, ApiError> {
    let body = AddLibraryBody {
        bot,
        source,
        title,
        tags,
    };
    api::authorized_post_json::<_, wire::LibraryEntry>(
        &gate,
        &api::api_base(),
        "/api/music-library",
        Some(&body),
    )
    .await
}

pub async fn delete_library_entry(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    entry: wire::LibraryEntryId,
) -> Result<(), ApiError> {
    let path = format!("/api/music-library/{}?bot={}", entry.0, bot.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

// ---- Playlists ----------------------------------------------------------

pub async fn list_playlists(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
) -> Result<Vec<wire::PlaylistSummary>, ApiError> {
    let path = format!("/api/playlists?bot={}", bot.0);
    api::authorized_get_json::<Vec<wire::PlaylistSummary>>(&gate, &api::api_base(), &path).await
}

pub async fn create_playlist(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    name: &str,
) -> Result<wire::PlaylistSummary, ApiError> {
    let body = wire::CreatePlaylistRequest {
        bot,
        name: name.to_string(),
    };
    api::authorized_post_json::<_, wire::PlaylistSummary>(
        &gate,
        &api::api_base(),
        "/api/playlists",
        Some(&body),
    )
    .await
}

pub async fn get_playlist(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    name: &str,
) -> Result<wire::PlaylistDetail, ApiError> {
    let path = format!("/api/playlists/{}?bot={}", urlencoding::encode(name), bot.0);
    api::authorized_get_json::<wire::PlaylistDetail>(&gate, &api::api_base(), &path).await
}

pub async fn delete_playlist(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    name: &str,
) -> Result<(), ApiError> {
    let path = format!("/api/playlists/{}?bot={}", urlencoding::encode(name), bot.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AddTrackBody<'a> {
    bot: wire::BotId,
    source: &'a wire::AudioSource,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_by: Option<&'a str>,
}

pub async fn add_playlist_track(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    playlist: &str,
    source: &wire::AudioSource,
    title: &str,
) -> Result<wire::Track, ApiError> {
    let path = format!("/api/playlists/{}/tracks", urlencoding::encode(playlist));
    let body = AddTrackBody {
        bot,
        source,
        title,
        duration_secs: None,
        requested_by: None,
    };
    api::authorized_post_json::<_, wire::Track>(&gate, &api::api_base(), &path, Some(&body)).await
}

pub async fn remove_playlist_track(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    playlist: &str,
    track: wire::TrackId,
) -> Result<(), ApiError> {
    let path = format!(
        "/api/playlists/{}/tracks/{}?bot={}",
        urlencoding::encode(playlist),
        track.0,
        bot.0
    );
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn enqueue_playlist(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    playlist: &str,
) -> Result<wire::PlaylistDetail, ApiError> {
    let path = format!(
        "/api/playlists/{}/enqueue?bot={}",
        urlencoding::encode(playlist),
        bot.0
    );
    api::authorized_post_json::<(), wire::PlaylistDetail>(&gate, &api::api_base(), &path, None)
        .await
}

// ---- Radio stations -----------------------------------------------------

pub async fn list_radio_stations(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
) -> Result<Vec<wire::RadioStation>, ApiError> {
    let path = format!("/api/radio-stations?bot={}", bot.0);
    api::authorized_get_json::<Vec<wire::RadioStation>>(&gate, &api::api_base(), &path).await
}

pub async fn create_radio_station(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    source: wire::AudioSource,
    title: String,
    tags: Vec<String>,
) -> Result<wire::RadioStation, ApiError> {
    let body = wire::CreateRadioStationRequest {
        bot,
        source,
        title,
        tags,
    };
    api::authorized_post_json::<_, wire::RadioStation>(
        &gate,
        &api::api_base(),
        "/api/radio-stations",
        Some(&body),
    )
    .await
}

pub async fn delete_radio_station(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    entry: wire::LibraryEntryId,
) -> Result<(), ApiError> {
    let path = format!("/api/radio-stations/{}?bot={}", entry.0, bot.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn play_radio_station(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    entry: wire::LibraryEntryId,
) -> Result<(), ApiError> {
    let path = format!("/api/radio-stations/{}/play?bot={}", entry.0, bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

// ---- Audio control (PURA-126 WS-6 follow-up) ----------------------------

pub async fn play_source(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    source: wire::AudioSource,
) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/play", bot.0);
    let body = wire::PlayRequest { source };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

pub async fn pause_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/pause", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn resume_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/resume", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn stop_bot(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/stop", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn skip_next(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/skip-next", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn skip_prev(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/skip-prev", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

pub async fn set_volume(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    gain: f32,
) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/volume", bot.0);
    let body = wire::SetVolumeRequest { gain };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

/// PURA-352 — scrub the current track to `secs` seconds from its start.
/// Fire-and-forget: the audible jump + the reset progress clock arrive
/// via the SSE `Progress` event.
pub async fn seek(gate: Arc<RefreshGate>, bot: wire::BotId, secs: u64) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/seek", bot.0);
    let body = wire::SeekRequest { secs };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

// ---- Direct queue mutation (PURA-126 WS-6 follow-up) --------------------
//
// `enqueue_track` / `clear_queue` / `remove_queue_track` / `advance_queue`
// fire-and-forget — the actor minted track id arrives via the SSE
// `QueueChanged` event. `reorder_queue` returns the post-reorder snapshot
// for optimistic rendering after a drag gesture (the SSE event is still
// the authoritative live signal).

pub async fn enqueue_track(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    source: wire::AudioSource,
    title: String,
    duration_secs: Option<u64>,
    requested_by: Option<String>,
) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/queue", bot.0);
    let body = wire::EnqueueTrackRequest {
        source,
        title,
        duration_secs,
        requested_by,
    };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

pub async fn clear_queue(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/queue", bot.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn remove_queue_track(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    track: wire::TrackId,
) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/queue/{}", bot.0, track.0);
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn reorder_queue(
    gate: Arc<RefreshGate>,
    bot: wire::BotId,
    track_ids: Vec<wire::TrackId>,
) -> Result<Vec<wire::Track>, ApiError> {
    let path = format!("/api/music-bots/{}/queue/reorder", bot.0);
    let body = wire::ReorderQueueRequest { track_ids };
    api::authorized_post_json::<_, Vec<wire::Track>>(&gate, &api::api_base(), &path, Some(&body))
        .await
}

pub async fn advance_queue(gate: Arc<RefreshGate>, bot: wire::BotId) -> Result<(), ApiError> {
    let path = format!("/api/music-bots/{}/queue/advance", bot.0);
    api::authorized_post_json::<(), ()>(&gate, &api::api_base(), &path, None).await
}

// ---- SSE event stream ---------------------------------------------------

/// Subscribe to `/api/music-bots/{id}/events` and call `on_event` for
/// every parsed [`wire::BotEventWire`] payload. The returned struct is
/// dropped automatically when the calling component unmounts; that closes
/// the underlying `EventSource` and detaches the JS callback.
///
/// The browser's native `EventSource` already handles reconnect (with a
/// 3 s default backoff) and `Last-Event-ID` resume — the FE only deals
/// with the parsed event stream. The native build (SSR / native unit
/// tests) returns a no-op handle that never fires `on_event`.
#[cfg(target_arch = "wasm32")]
pub fn open_bot_event_source<F>(bot: wire::BotId, mut on_event: F) -> BotEventStream
where
    F: FnMut(wire::BotEventWire) + 'static,
{
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    use web_sys::{EventSource, MessageEvent};

    let url = format!("{}/api/music-bots/{}/events", api::api_base(), bot.0);
    let source = match EventSource::new(&url) {
        Ok(es) => es,
        Err(_) => return BotEventStream::default(),
    };
    let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |evt: MessageEvent| {
        let Ok(text) = evt.data().dyn_into::<js_sys::JsString>() else {
            return;
        };
        let raw: String = text.into();
        if let Ok(parsed) = serde_json::from_str::<wire::BotEventWire>(&raw) {
            on_event(parsed);
        }
    });
    source.set_onmessage(Some(cb.as_ref().unchecked_ref()));
    BotEventStream {
        source: Some(source),
        _cb: Some(cb),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn open_bot_event_source<F>(_bot: wire::BotId, _on_event: F) -> BotEventStream
where
    F: FnMut(wire::BotEventWire) + 'static,
{
    BotEventStream
}

/// Owns the live `EventSource` + the JS callback closure for the lifetime
/// of the page. Drop closes both.
#[cfg(target_arch = "wasm32")]
pub struct BotEventStream {
    source: Option<web_sys::EventSource>,
    _cb: Option<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>>,
}

#[cfg(target_arch = "wasm32")]
impl Default for BotEventStream {
    fn default() -> Self {
        Self {
            source: None,
            _cb: None,
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for BotEventStream {
    fn drop(&mut self) {
        if let Some(s) = self.source.take() {
            s.close();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct BotEventStream;
