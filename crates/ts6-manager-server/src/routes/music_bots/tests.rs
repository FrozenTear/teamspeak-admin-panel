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
use ts6_manager_shared::auth::{ErrorResponse, auth_error_strings as msg};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::{jwt, password};
use crate::db::{connect_in_memory, migrations};
use crate::music_bots::MusicBotService;
use crate::repos::{server_connections, users};

async fn fresh_state() -> AppState {
    let db = connect_in_memory().await.unwrap();
    migrations::run(&db).await.unwrap();
    crate::crypto::init("test-seed-pura-123");
    server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "local".into(),
            host: "127.0.0.1".into(),
            webqueryPort: 10080,
            apiKey: "enc:00:00:00".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .unwrap();
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
        yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
        yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
        data_dir: std::path::PathBuf::from("./data"),
        music_dir: std::path::PathBuf::from("/data/music"),
        proxy_trust: crate::web::proxy::ProxyTrust::direct(),
        bug_reports: crate::bug_reports::unconfigured_sink(),
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
    seed_user_role(state, username, "admin").await
}

async fn seed_user_role(state: &AppState, username: &str, role: &str) -> i64 {
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
            role: role.into(),
            enabled: true,
        },
    )
    .await
    .unwrap()
    .id
}

fn mint_token(state: &AppState, id: i64, username: &str) -> String {
    mint_token_role(state, id, username, "admin")
}

