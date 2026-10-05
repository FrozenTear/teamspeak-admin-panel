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
        watch: crate::watch::Store::new(),
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

async fn get_bot_list(app: &Router, token: &str) -> Vec<wire::MusicBotSummary> {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-bots")
                .header("authorization", auth_header(token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    read_json(resp).await
}

/// An admin sees a bot whose address matches no enabled server, flagged
/// orphaned, and can stop it through DELETE. A moderator with a grant
/// on the real server still sees the bound bot and does not see the
/// orphan. A moderator with no grant sees neither.
#[tokio::test]
async fn admin_sees_and_stops_orphaned_bot_non_admin_does_not() {
    let state = fresh_state().await;
    let server_id = server_connections::list(&state.db).await.unwrap()[0].id;

    let admin_id = seed_user_role(&state, "admin-orphan", "admin").await;
    let admin_token = mint_token_role(&state, admin_id, "admin-orphan", "admin");
    let mod_id = seed_user_role(&state, "mod-orphan", "moderator").await;
    let mod_token = mint_token_role(&state, mod_id, "mod-orphan", "moderator");
    crate::repos::server_user_grants::insert(&state.db, mod_id, server_id)
        .await
        .unwrap();
    let stranger_id = seed_user_role(&state, "stranger-orphan", "moderator").await;
    let stranger_token = mint_token_role(&state, stranger_id, "stranger-orphan", "moderator");

    let cookie = state.yt_cookie.clone();
    let api_key = state.yt_api_key.clone();
    let bound = music_bot::BotConfig::new(
        "bound",
        std::env::temp_dir().join("ts6-bound-orphan.identity"),
    )
    .with_server_addr("127.0.0.1:9987")
    .with_auto_connect(false);
    state
        .music_bots
        .supervisor
        .spawn(bound, cookie.clone(), api_key.clone())
        .await
        .unwrap();
    let orphan = music_bot::BotConfig::new(
        "orphan",
        std::env::temp_dir().join("ts6-unbound-orphan.identity"),
    )
    .with_server_addr("10.255.255.1:9987")
    .with_auto_connect(false);
    let orphan_id = state
        .music_bots
        .supervisor
        .spawn(orphan, cookie, api_key)
        .await
        .unwrap();

    let app = app(state.clone());

    let admin_rows = get_bot_list(&app, &admin_token).await;
    assert_eq!(admin_rows.len(), 2);
    let admin_bound = admin_rows.iter().find(|b| b.name == "bound").unwrap();
    let admin_orphan = admin_rows.iter().find(|b| b.name == "orphan").unwrap();
    assert!(!admin_bound.orphaned);
    assert!(admin_orphan.orphaned);
    assert_eq!(admin_orphan.id.0, orphan_id.0);
    assert_eq!(admin_orphan.server_addr, "10.255.255.1:9987");

    let mod_rows = get_bot_list(&app, &mod_token).await;
    assert_eq!(mod_rows.len(), 1);
    assert_eq!(mod_rows[0].name, "bound");
    assert!(!mod_rows[0].orphaned);

    let stranger_rows = get_bot_list(&app, &stranger_token).await;
    assert!(
        stranger_rows.is_empty(),
        "a moderator with no grant must not see the bound bot or the orphan"
    );

    let hidden = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", orphan_id.0))
                .header("authorization", auth_header(&mod_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

    let refused = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}", orphan_id.0))
                .header("authorization", auth_header(&mod_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    assert!(
        state
            .music_bots
            .supervisor
            .list()
            .await
            .unwrap()
            .iter()
            .any(|info| info.id == orphan_id),
        "a non-admin stop must not tear the orphan down"
    );

    let stopped = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}", orphan_id.0))
                .header("authorization", auth_header(&admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
    let after = get_bot_list(&app, &admin_token).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].name, "bound");
    assert!(
        !state
            .music_bots
            .supervisor
            .list()
            .await
            .unwrap()
            .iter()
            .any(|info| info.id == orphan_id),
        "admin stop goes through the normal shutdown path"
    );
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

async fn put_summon_cap(
    app: &Router,
    token: &str,
    server: &str,
    cap: u32,
) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/music-summon-caps")
                .header("authorization", auth_header(token))
                .header("content-type", "application/json")
                .body(json_body(&wire::SummonCap {
                    server_addr: server.into(),
                    cap,
                }))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get_summon_caps(app: &Router, token: &str) -> wire::SummonCapList {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/music-summon-caps")
                .header("authorization", auth_header(token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    read_json(resp).await
}

#[tokio::test]
async fn summon_cap_is_empty_until_stored() {
    let (app, token, state) = make_test_app().await;
    let listed = get_summon_caps(&app, &token).await;
    assert!(
        listed.caps.is_empty(),
        "nothing stored must not invent a cap"
    );

    let created = put_summon_cap(&app, &token, "127.0.0.1:9987", 1).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0),
        "a stored cap arms nothing until a saved bot is known"
    );
    assert_eq!(created.status(), StatusCode::OK);
    let stored: wire::SummonCap = read_json(created).await;
    assert_eq!(stored.cap, 1);

    let other = put_summon_cap(&app, &token, "127.0.0.1:9988", 4).await;
    assert_eq!(other.status(), StatusCode::OK);
    let zero = put_summon_cap(&app, &token, "127.0.0.1:9988", 0).await;
    assert_eq!(zero.status(), StatusCode::OK);

    let listed = get_summon_caps(&app, &token).await;
    assert_eq!(listed.caps.len(), 2);
    assert_eq!(listed.caps[0].server_addr, "127.0.0.1:9987");
    assert_eq!(listed.caps[0].cap, 1);
    assert_eq!(listed.caps[1].server_addr, "127.0.0.1:9988");
    assert_eq!(listed.caps[1].cap, 0);

    let too_big = put_summon_cap(&app, &token, "127.0.0.1:9987", 65).await;
    assert_eq!(too_big.status(), StatusCode::BAD_REQUEST);
    let empty = put_summon_cap(&app, &token, "   ", 3).await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, "127.0.0.1:9987")
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn saved_bots_share_one_pool_and_do_not_write_the_cap() {
    let (app, token, state) = make_test_app().await;
    let first = create_test_bot(&app, &token).await;
    assert!(get_summon_caps(&app, &token).await.caps.is_empty());
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );

    let saved = put_summon_cap(&app, &token, "127.0.0.1:9987", 1).await;
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2),
        "a known saved bot is enough to arm the cap"
    );

    let second_body = wire::CreateBotRequest {
        name: "Second".into(),
        server_addr: "127.0.0.1:9987".into(),
        identity_path: None,
        auto_connect: Some(false),
    };
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&second_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    let other_body = wire::CreateBotRequest {
        name: "Other".into(),
        server_addr: "127.0.0.1:9988".into(),
        identity_path: None,
        auto_connect: Some(false),
    };
    let other = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(&token))
                .header("content-type", "application/json")
                .body(json_body(&other_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::CREATED);

    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9988"),
        Some(0)
    );
    let caps = get_summon_caps(&app, &token).await;
    assert_eq!(caps.caps.len(), 1);
    assert_eq!(caps.caps[0].cap, 1);

    let listed = app
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
    let bots: Vec<wire::MusicBotSummary> = read_json(listed).await;
    assert_eq!(bots.len(), 3);
    assert!(bots.iter().any(|bot| bot.id == first.id));

    let rows = crate::repos::music_bot_runtime::list(&state.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| {
        !row.identityPath.contains("quiet-identities") && !row.identityPath.contains("summon-")
    }));

    let fresh = crate::music_runtime::MusicBotFront::local(std::sync::Arc::new(
        music_bot::BotSupervisor::new(),
    ));
    for row in &rows {
        let cap = crate::repos::music_summon_cap::get(&state.db, &row.serverAddr)
            .await
            .unwrap();
        let home = crate::repos::music_summon_cap::get_home(&state.db, &row.serverAddr)
            .await
            .unwrap();
        fresh
            .spawn_with_id_and_summon(
                music_bot::BotId(row.id as u64),
                music_bot::BotConfig::new(
                    row.name.clone(),
                    std::path::PathBuf::from(&row.identityPath),
                )
                .with_server_addr(row.serverAddr.clone())
                .with_auto_connect(false),
                state.yt_cookie.clone(),
                state.yt_api_key.clone(),
                cap,
                home,
            )
            .await
            .unwrap();
    }
    assert_eq!(fresh.list().await.unwrap().len(), 3);
    assert_eq!(fresh.local_summon_limit("127.0.0.1:9987"), Some(2));
    assert_eq!(fresh.local_summon_limit("127.0.0.1:9988"), Some(0));
}

