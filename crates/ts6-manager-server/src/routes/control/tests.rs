//! Integration tests for the Phase 2 control surface — PURA-71.
//!
//! Strategy: spin up a tiny axum server on a random localhost port that
//! impersonates the TS WebQuery API (mirroring `webquery::tests`). The
//! `server_connection` row points at that port via plain HTTP, so the real
//! `WebQueryClient` issues real reqwest calls. Then a separate axum
//! `Router::oneshot` exercises the control router itself with a JWT-authed
//! caller.
//!
//! We assert the wire contract (`status` + body) per spec §7.0.2 and the
//! upstream invocation (path + query — captured by the mock).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, Query as AxQuery, State as AxState};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use ts6_manager_shared::control::{
    BanCreateRequest, BanCreated, BanListItem, ChannelTreeNode, ClientDetail, ClientListItem,
    KickKind, KickRequest, MoveRequest, MuteRequest, ServerInfoResponse,
};

use crate::app_state::AppState;
use crate::auth::extractors::AuthUser;
use crate::auth::{jwt, password};
use crate::crypto;
use crate::db::{Database, connect_in_memory, migrations};
use crate::repos::server_connections::{NewServerConnection, ServerConnection, insert};
use crate::repos::{server_user_grants, users};
use crate::webquery::WebQueryPool;
use crate::ws::Hub;

#[derive(Clone, Default)]
struct MockState {
    api_key: Arc<String>,
    captured_paths: Arc<Mutex<Vec<String>>>,
    captured_queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
    behavior: Arc<MockBehavior>,
}

#[derive(Default)]
struct MockBehavior {
    /// When set, every endpoint returns an upstream error with this code/msg.
    force_upstream_error: Mutex<Option<(i64, String)>>,
}

