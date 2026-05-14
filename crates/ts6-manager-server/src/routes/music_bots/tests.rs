//! Integration tests for the music-bot REST surface (PURA-123 WS-5).
//!
//! Hits every endpoint via `tower::ServiceExt::oneshot` against an
//! `AppState` literal — no network sockets, no SurrealDB outside of the
//! existing `connect_in_memory` fixture. Bots created here use
//! `auto_connect: false` so the supervisor doesn't try to dial a TS6
//! server during the test run; lifecycle commands assert the dispatch
//! reached the actor (the actor logs + the broadcast channel are the
//! source of truth — we don't need a live tsclientlib handshake here).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::{jwt, password};
use crate::db::{connect_in_memory, migrations};
use crate::music_bots::MusicBotService;
use crate::repos::users;

async fn fresh_state() -> AppState {
    let db = connect_in_memory().await.unwrap();
    migrations::run(&db).await.unwrap();
    crate::crypto::init("test-seed-pura-123");
    let control = crate::control::ControlBackendPool::new(false, db.clone());
    AppState {
        db,
        jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
        jwt_access_expiry: Duration::from_secs(900),
        jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
        setup_lock: Arc::new(Mutex::new(())),
        webquery: crate::webquery::WebQueryPool::new(false),
        control,
        ws_hub: crate::ws::Hub::new(),
        widget_cache: crate::widgets::WidgetCache::new(),
        music_bots: MusicBotService::default_for_tests(),
        sidecar: None,
        ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
        moq_public_url: None,
    }
}

fn app(state: AppState) -> Router {
    Router::new().merge(super::router()).with_state(state)
}

fn json_body<T: serde::Serialize>(value: &T) -> Body {
    Body::from(serde_json::to_vec(value).unwrap())
}

async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "expected JSON, got {:?}: {e}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

async fn seed_user(state: &AppState, username: &str) -> i64 {
    let pw = "Hunter2!ok".to_string();
    let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
        .await
        .unwrap()
        .unwrap();
    users::insert(
        &state.db,
        users::NewUser {
            username: username.into(),
            passwordHash: hash,
            displayName: username.into(),
            role: "admin".into(),
            enabled: true,
        },
    )
    .await
    .unwrap()
    .id
}

fn mint_token(state: &AppState, id: i64, username: &str) -> String {
    jwt::mint_access(
        id,
        username,
        "admin",
        state.jwt_access_expiry,
        &state.jwt_secret,
    )
    .unwrap()
}

fn auth_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

async fn make_test_app() -> (Router, String, AppState) {
    let state = fresh_state().await;
    let uid = seed_user(&state, "tester").await;
    let token = mint_token(&state, uid, "tester");
    (app(state.clone()), token, state)
}

fn create_bot_body() -> wire::CreateBotRequest {
    wire::CreateBotRequest {
        name: "DJ-Bot".into(),
        server_addr: "127.0.0.1:9987".into(),
        identity_path: None,
        // Avoid kicking off a real handshake — the supervisor still
        // spawns the actor but it sits in `Disconnected` waiting for
        // a `Connect` command we never send.
        auto_connect: Some(false),
    }
}

#[tokio::test]
async fn list_requires_auth() {
    let (app, _token, _state) = make_test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_bot_then_list_and_detail() {
    let (app, token, _state) = make_test_app().await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let summary: wire::MusicBotSummary = read_json(resp).await;
    assert_eq!(summary.name, "DJ-Bot");
    assert_eq!(summary.server_addr, "127.0.0.1:9987");
    let bot_id = summary.id;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<wire::MusicBotSummary> = read_json(resp).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, bot_id);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", bot_id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail: wire::MusicBotDetail = read_json(resp).await;
    assert_eq!(detail.id, bot_id);
    assert!(detail.queue.is_empty());
}

#[tokio::test]
async fn validation_error_envelope_uses_camel_case() {
    let (app, token, _state) = make_test_app().await;

    let body = wire::CreateBotRequest {
        name: "".into(),
        server_addr: "127.0.0.1:9987".into(),
        identity_path: None,
        auto_connect: Some(false),
    };
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let envelope: wire::ErrorBody = read_json(resp).await;
    assert_eq!(envelope.code.as_deref(), Some("validation"));
    assert!(envelope.error.contains("name"));
}