#[tokio::test]
async fn connect_disconnect_and_dropping_a_summon_client_leave_the_saved_bot() {
    let (app, token, state) = make_test_app().await;
    let created = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 1)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    state
        .music_bots
        .supervisor
        .local_hear_crossing("127.0.0.1:9987", 40, "!play https://cdn.example/one.mp3")
        .unwrap();
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_quiet_count("127.0.0.1:9987"),
        Some(1)
    );

    let connect = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/connect", created.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(connect.status(), StatusCode::ACCEPTED);
    let disconnect = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/music-bots/{}/disconnect", created.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disconnect.status(), StatusCode::ACCEPTED);
    // Connect and Disconnect on the saved bot neither start nor stop a
    // summon client, and leave summon armed.
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    let summons = state
        .music_bots
        .supervisor
        .local_summon_ids("127.0.0.1:9987")
        .unwrap();
    assert_eq!(summons.len(), 1);
    assert!(
        state
            .music_bots
            .supervisor
            .local_drop_summon("127.0.0.1:9987", summons[0])
            .unwrap()
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_quiet_count("127.0.0.1:9987"),
        Some(0)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );

    let detail = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/music-bots/{}", created.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    let detail: wire::MusicBotDetail = read_json(detail).await;
    assert_eq!(detail.id, created.id);
    assert_eq!(
        crate::repos::music_bot_runtime::list(&state.db)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_later_bot_push_carries_only_that_servers_stored_cap() {
    let (app, token, state) = make_test_app().await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 2)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );
    let _created = create_test_bot(&app, &token).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    let _second = create_test_bot(&app, &token).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 2,
        }]
    );
}