fn ok_envelope(body: Value) -> axum::response::Response {
    axum::Json(json!({
        "body": body,
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

fn err_envelope(code: i64, msg: &str) -> axum::response::Response {
    axum::Json(json!({
        "body": null,
        "status": {"code": code, "message": msg},
    }))
    .into_response()
}

fn key_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h == expected)
}

async fn capture_and_dispatch(
    state: &MockState,
    headers: &HeaderMap,
    path: String,
    params: HashMap<String, String>,
    body: Value,
) -> axum::response::Response {
    state.captured_paths.lock().unwrap().push(path);
    state.captured_queries.lock().unwrap().push(params);
    if !key_matches(headers, &state.api_key) {
        return err_envelope(1283, "client_query_login_failed");
    }
    if let Some((code, msg)) = state.behavior.force_upstream_error.lock().unwrap().clone() {
        return err_envelope(code, &msg);
    }
    ok_envelope(body)
}

/// Single dispatcher — `cmd` is the path segment after the sid (which
/// the WebQuery client builds as `clientlist-uid-away-…` when the
/// PURA-68 surface adds flags). Dispatch by `cmd` prefix so the mock
/// covers both the bare `clientlist` and `clientlist-…` shapes.
async fn handler_dispatch(
    AxState(state): AxState<MockState>,
    headers: HeaderMap,
    AxPath((sid, cmd)): AxPath<(i64, String)>,
    AxQuery(params): AxQuery<HashMap<String, String>>,
) -> impl IntoResponse {
    let path_for_capture = format!("/{sid}/{cmd}");
    let base = cmd.split('-').next().unwrap_or("").to_string();
    let body = match base.as_str() {
        // Detail-route's lookup ignores the `clid` query param: the route
        // falls back to a clientlist scan for live-clid resolution, so the
        // mock returns the same payload as the bulk fetch regardless.
        "clientlist" => json!([
            {
                "clid": "10",
                "cid": "1",
                "client_database_id": "100",
                "client_type": "0",
                "client_nickname": "Alice",
                "client_unique_identifier": "uid-A=",
                "client_input_muted": "0",
                "client_output_muted": "0",
                "client_country": "DE",
                "connection_client_ip": "203.0.113.10",
            },
            {
                "clid": "11",
                "cid": "1",
                "client_database_id": "101",
                "client_type": "1",
                "client_nickname": "serveradmin",
                "client_unique_identifier": "uid-Q=",
            }
        ]),
        "channellist" => json!([
            { "cid": "1", "pid": "0", "channel_order": "0", "channel_name": "Lobby", "channel_topic": "Welcome" },
            { "cid": "2", "pid": "1", "channel_order": "1", "channel_name": "Voice", "channel_topic": "" }
        ]),
        "clientdbinfo" => json!({
            "cldbid": "100",
            "client_unique_identifier": "uid-A=",
            "client_nickname": "Alice",
            "client_created": "1700000000",
            "client_lastconnected": "1700000100",
            "client_totalconnections": "5",
            "client_description": "regular",
            "client_lastip": "203.0.113.10",
        }),
        "clientinfo" => json!({
            "client_unique_identifier": "uid-A=",
            "client_nickname": "Alice",
            "client_database_id": "100",
            "cid": "5",
            "client_type": "0",
            "client_idle_time": "1000",
            "client_lastconnected": "1700000100",
            "client_input_muted": "0",
            "client_output_muted": "1",
            "client_country": "DE",
        }),
        "serverinfo" => json!({
            "virtualserver_name": "Alpha",
            "virtualserver_platform": "Linux",
            "virtualserver_version": "3.13.7",
            "virtualserver_maxclients": "32",
            "virtualserver_uptime": "12345",
            "virtualserver_total_packetloss_total": "0.001",
            "virtualserver_total_ping": "20.5"
        }),
        "logview" => json!([
            { "last_pos": "1024", "file_size": "4096", "l": "2024-01-01 INFO ServerLib started" },
            { "l": "2024-01-01 WARN something fishy" },
            { "l": "2024-01-01 ERROR boom" }
        ]),
        "banlist" => json!([
            {
                "banid": "1",
                "ip": "203.0.113.10",
                "uid": "",
                "mytsid": "",
                "name": "",
                "created": "1700000000",
                "duration": "0",
                "reason": "Spamming",
                "invokername": "operator",
                "invokercldbid": "1",
                "invokeruid": "op-uid=",
                "enforcements": "0",
                "lastnickname": ""
            }
        ]),
        "banadd" => json!({ "banid": "42" }),
        // Write commands return a status-only envelope with body `{}`.
        "clientkick" | "clientmove" | "clientedit" | "bandel" | "bandelall" => json!({}),
        // Anything we didn't pre-load — surface as a generic upstream
        // mismatch so the test fails loud.
        other => json!({ "_mock_unknown_command": other }),
    };
    capture_and_dispatch(&state, &headers, path_for_capture, params, body).await
}

/// Boot the mock TS WebQuery server. Returns the bound port and the
/// shared `MockState` so tests can assert on captured paths/queries.
async fn boot_mock_webquery(api_key: &str) -> (u16, MockState) {
    let state = MockState {
        api_key: Arc::new(api_key.to_string()),
        captured_paths: Arc::new(Mutex::new(Vec::new())),
        captured_queries: Arc::new(Mutex::new(Vec::new())),
        behavior: Arc::new(MockBehavior::default()),
    };
    let app = Router::new()
        .route("/{sid}/{cmd}", get(handler_dispatch))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, state)
}

async fn fresh_state() -> AppState {
    let db = connect_in_memory().await.unwrap();
    migrations::run(&db).await.unwrap();
    crypto::init("test-seed-pura-71-routes");
    let control = crate::control::ControlBackendPool::new(false, db.clone());
    AppState {
        db,
        jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
        jwt_access_expiry: Duration::from_secs(900),
        jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
        setup_lock: Arc::new(tokio::sync::Mutex::new(())),
        webquery: WebQueryPool::new(false),
        control,
        ws_hub: Hub::new(),
        widget_cache: crate::widgets::WidgetCache::new(),
        music_bots: crate::music_bots::MusicBotService::default_for_tests(),
        sidecar: None,
        ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
        moq_public_url: None,
        yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
        data_dir: std::path::PathBuf::from("./data"),
        trusted_proxy_hops: 0,
    }
}

async fn seed_user_with_token(state: &AppState, name: &str, role: &str) -> (AuthUser, String) {
    let pw = "Hunter2!ok".to_string();
    let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
        .await
        .unwrap()
        .unwrap();
    let row = users::insert(
        &state.db,
        users::NewUser {
            username: name.into(),
            passwordHash: hash,
            displayName: name.into(),
            role: role.into(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let token = jwt::mint_access(
        row.id,
        &row.username,
        &row.role,
        state.jwt_access_expiry,
        &state.jwt_secret,
    )
    .unwrap();
    (
        AuthUser {
            id: row.id,
            username: row.username,
            display_name: row.displayName,
            role: row.role,
            enabled: row.enabled,
        },
        token,
    )
}

async fn seed_server(state: &AppState, port: u16, api_key_plaintext: &str) -> ServerConnection {
    let new = NewServerConnection {
        name: "Mock".into(),
        host: "127.0.0.1".into(),
        webqueryPort: port as i64,
        apiKey: crypto::seal(api_key_plaintext).unwrap(),
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
    };
    insert(&state.db, new).await.unwrap()
}

fn app(state: AppState) -> Router {
    Router::new().merge(super::router()).with_state(state)
}

fn auth_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
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

async fn read_body_value(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------
// Read happy paths
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_clients_strips_ip_for_non_admin_caller() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;

    // Viewer with grant — can read; FE must NOT see connection_client_ip.
    let (viewer, vtoken) = seed_user_with_token(&state, "viewer", "viewer").await;
    server_user_grants::insert(&state.db, viewer.id, server.id)
        .await
        .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/clients", server.id))
                .header("authorization", auth_header(&vtoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: Vec<ClientListItem> = read_json(resp).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|r| r.connection_client_ip.is_empty()),
        "non-admin must not see connection_client_ip"
    );
}

#[tokio::test]
async fn list_clients_keeps_ip_for_admin() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/clients", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: Vec<ClientListItem> = read_json(resp).await;
    assert_eq!(rows.len(), 2);
    let alice = rows.iter().find(|r| r.clid == 10).unwrap();
    assert_eq!(alice.connection_client_ip, "203.0.113.10");
}

#[tokio::test]
async fn list_clients_unauthenticated_is_401() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/clients", server.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_clients_viewer_without_grant_is_403() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_viewer, vtoken) = seed_user_with_token(&state, "viewer", "viewer").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/clients", server.id))
                .header("authorization", auth_header(&vtoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn list_clients_unknown_server_is_404() {
    let state = fresh_state().await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/servers/9999/vs/1/clients")
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn channel_list_returns_flat_tree() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/channels", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let nodes: Vec<ChannelTreeNode> = read_json(resp).await;
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].channel_name, "Lobby");
    assert_eq!(nodes[1].pid, 1);
}

#[tokio::test]
async fn server_info_passthrough() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/info", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let info: ServerInfoResponse = read_json(resp).await;
    assert_eq!(info.virtualserver_name, "Alpha");
    assert_eq!(info.virtualserver_uptime, 12_345);
}