#[tokio::test]
async fn shutdown_returns_204_then_404() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Second shutdown 404s — bot is gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lifecycle_commands_dispatch_when_bot_exists() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    for path in ["connect", "disconnect", "leave"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/music-bots/{}/{}", bot.id.0, path))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "{path} dispatch should be accepted"
        );
    }

    // Join needs a body.
    let join = wire::JoinChannelRequest { channel_id: 7 };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/join", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&join))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn lifecycle_command_404s_for_unknown_bot() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots/9999/connect")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn library_crud_round_trip() {
    let (app, token, _state) = make_test_app().await;
    // Need a bot id (library is per-bot).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Add an entry.
    let body = serde_json::json!({
        "bot": bot.id,
        "source": { "kind": "url", "url": "https://example.com/lofi.mp3" },
        "title": "lofi-1",
        "tags": ["chill"],
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-library")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let entry: wire::LibraryEntry = read_json(resp).await;
    assert_eq!(entry.title, "lofi-1");

    // List filters by tag.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-library?bot={}&tag=chill", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<wire::LibraryEntry> = read_json(resp).await;
    assert_eq!(list.len(), 1);

    // Patch (rename + retag).
    let patch_body = serde_json::json!({
        "bot": bot.id,
        "title": "lofi-renamed",
        "tags": ["chill", "instrumental"],
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/api/music-library/{}", entry.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: wire::LibraryEntry = read_json(resp).await;
    assert_eq!(updated.title, "lofi-renamed");

    // Delete.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!(
                    "/api/music-library/{}?bot={}",
                    updated.id.0, bot.id.0
                ))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn playlist_crud_and_track_ops() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Create playlist.
    let req = wire::CreatePlaylistRequest {
        bot: bot.id,
        name: "lo-fi-radio".into(),
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/playlists")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&req))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Add a track.
    let body = serde_json::json!({
        "bot": bot.id,
        "source": { "kind": "url", "url": "https://example.com/a.mp3" },
        "title": "a",
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/playlists/lo-fi-radio/tracks")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let track: wire::Track = read_json(resp).await;

    // Detail.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/playlists/lo-fi-radio?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail: wire::PlaylistDetail = read_json(resp).await;
    assert_eq!(detail.tracks.len(), 1);

    // Remove track.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!(
                    "/api/playlists/lo-fi-radio/tracks/{}?bot={}",
                    track.id.0, bot.id.0
                ))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Rename.
    let body = wire::PatchPlaylistRequest {
        new_name: Some("lofi-renamed".into()),
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/api/playlists/lo-fi-radio?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/playlists/lofi-renamed?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn radio_station_create_list_play_log() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Create a radio station.
    let body = wire::CreateRadioStationRequest {
        bot: bot.id,
        source: wire::AudioSource::Url {
            url: "https://radio.example.com/stream".into(),
        },
        title: "lo-fi-radio".into(),
        tags: vec!["chill".into()],
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/radio-stations")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let station: wire::RadioStation = read_json(resp).await;
    assert!(station.tags.iter().any(|t| t == wire::RADIO_TAG));

    // List shows it under /radio-stations.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/radio-stations?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list: Vec<wire::RadioStation> = read_json(resp).await;
    assert_eq!(list.len(), 1);

    // Play (lifecycle dispatch).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/radio-stations/{}/play?bot={}",
                    station.id.0, bot.id.0
                ))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Request log has a row with track_id: None (radio play bypasses
    // the queue).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-requests?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let requests: Vec<wire::MusicRequest> = read_json(resp).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].track_id.is_none());
    assert_eq!(requests[0].title, "lo-fi-radio");
}

#[tokio::test]
async fn radio_station_delete_404s_on_non_radio_library_entry() {
    let (app, token, state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Insert a plain library entry (no RADIO_TAG) directly — the
    // /radio-stations DELETE route must refuse to delete it.
    let entry = state
        .music_bots
        .supervisor
        .library_add(
            music_bot::BotId(bot.id.0),
            music_bot::NewLibraryEntry {
                source: music_bot::AudioSource::Url("https://x".into()),
                title: "regular".into(),
                tags: vec!["chill".into()],
            },
        )
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!(
                    "/api/radio-stations/{}?bot={}",
                    entry.id.0, bot.id.0
                ))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn music_requests_filter_by_bot_returns_only_that_bot() {
    let (app, token, state) = make_test_app().await;
    let resp_a = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot_a: wire::MusicBotSummary = read_json(resp_a).await;
    let resp_b = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&wire::CreateBotRequest {
                    name: "Bot-B".into(),
                    server_addr: "127.0.0.1:9988".into(),
                    identity_path: None,
                    auto_connect: Some(false),
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot_b: wire::MusicBotSummary = read_json(resp_b).await;

    // Hand-write request log rows for both bots.
    let now = chrono::Utc::now();
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: bot_a.id,
            track_id: None,
            source: wire::AudioSource::Url {
                url: "https://example.com/a".into(),
            },
            title: "a".into(),
            requested_by: Some("alice".into()),
            requested_at: now,
        })
        .await;
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: bot_b.id,
            track_id: None,
            source: wire::AudioSource::Url {
                url: "https://example.com/b".into(),
            },
            title: "b".into(),
            requested_by: Some("bob".into()),
            requested_at: now,
        })
        .await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-requests?bot={}", bot_a.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let requests: Vec<wire::MusicRequest> = read_json(resp).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].bot, bot_a.id);
}

/// E2E from the issue acceptance criteria:
/// `POST /music-bots` → `POST /{id}/join` → playlist enqueue →
/// `GET /music-bots/{id}` observes `state` ≠ disconnected and a
/// non-empty queue.
///
/// We can't drive the audio task to `nowPlaying` without a live
/// tsclientlib handshake (out of scope for the in-process test
/// harness — that's the lifecycle-e2e test in the music-bot crate
/// itself and the `ts6-voice-fixture::audio_e2e` integration test).
/// The wire-level acceptance therefore checks the queue contents and
/// dispatch surface, not the audio side; the issue calls out
/// `nowPlaying` as a forward expectation that WS-2 fulfils.
#[tokio::test]
async fn e2e_create_join_enqueue_observes_queue() {
    let (app, token, _state) = make_test_app().await;

    // 1) POST /music-bots.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // 2) POST /{id}/join — accepted dispatch.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/join", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&wire::JoinChannelRequest { channel_id: 7 }))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // 3) Create a playlist + add a track.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/playlists")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&wire::CreatePlaylistRequest {
                    bot: bot.id,
                    name: "set".into(),
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = serde_json::json!({
        "bot": bot.id,
        "source": { "kind": "url", "url": "https://example.com/song.mp3" },
        "title": "Song",
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/playlists/set/tracks")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // 4) POST /playlists/{name}/enqueue?bot={id}.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/playlists/set/enqueue?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Tiny pause to let the supervisor hand the EnqueuePlaylist command
    // to the actor; the actor's QueueChanged event is what populates
    // `now_playing`. We don't strictly need it for the queue assertion
    // (the store is mutated by the dispatcher when the actor processes
    // the command), so we yield once and re-poll.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5) GET /music-bots/{id} — observe a non-empty queue.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail: wire::MusicBotDetail = read_json(resp).await;
    assert_eq!(
        detail.queue.len(),
        1,
        "queue should hold the enqueued track"
    );
    assert_eq!(detail.queue[0].title, "Song");

    // Request log captured the enqueue.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-requests?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let requests: Vec<wire::MusicRequest> = read_json(resp).await;
    assert!(
        !requests.is_empty(),
        "playlist enqueue must populate the request log"
    );
}

// ----------------------------------------------------------------------
// PURA-126 WS-6 follow-up — audio-control + direct-queue dispatch tests.
//
// Mirrors the WS-5 coverage: 404 on unknown bot, success path on a
// spawned bot, request-log row on `enqueue` + `play`. We don't assert
// audio-stack behaviour (WS-1 logs the audio commands; the audio
// pipeline itself is WS-2's lifecycle-e2e turf) — these tests only
// prove the REST → BotSupervisor wiring is correct.
// ----------------------------------------------------------------------

#[tokio::test]
async fn audio_control_dispatch_returns_202_for_each_route() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // No-body audio routes — pause / resume / stop / skip-next / skip-prev.
    for path in ["pause", "resume", "stop", "skip-next", "skip-prev"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/music-bots/{}/{}", bot.id.0, path))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "{path} dispatch should be accepted"
        );
    }

    // Volume — body required.
    let body = wire::SetVolumeRequest { gain: 0.5 };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/volume", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn audio_control_404s_for_unknown_bot() {
    let (app, token, _state) = make_test_app().await;
    for path in ["pause", "resume", "stop", "skip-next", "skip-prev"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/music-bots/9999/{}", path))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} should 404 for unknown bot"
        );
    }
}

#[tokio::test]
async fn audio_play_writes_request_log_row() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    let body = wire::PlayRequest {
        source: wire::AudioSource::Url {
            url: "https://example.com/song.mp3".into(),
        },
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/play", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Request log row exists, `track_id` is None (queue bypassed),
    // `title` falls back to the source URL.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-requests?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let requests: Vec<wire::MusicRequest> = read_json(resp).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].track_id.is_none());
    assert_eq!(requests[0].title, "https://example.com/song.mp3");
}

#[tokio::test]
async fn queue_dispatch_routes_return_202_and_404() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Clear (no body).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}/queue", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Remove a (non-existent) track id — actor processes it as a no-op,
    // dispatch surface still returns 202.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}/queue/42", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Advance.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/queue/advance", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // 404 path — clear on unknown bot.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/music-bots/9999/queue")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn queue_enqueue_writes_request_log_row() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    let body = wire::EnqueueTrackRequest {
        source: wire::AudioSource::Url {
            url: "https://example.com/track.mp3".into(),
        },
        title: "Direct Track".into(),
        duration_secs: Some(180),
        requested_by: Some("alice".into()),
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/queue", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Request log row recorded.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-requests?bot={}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let requests: Vec<wire::MusicRequest> = read_json(resp).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].title, "Direct Track");
    assert_eq!(requests[0].requested_by.as_deref(), Some("alice"));

    // Yield once for the actor to process the dispatched Enqueue, then
    // verify the queue holds the new track via the bot detail endpoint.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail: wire::MusicBotDetail = read_json(resp).await;
    assert_eq!(detail.queue.len(), 1);
    assert_eq!(detail.queue[0].title, "Direct Track");
}