fn mint_token_role(state: &AppState, id: i64, username: &str, role: &str) -> String {
    jwt::mint_access(
        id,
        username,
        role,
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
async fn list_and_create_are_5xx_when_music_runtime_is_down() {
    let mut state = fresh_state().await;
    state.music_bots = MusicBotService::remote(
        std::env::temp_dir().join("ts6-test-music-bots-remote-down"),
        "http://127.0.0.1:1",
        None,
    );
    let uid = seed_user(&state, "tester-remote").await;
    let token = mint_token(&state, uid, "tester-remote");
    let app = app(state);

    let list = app
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
    assert_eq!(list.status(), StatusCode::BAD_GATEWAY);

    let create = app
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
    assert_eq!(create.status(), StatusCode::BAD_GATEWAY);
}

async fn create_test_bot(app: &Router, token: &str) -> wire::MusicBotSummary {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(token))
                .header("content-type", "application/json")
                .body(json_body(&create_bot_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    read_json(resp).await
}

fn events_uri(bot_id: u64, token: Option<&str>) -> String {
    match token {
        Some(t) => format!(
            "/api/music-bots/{bot_id}/events?token={}",
            urlencoding::encode(t)
        ),
        None => format!("/api/music-bots/{bot_id}/events"),
    }
}

/// SSE success responses are an infinite keep-alive stream — assert
/// status + content-type only; do not collect the body.
fn assert_sse_ok(resp: &axum::http::Response<Body>) {
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/event-stream"),
        "expected event-stream, got {ct:?}"
    );
}

#[tokio::test]
async fn events_sse_rejects_missing_auth() {
    let (app, token, _state) = make_test_app().await;
    let bot = create_test_bot(&app, &token).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(events_uri(bot.id.0, None))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: ErrorResponse = read_json(resp).await;
    assert_eq!(body.error, msg::NO_TOKEN);
}

#[tokio::test]
async fn events_sse_rejects_invalid_query_token() {
    let (app, token, _state) = make_test_app().await;
    let bot = create_test_bot(&app, &token).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(events_uri(bot.id.0, Some("not-a-jwt")))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: ErrorResponse = read_json(resp).await;
    assert_eq!(body.error, msg::INVALID_TOKEN);
}

#[tokio::test]
async fn events_sse_rejects_invalid_bearer() {
    let (app, token, _state) = make_test_app().await;
    let bot = create_test_bot(&app, &token).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(events_uri(bot.id.0, None))
                .header("authorization", auth_header("not-a-jwt"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: ErrorResponse = read_json(resp).await;
    assert_eq!(body.error, msg::INVALID_TOKEN);
}

#[tokio::test]
async fn events_sse_accepts_bearer() {
    let (app, token, _state) = make_test_app().await;
    let bot = create_test_bot(&app, &token).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(events_uri(bot.id.0, None))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_sse_ok(&resp);
}

#[tokio::test]
async fn events_sse_accepts_query_token() {
    let (app, token, _state) = make_test_app().await;
    let bot = create_test_bot(&app, &token).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(events_uri(bot.id.0, Some(&token)))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_sse_ok(&resp);
}

#[tokio::test]
async fn list_does_not_accept_query_token() {
    // Query-token auth is SSE-only. Other music-bot REST routes stay
    // Bearer-gated so access JWTs are not leaked onto arbitrary query strings.
    let (app, token, _state) = make_test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/music-bots?token={}",
                    urlencoding::encode(&token)
                ))
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
async fn audio_play_rejects_private_url_before_dispatch() {
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
            url: "http://127.0.0.1/secret".into(),
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
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err: wire::ErrorBody = read_json(resp).await;
    assert_eq!(err.code.as_deref(), Some("ssrf_blocked"));

    let resp = app
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
        requests.is_empty(),
        "rejected play must not write a request-log row"
    );
}

#[tokio::test]
async fn audio_play_rejects_library_path_escape() {
    let root = std::env::temp_dir().join(format!("ts6-music-play-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ok.mp3"), b"x").unwrap();

    let mut state = fresh_state().await;
    state.music_dir = root.clone();
    let uid = seed_user(&state, "jail-tester").await;
    let token = mint_token(&state, uid, "jail-tester");
    let app = app(state);

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

    let escape = wire::PlayRequest {
        source: wire::AudioSource::LibraryPath {
            path: "../ok.mp3".into(),
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
                .body(json_body(&escape))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let ok = wire::PlayRequest {
        source: wire::AudioSource::LibraryPath {
            path: "./ok.mp3".into(),
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
                .body(json_body(&ok))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The request log stores the path the play command carried: the
    // canonical file under MUSIC_DIR, not the raw `./ok.mp3`.
    let resp = app
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
    let canon = root.canonicalize().unwrap().join("ok.mp3");
    match &requests[0].source {
        wire::AudioSource::LibraryPath { path } => {
            assert_eq!(path, &canon.to_string_lossy());
            assert_ne!(path, "./ok.mp3");
        }
        other => panic!("expected library path, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// `http://203.0.113.10\@127.0.0.1/a.mp3` passes the WHATWG gate (host
/// `203.0.113.10`) and is dialed as `127.0.0.1` by ffmpeg if the raw
/// string is forwarded. The play route must hand the runtime the
/// serialized URL, and must still reject a form whose parsed host is
/// loopback.
#[tokio::test]
async fn audio_play_dispatches_normalized_url_and_still_blocks_loopback() {
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

    let raw = "http://203.0.113.10\\@127.0.0.1/a.mp3";
    let body = wire::PlayRequest {
        source: wire::AudioSource::Url { url: raw.into() },
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
    let expected = "http://203.0.113.10/@127.0.0.1/a.mp3";
    match &requests[0].source {
        wire::AudioSource::Url { url } => {
            assert_eq!(url, expected);
            assert!(!url.contains('\\'));
        }
        other => panic!("expected url, got {other:?}"),
    }
    assert_eq!(requests[0].title, expected);

    // Encoded backslash stays in userinfo; parsed host is loopback.
    let blocked = wire::PlayRequest {
        source: wire::AudioSource::Url {
            url: "http://203.0.113.10%5c@127.0.0.1/a.mp3".into(),
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
                .body(json_body(&blocked))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err: wire::ErrorBody = read_json(resp).await;
    assert_eq!(err.code.as_deref(), Some("ssrf_blocked"));

    let resp = app
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
    assert_eq!(
        requests.len(),
        1,
        "a blocked follow-up play must not append a request-log row"
    );
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

#[tokio::test]
async fn bug_report_context_requires_auth() {
    let (app, _token, _state) = make_test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots/bug-report-context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bug_report_context_returns_camel_case_snapshot() {
    let _guard = music_bot::bug_report::test_global_lock().await;
    music_bot::bug_report::global_ring().clear();
    music_bot::bug_report::global_ring().record_stage(
        music_bot::bug_report::LatencyStage {
            stage: "first_frame_on_wire".into(),
            elapsed_ms: Some(105),
            retry: false,
        },
        "music_bot_latency stage=first_frame_on_wire elapsed_ms=105",
    );

    let (app, token, _state) = make_test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots/bug-report-context")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let snap: wire::MusicBotBugReportContext = read_json(resp).await;
    assert!(
        snap.music_bot_latency
            .contains("first_frame_on_wire elapsed_ms=105 retry=0"),
        "{}",
        snap.music_bot_latency
    );
    assert!(
        snap.log_tail.contains("stage=first_frame_on_wire"),
        "{}",
        snap.log_tail
    );
}

#[tokio::test]
async fn bug_report_post_middleware_merges_absent_context_keys() {
    use crate::routes::music_bots::enrich_bug_report_request;
    use axum::Json;
    use axum::routing::post;
    use serde_json::Value;

    let _guard = music_bot::bug_report::test_global_lock().await;
    music_bot::bug_report::global_ring().clear();
    music_bot::bug_report::global_ring().record_stage(
        music_bot::bug_report::LatencyStage {
            stage: "resolver_warm_retry".into(),
            elapsed_ms: None,
            retry: true,
        },
        "music_bot_latency stage=resolver_warm_retry retry=1",
    );

    async fn echo(Json(body): Json<Value>) -> Json<Value> {
        Json(body)
    }

    let state = fresh_state().await;
    let app = Router::new()
        .route("/api/bug-reports", post(echo))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state,
            enrich_bug_report_request,
        ));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/bug-reports")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"pagePath":"/music-bots/42"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = read_json(resp).await;
    assert_eq!(body["pagePath"], "/music-bots/42");
    let latency = body["context"]["musicBotLatency"].as_str().unwrap();
    assert!(
        latency.contains("resolver_warm_retry elapsed_ms=- retry=1"),
        "{latency}"
    );
}

#[tokio::test]
async fn create_requires_moderator_grant_and_ignores_identity_path() {
    let state = fresh_state().await;
    let server = server_connections::list(&state.db).await.unwrap();
    let server_id = server[0].id;

    let viewer_id = seed_user_role(&state, "viewer", "viewer").await;
    let viewer_token = mint_token_role(&state, viewer_id, "viewer", "viewer");
    let mod_id = seed_user_role(&state, "mod", "moderator").await;
    let mod_token = mint_token_role(&state, mod_id, "mod", "moderator");
    let app = app(state.clone());

    let mut body = create_bot_body();
    body.identity_path = Some("/etc/passwd".into());

    let viewer = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&viewer_token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);

    let ungranted = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&mod_token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ungranted.status(), StatusCode::FORBIDDEN);

    crate::repos::server_user_grants::insert(&state.db, mod_id, server_id)
        .await
        .unwrap();
    let created = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&mod_token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let rows = crate::repos::music_bot_runtime::list(&state.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].identityPath.contains("bot-") && rows[0].identityPath.ends_with(".identity"),
        "{}",
        rows[0].identityPath
    );
    assert!(!rows[0].identityPath.contains("passwd"));
}

const RUNTIME_TOKEN: &str = "browser-must-not-see-this-token";

#[derive(Clone, Copy)]
enum RuntimeAuthMock {
    /// Every runtime route answers 401.
    Deny,
    /// `GET /v1/bots` returns one bot. Every other route answers 401.
    ListOpen,
}

async fn auth_runtime(mode: RuntimeAuthMock) -> String {
    use axum::extract::{Request, State};
    use axum::response::IntoResponse;
    let app = axum::Router::new()
        .fallback(
            |State(mode): State<RuntimeAuthMock>, req: Request| async move {
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                if matches!(mode, RuntimeAuthMock::ListOpen)
                    && method == Method::GET
                    && path == "/v1/bots"
                {
                    return (
                        StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "bots": [{
                                "id": 1,
                                "name": "t",
                                "server_addr": "127.0.0.1:9987"
                            }]
                        })),
                    )
                        .into_response();
                }
                StatusCode::UNAUTHORIZED.into_response()
            },
        )
        .with_state(mode);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn assert_runtime_auth(status: StatusCode, body: &str) {
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, r#"{"error":"music_runtime_auth"}"#);
    assert!(!body.contains(RUNTIME_TOKEN), "{body}");
}

#[tokio::test]
async fn runtime_401_on_list_events_and_store_is_browser_502() {
    let url = auth_runtime(RuntimeAuthMock::Deny).await;
    let mut state = fresh_state().await;
    state.music_bots = MusicBotService::remote(
        std::env::temp_dir().join("ts6-test-music-runtime-auth"),
        url,
        crate::music_runtime::MusicRuntimeToken::parse(RUNTIME_TOKEN),
    );
    let uid = seed_user(&state, "auth-tester").await;
    let token = mint_token(&state, uid, "auth-tester");
    let app = app(state);

    let list = app
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
    let list_status = list.status();
    assert_runtime_auth(list_status, &body_string(list).await);

    let events = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots/1/events")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let events_status = events.status();
    assert_runtime_auth(events_status, &body_string(events).await);

    let playlists = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/playlists?bot=1")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let playlists_status = playlists.status();
    assert_runtime_auth(playlists_status, &body_string(playlists).await);
}

#[tokio::test]
async fn runtime_401_on_command_and_now_playing_is_browser_502() {
    let url = auth_runtime(RuntimeAuthMock::ListOpen).await;
    let mut state = fresh_state().await;
    state.music_bots = MusicBotService::remote(
        std::env::temp_dir().join("ts6-test-music-runtime-auth-cmd"),
        url,
        crate::music_runtime::MusicRuntimeToken::parse(RUNTIME_TOKEN),
    );
    let uid = seed_user(&state, "auth-cmd").await;
    let token = mint_token(&state, uid, "auth-cmd");
    let app = app(state);

    let connect = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots/1/connect")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let connect_status = connect.status();
    assert_runtime_auth(connect_status, &body_string(connect).await);

    let detail = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots/1")
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail_status = detail.status();
    assert_runtime_auth(detail_status, &body_string(detail).await);
}

#[tokio::test]
async fn runtime_auth_mapping_uses_http_status_not_message_text() {
    use crate::music_runtime::{FrontSendError, FrontStoreError, MusicRuntimeError};
    use crate::routes::music_bots::{
        map_music_runtime_error, translate_send_error, translate_store_error,
    };
    use music_bot::StoreError;

    let from_status =
        translate_store_error(FrontStoreError::Unauthorized(StatusCode::UNAUTHORIZED));
    let from_status_code = from_status.status();
    assert_runtime_auth(from_status_code, &body_string(from_status).await);

    let decoy = translate_store_error(FrontStoreError::Store(StoreError::Backend(
        "music_runtime_auth".into(),
    )));
    let decoy_status = decoy.status();
    let decoy_body = body_string(decoy).await;
    assert_ne!(decoy_status, StatusCode::BAD_GATEWAY);
    assert_ne!(decoy_body, r#"{"error":"music_runtime_auth"}"#);
    assert!(
        decoy_body.contains("music_runtime_auth"),
        "the message text is still a backend error, not the auth mapping: {decoy_body}"
    );

    let other_status = translate_store_error(FrontStoreError::Unauthorized(StatusCode::FORBIDDEN));
    let other_body = body_string(other_status).await;
    assert_ne!(other_body, r#"{"error":"music_runtime_auth"}"#);

    let runtime =
        map_music_runtime_error(MusicRuntimeError::Unauthorized(StatusCode::UNAUTHORIZED));
    let runtime_status = runtime.status();
    assert_runtime_auth(runtime_status, &body_string(runtime).await);

    let runtime_text =
        map_music_runtime_error(MusicRuntimeError::Unavailable("music_runtime_auth".into()));
    let runtime_text_body = body_string(runtime_text).await;
    assert_ne!(runtime_text_body, r#"{"error":"music_runtime_auth"}"#);

    let command = translate_send_error(FrontSendError::Unauthorized(StatusCode::UNAUTHORIZED));
    let command_status = command.status();
    assert_runtime_auth(command_status, &body_string(command).await);

    let command_other = translate_send_error(FrontSendError::Unauthorized(StatusCode::FORBIDDEN));
    let command_other_body = body_string(command_other).await;
    assert_ne!(command_other_body, r#"{"error":"music_runtime_auth"}"#);
}