#[tokio::test]
async fn a_failed_store_rolls_the_music_process_back_to_the_stored_cap() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 1)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );

    state
        .db
        .query("DEFINE FIELD OVERWRITE cap ON music_summon_cap TYPE int ASSERT $value = 1;")
        .await
        .unwrap()
        .check()
        .unwrap();

    let failed = put_summon_cap(&app, &token, "127.0.0.1:9987", 4).await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(1)),
        "the process is back on the stored number without another push or a restart"
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_armed("127.0.0.1:9987"),
        Some(true)
    );
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 1,
        }]
    );
}

#[tokio::test]
async fn a_failed_first_store_clears_the_number_the_process_accepted() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_armed("127.0.0.1:9987"),
        Some(false)
    );

    state
        .db
        .query(
            "DEFINE FIELD OVERWRITE serverAddr ON music_summon_cap TYPE string ASSERT $value = 'blocked';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    let failed = put_summon_cap(&app, &token, "127.0.0.1:9987", 2).await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(None)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_armed("127.0.0.1:9987"),
        Some(false)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );
    assert!(get_summon_caps(&app, &token).await.caps.is_empty());

    let _later = create_test_bot(&app, &token).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_armed("127.0.0.1:9987"),
        Some(false),
        "a later push that carries no number does not arm the cleared cap"
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(None)
    );
}

#[tokio::test]
async fn caps_the_caller_cannot_see_are_omitted() {
    let state = fresh_state().await;
    let server_id = server_connections::list(&state.db).await.unwrap()[0].id;
    let admin_id = seed_user_role(&state, "admin-caps", "admin").await;
    let admin_token = mint_token_role(&state, admin_id, "admin-caps", "admin");
    let mod_id = seed_user_role(&state, "mod-caps", "moderator").await;
    let mod_token = mint_token_role(&state, mod_id, "mod-caps", "moderator");
    crate::repos::server_user_grants::insert(&state.db, mod_id, server_id)
        .await
        .unwrap();
    let stranger_id = seed_user_role(&state, "stranger-caps", "moderator").await;
    let stranger_token = mint_token_role(&state, stranger_id, "stranger-caps", "moderator");
    crate::repos::music_summon_cap::upsert(&state.db, "127.0.0.1:9987", 1)
        .await
        .unwrap();
    crate::repos::music_summon_cap::upsert(&state.db, "10.255.255.1:9987", 4)
        .await
        .unwrap();
    let app = app(state);

    let admin_caps = get_summon_caps(&app, &admin_token).await;
    assert_eq!(admin_caps.caps.len(), 2);
    let mod_caps = get_summon_caps(&app, &mod_token).await;
    assert_eq!(mod_caps.caps.len(), 1);
    assert_eq!(mod_caps.caps[0].server_addr, "127.0.0.1:9987");
    let stranger_caps = get_summon_caps(&app, &stranger_token).await;
    assert!(
        stranger_caps.caps.is_empty(),
        "a moderator with no grant must not see summon caps"
    );
}

#[tokio::test]
async fn deleting_the_last_saved_bot_stops_summon_clients_and_the_cap_stays_settable() {
    let (app, token, state) = make_test_app().await;
    let created = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 1)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
    state
        .music_bots
        .supervisor
        .local_hear_crossing("127.0.0.1:9987", 41, "!play https://cdn.example/one.mp3")
        .unwrap();
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_quiet_count("127.0.0.1:9987"),
        Some(1)
    );

    let deleted = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/music-bots/{}", created.id.0))
                .header("authorization", auth_header(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_quiet_count("127.0.0.1:9987"),
        Some(0)
    );
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 1,
        }]
    );

    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 0)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(0))
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(0)
    );
}

#[tokio::test]
async fn two_saves_at_once_leave_the_process_on_the_stored_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let (first, second) = tokio::join!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 2),
        put_summon_cap(&app, &token, "127.0.0.1", 1),
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let stored = get_summon_caps(&app, &token).await;
    assert_eq!(stored.caps.len(), 1);
    let cap = stored.caps[0].cap;
    assert!(cap == 1 || cap == 2);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap("127.0.0.1"),
        Some(Some(cap))
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(music_bot::parallel_limit(Some(cap)))
    );
}