#[tokio::test]
async fn logs_filter_by_severity_substring() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/servers/{}/vs/1/logs?severity=ERROR",
                    server.id
                ))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = read_json(resp).await;
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0]["text"].as_str().unwrap().contains("ERROR"));
    assert_eq!(body["lastPos"], 1024);
}

// ---------------------------------------------------------------------
// Write paths — kick / mute / move / ban
// ---------------------------------------------------------------------

#[tokio::test]
async fn kick_emits_ws_event_on_clients_topic() {
    let (port, mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    // Subscribe an admin principal BEFORE the action so the broadcast
    // catches the published envelope.
    let admin_principal = crate::ws::auth::Principal::User(crate::ws::auth::UserPrincipal {
        user_id: 1,
        username: "alice".into(),
        role: "admin".into(),
        is_admin: true,
        is_at_least_moderator: true,
    });
    let topic = crate::ws::topic::Topic::new(server.id, crate::ws::topic::TopicKind::Clients);
    let mut sub = state
        .ws_hub
        .subscribe(&state.db, &admin_principal, topic, None)
        .await
        .unwrap();

    let body = KickRequest {
        kind: KickKind::Server,
        reason: Some("Spamming".into()),
    };
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/kick", server.id))
                .header("authorization", auth_header(&atoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // WS event arrived on the clients topic.
    let envelope = sub.receiver.recv().await.unwrap();
    assert_eq!(envelope.kind, "ts:client:kicked_from_server");
    assert_eq!(envelope.data["clid"], 14);
    assert_eq!(envelope.data["reasonid"], 5);

    // Upstream saw the right path + reasonid query param.
    let path = mock.captured_paths.lock().unwrap();
    assert!(path.iter().any(|p| p == "/1/clientkick"));
    let q = mock.captured_queries.lock().unwrap();
    let last = q.last().unwrap();
    assert_eq!(last.get("clid").map(|s| s.as_str()), Some("14"));
    assert_eq!(last.get("reasonid").map(|s| s.as_str()), Some("5"));
    assert_eq!(last.get("reasonmsg").map(|s| s.as_str()), Some("Spamming"));
}

#[tokio::test]
async fn viewer_with_grant_cannot_write() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (viewer, vtoken) = seed_user_with_token(&state, "viewer", "viewer").await;
    server_user_grants::insert(&state.db, viewer.id, server.id)
        .await
        .unwrap();

    let body = KickRequest {
        kind: KickKind::Server,
        reason: None,
    };
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/kick", server.id))
                .header("authorization", auth_header(&vtoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn moderator_with_grant_can_write() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (modr, mtoken) = seed_user_with_token(&state, "modr", "moderator").await;
    server_user_grants::insert(&state.db, modr.id, server.id)
        .await
        .unwrap();

    let body = MoveRequest {
        cid: 2,
        channel_password: None,
    };
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/move", server.id))
                .header("authorization", auth_header(&mtoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn mute_revokes_talker_flag() {
    let (port, mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/mute", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let q = mock.captured_queries.lock().unwrap();
    let last = q.last().unwrap();
    // PURA-299: mute now revokes the talker flag, not client-self muted props.
    assert_eq!(last.get("client_is_talker").map(|s| s.as_str()), Some("0"));
}

#[tokio::test]
async fn unmute_resets_talker_flag() {
    let (port, mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/unmute", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let q = mock.captured_queries.lock().unwrap();
    let last = q.last().unwrap();
    // PURA-299: unmute restores the talker flag.
    assert_eq!(last.get("client_is_talker").map(|s| s.as_str()), Some("1"));
}

#[tokio::test]
async fn ban_create_returns_banid_and_lists_round_trip() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    // POST /bans
    let body = BanCreateRequest {
        ip: Some("203.0.113.10".into()),
        uid: None,
        my_ts_id: None,
        name: None,
        reason: Some("Spamming".into()),
        duration: Some(0),
    };
    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/bans", server.id))
                .header("authorization", auth_header(&atoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: BanCreated = read_json(resp).await;
    assert_eq!(created.banid, 42);

    // GET /bans
    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/bans", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bans: Vec<BanListItem> = read_json(resp).await;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].reason, "Spamming");

    // DELETE /bans/{id}
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/servers/{}/vs/1/bans/1", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn ban_create_rejects_empty_body() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let body = BanCreateRequest::default();
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/bans", server.id))
                .header("authorization", auth_header(&atoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upstream_error_translates_to_502_with_code() {
    let (port, mock) = boot_mock_webquery("API-KEY").await;
    *mock.behavior.force_upstream_error.lock().unwrap() =
        Some((2568, "insufficient client permissions".into()));
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let body = MoveRequest {
        cid: 2,
        channel_password: None,
    };
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/servers/{}/vs/1/clients/14/move", server.id))
                .header("authorization", auth_header(&atoken))
                .header("content-type", "application/json")
                .body(json_body(&body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = read_body_value(resp).await;
    assert_eq!(v["error"], "TeamSpeak API Error");
    assert_eq!(v["code"], 2568);
    assert_eq!(v["details"], "insufficient client permissions");
}

#[tokio::test]
async fn client_detail_passes_through_with_live_when_online() {
    let (port, _mock) = boot_mock_webquery("API-KEY").await;
    let state = fresh_state().await;
    let server = seed_server(&state, port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/clients/100", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail: ClientDetail = read_json(resp).await;
    assert_eq!(detail.cldbid, 100);
    assert_eq!(detail.client_lastip, "203.0.113.10");
    let live = detail.live_client.as_ref().unwrap();
    assert_eq!(live.clid, 10);
    assert_eq!(live.cid, 5);
}

/// PURA-220 — the §7.0.2 `details` envelope on a control-route request
/// that hits a refused TCP target carries the typed `connect:` prefix
/// (and references the dial target), not reqwest's `Display` blob. This
/// is the regression PURA-211 caught on the dashboard banner but never
/// applied to the `/clients` / `/channels` / `/info` paths.
#[tokio::test]
async fn transport_error_details_carry_typed_class_prefix() {
    // Bind/drop pattern reliably yields ECONNREFUSED on connect.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);

    let state = fresh_state().await;
    let server = seed_server(&state, dead_port, "API-KEY").await;
    let (_admin, atoken) = seed_user_with_token(&state, "alice", "admin").await;

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/servers/{}/vs/1/channels", server.id))
                .header("authorization", auth_header(&atoken))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = read_body_value(resp).await;
    assert_eq!(v["error"], "TeamSpeak API Error");
    assert_eq!(v["code"], -1);
    let details = v["details"].as_str().expect("details is a string");
    assert!(
        details.starts_with("connect: "),
        "expected operator-friendly `connect:` prefix per PURA-220, got `{details}`"
    );
    assert!(
        details.contains(&format!("127.0.0.1:{dead_port}")),
        "details must name the dial target, got `{details}`"
    );
}

// Silence unused-helper lints when individual tests are toggled.
#[allow(dead_code)]
fn _ensure_db_arc_used() {
    fn _is_send<T: Send>() {}
    _is_send::<Arc<Database>>();
    let _: Option<MuteRequest> = None;
}