#[tokio::test]
async fn queue_reorder_returns_snapshot() {
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bot: wire::MusicBotSummary = read_json(resp).await;

    // Enqueue two tracks via the new queue route — same dispatch path
    // the FE uses, so the test exercises the full chain.
    for (i, title) in [(1, "A"), (2, "B")] {
        let body = wire::EnqueueTrackRequest {
            source: wire::AudioSource::Url {
                url: format!("https://example.com/{i}.mp3"),
            },
            title: title.into(),
            duration_secs: None,
            requested_by: None,
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/music-bots/{}/queue", bot.id.0))
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(json_body(&body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    // Wait for both Enqueue dispatches to drain through the actor.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Read the current queue to learn the minted ids, then reverse it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", bot.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail: wire::MusicBotDetail = read_json(resp).await;
    assert_eq!(detail.queue.len(), 2);
    let mut reversed: Vec<wire::TrackId> = detail.queue.iter().map(|t| t.id).collect();
    reversed.reverse();

    let body = wire::ReorderQueueRequest {
        track_ids: reversed.clone(),
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/queue/reorder", bot.id.0))
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let snapshot: Vec<wire::Track> = read_json(resp).await;
    assert_eq!(snapshot.len(), 2);
    let ids: Vec<wire::TrackId> = snapshot.iter().map(|t| t.id).collect();
    assert_eq!(ids, reversed, "reorder should return the new order");
}