#[tokio::test]
async fn a_dropped_save_stays_on_the_number_the_other_save_stores() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 1)
            .await
            .status(),
        StatusCode::OK
    );
    let guard = super::summon::hold_forwarded_cap(&state, "127.0.0.1:9987", 4).await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(4))
    );
    let app2 = app.clone();
    let token2 = token.clone();
    let put = tokio::spawn(async move { put_summon_cap(&app2, &token2, "127.0.0.1", 2).await });
    drop(guard);
    assert_eq!(put.await.unwrap().status(), StatusCode::OK);
    let stored = get_summon_caps(&app, &token).await;
    assert_eq!(stored.caps.len(), 1);
    assert_eq!(stored.caps[0].cap, 2);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap("127.0.0.1"),
        Some(Some(2))
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
}

#[tokio::test]
async fn a_failed_first_save_does_not_clear_the_other_saves_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let guard = super::summon::hold_forwarded_cap(&state, "127.0.0.1:9987", 2).await;
    let app2 = app.clone();
    let token2 = token.clone();
    let put =
        tokio::spawn(async move { put_summon_cap(&app2, &token2, "127.0.0.1:9987", 1).await });
    drop(guard);
    assert_eq!(put.await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 1,
        }]
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(1))
    );
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_limit("127.0.0.1:9987"),
        Some(2)
    );
}

#[tokio::test]
async fn a_lost_forward_puts_the_process_back_on_the_stored_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 1)
            .await
            .status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap("127.0.0.1:9987", 4)
        .await
        .unwrap();
    super::summon::align_cap_for_test(
        &state,
        state.music_bots.supervisor.clone(),
        "127.0.0.1:9987",
        None,
    )
    .await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(1))
    );
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 1,
        }]
    );
}

#[tokio::test]
async fn a_second_save_is_not_rolled_back_to_a_number_read_earlier() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    assert_eq!(
        put_summon_cap(&app, &token, "127.0.0.1:9987", 2)
            .await
            .status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap("127.0.0.1:9987", 4)
        .await
        .unwrap();
    super::summon::align_cap_for_test(
        &state,
        state.music_bots.supervisor.clone(),
        "127.0.0.1:9987",
        Some(4),
    )
    .await;
    assert_eq!(
        state
            .music_bots
            .supervisor
            .local_summon_cap("127.0.0.1:9987"),
        Some(Some(2))
    );
    assert_eq!(
        get_summon_caps(&app, &token).await.caps,
        vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 2,
        }]
    );
}

#[tokio::test]
async fn a_failed_restore_stores_only_the_number_a_later_read_shows() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9983";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    super::summon::arm_cap_save_fault(server, 0, 1);
    let outcome = super::summon::align_cap_for_test(
        &state,
        state.music_bots.supervisor.clone(),
        server,
        Some(4),
    )
    .await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(outcome, super::summon::AlignOutcome::CaughtUp(4));
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(4)
    );
}

#[tokio::test]
async fn a_restore_error_after_apply_does_not_store_the_new_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9984";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    super::summon::arm_restore_error_after_apply(server);
    let outcome = super::summon::align_cap_for_test(
        &state,
        state.music_bots.supervisor.clone(),
        server,
        Some(4),
    )
    .await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(outcome, super::summon::AlignOutcome::Restored(Some(1)));
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1)),
        "the restore landed, so the process is not left on 4"
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn a_later_save_cannot_store_while_align_holds_the_lock() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9991";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    super::summon::arm_cap_save_fault(server, 1, 1);
    let (pause, release) = super::summon::align_pause(server);
    let aligning = state.clone();
    let align = tokio::spawn(async move {
        super::summon::align_cap_for_test(
            &aligning,
            aligning.music_bots.supervisor.clone(),
            "127.0.0.1:9991",
            Some(4),
        )
        .await
    });
    pause
        .await
        .expect("align sleeps while the save lock is held");
    assert!(
        state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_err(),
        "the save lock stays held between align tries"
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1),
        "the catch-up write has not landed, and a later save has not stored"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    let app2 = app.clone();
    let token2 = token.clone();
    let put =
        tokio::spawn(async move { put_summon_cap(&app2, &token2, "127.0.0.1:9991", 2).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !put.is_finished(),
        "a later save must not store while align still holds the lock"
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
    release
        .send(())
        .expect("align is still waiting on the save lock");
    assert_eq!(
        align.await.unwrap(),
        super::summon::AlignOutcome::CaughtUp(4),
        "a failed restore is followed by a read, and that read showed 4"
    );
    let saved = put.await.unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let body: wire::SummonCap = read_json(saved).await;
    assert_eq!(body.cap, 2);
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(2))
    );
    super::summon::clear_cap_save_fault(server);
}

#[tokio::test]
async fn a_catch_up_write_returns_the_number_that_stuck() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9992";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_cap_save_fault(server, 1, 1);
    let saved = put_summon_cap(&app, &token, server, 4).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(saved.status(), StatusCode::OK);
    let body: wire::SummonCap = read_json(saved).await;
    assert_eq!(body.server_addr, server);
    assert_eq!(body.cap, 4);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4)),
        "the process stays on the number the catch-up write stored"
    );
    let caps = get_summon_caps(&app, &token).await;
    let row = caps
        .caps
        .iter()
        .find(|cap| cap.server_addr == server)
        .expect("the catch-up number is stored");
    assert_eq!(row.cap, 4);
}

#[tokio::test]
async fn a_forward_error_after_accept_returns_the_number_the_process_holds() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9993";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_forward_error_after_accept(server);
    let saved = put_summon_cap(&app, &token, server, 4).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(saved.status(), StatusCode::OK);
    let body: wire::SummonCap = read_json(saved).await;
    assert_eq!(body.server_addr, server);
    assert_eq!(body.cap, 4);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(4)
    );
}

#[tokio::test]
async fn a_forward_error_leaves_the_store_when_the_process_is_still_on_the_old_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9994";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_forward_error_before_accept(server);
    let failed = put_summon_cap(&app, &token, server, 4).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn a_failed_process_read_does_not_return_the_forward_error() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9995";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_forward_error_after_accept(server);
    super::summon::arm_process_read_failures(server, 1);
    let (pause, release) = super::summon::align_pause(server);
    let app2 = app.clone();
    let token2 = token.clone();
    let put =
        tokio::spawn(async move { put_summon_cap(&app2, &token2, "127.0.0.1:9995", 4).await });
    pause
        .await
        .expect("a failed process read waits under the save lock");
    assert!(
        state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_err(),
        "the save lock stays held while the process read is retried"
    );
    assert!(
        !put.is_finished(),
        "a failed process read must not return the forward error"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1),
        "the stored row stays on the previous save while the read is failing"
    );
    release.send(()).expect("the process read is still waiting");
    let saved = put.await.unwrap();
    super::summon::clear_cap_save_fault(server);
    assert_eq!(saved.status(), StatusCode::OK);
    let body: wire::SummonCap = read_json(saved).await;
    assert_eq!(body.server_addr, server);
    assert_eq!(body.cap, 4);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(4)
    );
}

#[tokio::test]
async fn a_dropped_handler_reads_until_the_process_read_succeeds() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9996";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    super::summon::arm_process_read_failures(server, 4);
    let (pause, release) = super::summon::align_pause(server);
    drop(super::summon::hold_unconfirmed_cap(&state, server, 4).await);
    pause
        .await
        .expect("a dropped save keeps reading under the save lock");
    assert!(
        state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_err(),
        "align keeps the save lock while process reads fail"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4)),
        "a dropped save does not roll the process back while reads fail"
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1),
        "the stored row stays until a process read succeeds"
    );
    release
        .send(())
        .expect("align is still waiting on the failed process read");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("align did not finish after the process read succeeded");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    super::summon::clear_cap_save_fault(server);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(4)
    );
}

#[tokio::test]
async fn a_dropped_handler_leaves_the_store_when_the_process_is_still_on_the_old_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9997";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    drop(super::summon::hold_unconfirmed_cap(&state, server, 4).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("align did not finish after reading the old number");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn a_restore_that_lands_and_then_errors_does_not_return_the_new_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9985";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_cap_save_fault(server, 1, 0);
    super::summon::arm_restore_error_after_apply(server);
    let saved = put_summon_cap(&app, &token, server, 4).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(saved.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn a_drop_after_a_forward_stores_the_new_number() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9990";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    let pending = super::summon::hold_forwarded_cap(&state, server, 4).await;
    drop(pending);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the dropped forward did not store its number");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(4)
    );
}

#[tokio::test]
async fn a_drop_during_align_does_not_let_the_failed_save_overwrite_the_later_one() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9998";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    super::summon::arm_cap_save_fault(server, 1, 1);
    let (pause, release) = super::summon::align_pause(server);
    let app2 = app.clone();
    let token2 = token.clone();
    let put =
        tokio::spawn(async move { put_summon_cap(&app2, &token2, "127.0.0.1:9998", 4).await });
    pause
        .await
        .expect("align waits under the save lock after the restore fails");
    let app3 = app.clone();
    let token3 = token.clone();
    let later =
        tokio::spawn(async move { put_summon_cap(&app3, &token3, "127.0.0.1:9998", 2).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !later.is_finished(),
        "a later save waits while align holds the lock"
    );
    put.abort();
    let _ = put.await;
    assert!(
        state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_err(),
        "dropping the handler leaves the align task holding the save lock"
    );
    assert!(
        !later.is_finished(),
        "a later save must not store while the dropped align still holds the lock"
    );
    release
        .send(())
        .expect("align is still waiting on the failed restore");
    let saved = later.await.unwrap();
    super::summon::clear_cap_save_fault(server);
    assert_eq!(saved.status(), StatusCode::OK);
    let body: wire::SummonCap = read_json(saved).await;
    assert_eq!(body.cap, 2);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(2))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(2)
    );
}

#[tokio::test]
async fn a_dropped_save_restores_a_third_number_onto_the_stored_row() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9989";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 7)
        .await
        .unwrap();
    drop(super::summon::hold_unconfirmed_cap(&state, server, 4).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("align looped on a third number instead of restoring the stored row");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn a_forward_error_restores_a_third_number_before_it_returns() {
    let (app, token, state) = make_test_app().await;
    let _bot = create_test_bot(&app, &token).await;
    let server = "127.0.0.1:9986";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 7)
        .await
        .unwrap();
    super::summon::arm_forward_error_before_accept(server);
    let failed = put_summon_cap(&app, &token, server, 4).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(1))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        Some(1)
    );
}

async fn create_bot_on(app: &Router, token: &str, server: &str) -> axum::http::Response<Body> {
    let body = wire::CreateBotRequest {
        name: "DJ-Bot".into(),
        server_addr: server.to_string(),
        identity_path: None,
        auto_connect: Some(false),
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/music-bots")
                .header("authorization", auth_header(token))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_bot_create_does_not_overwrite_a_cap_the_process_already_accepted() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9971";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(0)
    );
    let created = create_bot_on(&app, &token, server).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let process = state.music_bots.supervisor.local_summon_cap(server);
    let stored = crate::repos::music_summon_cap::get(&state.db, server)
        .await
        .unwrap();
    assert_eq!(
        process,
        Some(Some(4)),
        "the push carried the stored 1 and must not replace the accepted 4"
    );
    assert_eq!(
        stored,
        process.flatten(),
        "the store must keep the number the process kept"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

#[tokio::test]
async fn a_bot_create_without_a_stored_cap_arms_the_number_the_process_holds() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9972";
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    let created = create_bot_on(&app, &token, server).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let process = state.music_bots.supervisor.local_summon_cap(server);
    let stored = crate::repos::music_summon_cap::get(&state.db, server)
        .await
        .unwrap();
    assert_eq!(process, Some(Some(4)));
    assert_eq!(
        stored,
        process.flatten(),
        "the store must hold the number the process already held"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

async fn assert_store_matches_process(state: &AppState, server: &str) {
    let process = state.music_bots.supervisor.local_summon_cap(server);
    let stored = crate::repos::music_summon_cap::get(&state.db, server)
        .await
        .unwrap();
    assert_eq!(
        stored,
        process.flatten(),
        "the store and the process must hold the same number"
    );
}

#[tokio::test]
async fn a_push_error_stores_the_cap_the_process_already_holds() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9961";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    crate::music_runtime::arm_spawn_error_after_push(server);
    let created = create_bot_on(&app, &token, server).await;
    crate::music_runtime::clear_spawn_error_after_push(server);
    assert_eq!(created.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4)),
        "the push was applied before the reply failed"
    );
    assert_store_matches_process(&state, server).await;
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

#[tokio::test]
async fn a_push_error_stores_a_held_cap_when_the_store_was_empty() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9962";
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    crate::music_runtime::arm_spawn_error_after_push(server);
    let created = create_bot_on(&app, &token, server).await;
    crate::music_runtime::clear_spawn_error_after_push(server);
    assert_eq!(created.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_store_matches_process(&state, server).await;
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

async fn drop_create_while_the_cap_write_is_in_progress(
    app: &Router,
    token: &str,
    state: &AppState,
    server: &str,
    stored_before: Option<u32>,
) {
    super::summon::arm_process_read_failures(server, 1);
    let (pause, release) = super::summon::align_pause(server);
    let app2 = app.clone();
    let token2 = token.to_string();
    let server_for_task = server.to_string();
    let created =
        tokio::spawn(async move { create_bot_on(&app2, &token2, &server_for_task).await });
    pause
        .await
        .expect("the cap write waits under the save lock");
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        stored_before,
        "the write has not landed yet"
    );
    created.abort();
    let _ = created.await;
    assert!(
        state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_err(),
        "dropping create leaves the cap write holding the save lock"
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_cap(server),
        Some(Some(4))
    );
    assert_eq!(
        crate::repos::music_summon_cap::get(&state.db, server)
            .await
            .unwrap(),
        stored_before,
        "a dropped create must not leave the store behind while the write still holds the lock"
    );
    release
        .send(())
        .expect("the cap write is still waiting on the process read");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if state
            .music_bots
            .summon_save_mutex(server)
            .try_lock()
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the cap write did not finish after the process read succeeded");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    super::summon::clear_cap_save_fault(server);
    assert_store_matches_process(state, server).await;
}

#[tokio::test]
async fn a_dropped_create_stores_the_cap_the_process_already_holds() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9963";
    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    drop_create_while_the_cap_write_is_in_progress(&app, &token, &state, server, Some(1)).await;
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

#[tokio::test]
async fn a_dropped_create_stores_a_held_cap_when_the_store_was_empty() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9964";
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, 4)
        .await
        .unwrap();
    drop_create_while_the_cap_write_is_in_progress(&app, &token, &state, server, None).await;
    assert_eq!(
        state.music_bots.supervisor.local_summon_armed(server),
        Some(true)
    );
    assert_eq!(
        state.music_bots.supervisor.local_summon_limit(server),
        Some(4)
    );
}

async fn put_summon_home(
    app: &Router,
    token: &str,
    server: &str,
    channel_id: Option<u64>,
) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/music-summon-homes")
                .header("authorization", auth_header(token))
                .header("content-type", "application/json")
                .body(json_body(&wire::SummonHome {
                    server_addr: server.into(),
                    channel_id,
                }))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn process_home(state: &AppState, server: &str) -> Option<u64> {
    state
        .music_bots
        .supervisor
        .read_summon_home(server)
        .await
        .unwrap()
}

async fn stored_home(state: &AppState, server: &str) -> Option<u64> {
    crate::repos::music_summon_cap::get_home(&state.db, server)
        .await
        .unwrap()
}

#[tokio::test]
async fn summon_home_is_absent_until_picked_and_shares_the_cap_row() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9941";
    let listed = get_summon_caps(&app, &token).await;
    assert!(listed.homes.is_empty(), "nothing picked is not a home");

    let picked = put_summon_home(&app, &token, server, Some(12)).await;
    assert_eq!(picked.status(), StatusCode::OK);
    let picked: wire::SummonHome = read_json(picked).await;
    assert_eq!(picked.channel_id, Some(12));
    assert_eq!(process_home(&state, server).await, Some(12));
    let listed = get_summon_caps(&app, &token).await;
    assert_eq!(
        listed.homes,
        vec![wire::SummonHome {
            server_addr: server.into(),
            channel_id: Some(12),
        }]
    );
    assert!(listed.caps.is_empty(), "a home does not invent a cap");

    assert_eq!(
        put_summon_cap(&app, &token, server, 1).await.status(),
        StatusCode::OK
    );
    let listed = get_summon_caps(&app, &token).await;
    assert_eq!(listed.caps.len(), 1);
    assert_eq!(listed.caps[0].cap, 1);
    assert_eq!(listed.homes.len(), 1);
    assert_eq!(
        crate::repos::music_summon_cap::list(&state.db)
            .await
            .unwrap()
            .len(),
        1
    );

    let cleared = put_summon_home(&app, &token, server, None).await;
    assert_eq!(cleared.status(), StatusCode::OK);
    assert_eq!(process_home(&state, server).await, None);
    let listed = get_summon_caps(&app, &token).await;
    assert!(listed.homes.is_empty());
    assert_eq!(listed.caps[0].cap, 1, "clearing the home keeps the cap");

    for bad in [Some(0), Some(u64::MAX)] {
        let refused = put_summon_home(&app, &token, server, bad).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{bad:?}");
    }
    let empty = put_summon_home(&app, &token, "   ", Some(12)).await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(stored_home(&state, server).await, None);
}

#[tokio::test]
async fn a_summon_home_save_needs_the_same_access_as_the_cap() {
    let (app, _token, state) = make_test_app().await;
    let server = "127.0.0.1:9942";
    let viewer_id = seed_user_role(&state, "viewer-home", "viewer").await;
    let viewer = mint_token_role(&state, viewer_id, "viewer-home", "viewer");
    let refused = put_summon_home(&app, &viewer, server, Some(12)).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(process_home(&state, server).await, None);
    assert_eq!(stored_home(&state, server).await, None);
}

#[tokio::test]
async fn a_bot_push_carries_the_stored_home() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9943";
    crate::repos::music_summon_cap::upsert_home(&state.db, server, Some(21))
        .await
        .unwrap();
    assert_eq!(process_home(&state, server).await, None);
    let created = create_bot_on(&app, &token, server).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(process_home(&state, server).await, Some(21));
}

#[tokio::test]
async fn a_failed_home_store_puts_the_process_back_on_the_stored_home() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9944";
    assert_eq!(
        put_summon_home(&app, &token, server, Some(12))
            .await
            .status(),
        StatusCode::OK
    );
    super::summon::arm_cap_save_fault(server, 1, 0);
    let failed = put_summon_home(&app, &token, server, Some(30)).await;
    super::summon::clear_cap_save_fault(server);
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(process_home(&state, server).await, Some(12));
    assert_eq!(stored_home(&state, server).await, Some(12));
}

#[tokio::test]
async fn a_home_forward_error_is_settled_by_what_the_process_holds() {
    let (app, token, state) = make_test_app().await;
    let server = "127.0.0.1:9945";
    // The process took it, then the reply failed: that home is stored.
    super::summon::arm_forward_error_after_accept(server);
    let landed = put_summon_home(&app, &token, server, Some(12)).await;
    assert_eq!(landed.status(), StatusCode::OK);
    assert_eq!(process_home(&state, server).await, Some(12));
    assert_eq!(stored_home(&state, server).await, Some(12));

    // The process never took it: both sides keep the old home.
    super::summon::arm_forward_error_before_accept(server);
    let lost = put_summon_home(&app, &token, server, Some(30)).await;
    super::summon::clear_cap_save_fault(server);
    assert!(lost.status().is_server_error(), "{}", lost.status());
    assert_eq!(process_home(&state, server).await, Some(12));
    assert_eq!(stored_home(&state, server).await, Some(12));
}

/// WebQuery stand-in for the home picker: two virtual servers, one per
/// voice port, each with its own channels.
async fn boot_home_picker_webquery(api_key: &'static str) -> u16 {
    use axum::extract::Path as AxPath;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use serde_json::{Value, json};

    fn reply(headers: &HeaderMap, api_key: &str, body: Value) -> axum::Json<Value> {
        let ok = headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == api_key);
        if !ok {
            return axum::Json(json!({
                "body": null,
                "status": {"code": 1283, "message": "client_query_login_failed"},
            }));
        }
        axum::Json(json!({ "body": body, "status": {"code": 0, "message": "ok"} }))
    }

    let app = Router::new()
        .route(
            "/serverlist",
            get(move |headers: HeaderMap| async move {
                reply(
                    &headers,
                    api_key,
                    json!([
                        { "virtualserver_id": "1", "virtualserver_name": "Main", "virtualserver_port": "9987" },
                        { "virtualserver_id": "2", "virtualserver_name": "Side", "virtualserver_port": "9946" }
                    ]),
                )
            }),
        )
        .route(
            "/{sid}/{cmd}",
            get(move |headers: HeaderMap, AxPath((sid, cmd)): AxPath<(i64, String)>| async move {
                assert!(cmd.starts_with("channellist"), "{cmd}");
                let body = if sid == 2 {
                    json!([
                        { "cid": "1", "pid": "0", "channel_order": "0", "channel_name": "Side lobby", "channel_flag_default": "1" },
                        { "cid": "3", "pid": "0", "channel_order": "1", "channel_name": "Bot room" }
                    ])
                } else {
                    json!([
                        { "cid": "1", "pid": "0", "channel_order": "0", "channel_name": "Lobby", "channel_flag_default": "1" }
                    ])
                };
                reply(&headers, api_key, body)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    port
}

#[tokio::test]
async fn the_home_picker_lists_the_channels_on_that_voice_port() {
    let (app, token, state) = make_test_app().await;
    let api_key = "home-picker-key";
    let port = boot_home_picker_webquery(api_key).await;
    let connection = server_connections::list(&state.db).await.unwrap().remove(0);
    server_connections::patch(
        &state.db,
        connection.id,
        server_connections::PatchServerConnection {
            name: None,
            host: None,
            webquery_port: Some(i64::from(port)),
            api_key: Some(crate::crypto::seal(api_key).unwrap()),
            use_https: None,
            ssh_port: None,
            ssh_username: None,
            ssh_password: None,
            control_path: None,
            ssh_auth_method: None,
            ssh_host_key_fingerprint: None,
        },
    )
    .await
    .unwrap();

    let get_channels = |server: &str| {
        let app = app.clone();
        let token = token.clone();
        let uri = format!("/api/music-summon-homes/channels?serverAddr={server}");
        async move {
            app.oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    let side = get_channels("127.0.0.1:9946").await;
    assert_eq!(side.status(), StatusCode::OK);
    let side: wire::SummonHomeChannelList = read_json(side).await;
    assert_eq!(side.server_addr, "127.0.0.1:9946");
    assert_eq!(
        side.channels,
        vec![
            wire::SummonHomeChannel {
                channel_id: 1,
                path: "Side lobby".into(),
                is_default: true,
            },
            wire::SummonHomeChannel {
                channel_id: 3,
                path: "Bot room".into(),
                is_default: false,
            },
        ]
    );

    let main: wire::SummonHomeChannelList = read_json(get_channels("127.0.0.1").await).await;
    assert_eq!(main.server_addr, "127.0.0.1:9987");
    assert_eq!(main.channels.len(), 1);
    assert_eq!(main.channels[0].path, "Lobby");

    let nowhere = get_channels("127.0.0.1:9999").await;
    assert_eq!(nowhere.status(), StatusCode::BAD_REQUEST);
    let unknown = get_channels("10.255.255.1:9987").await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
}
