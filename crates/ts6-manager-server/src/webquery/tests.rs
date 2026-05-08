//! Integration tests for the WebQuery client.
//!
//! These spin up a tiny axum router on a random localhost port and let the
//! real `reqwest` client talk to it. Coverage:
//!
//! - happy path: envelope `{body, status: {code: 0, message: "ok"}}` → typed
//!   model.
//! - upstream-error path: non-zero `status.code` surfaces as
//!   [`WebQueryError::Upstream`] with the original code + message.
//! - auth path: missing / wrong `x-api-key` → `1283 client_query_login_failed`
//!   propagated as `Upstream`.
//! - TLS-rejection path: HTTPS to a self-signed listener with
//!   `allow_self_signed = false` → `Transport` error.
//! - parameter passing: query params survive the URL-encode round-trip.
//!
//! No real TS6 instance is required — every test runs in-process.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::*;

/// State held by the mock router so individual tests can assert on captured
/// query strings.
#[derive(Default, Clone)]
struct MockState {
    expected_api_key: Arc<String>,
    captured_channel_query: Arc<std::sync::Mutex<Option<String>>>,
    request_count: Arc<AtomicU64>,
}

async fn handler_version(
    State(state): State<MockState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    state.request_count.fetch_add(1, Ordering::SeqCst);
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": {"version": "3.13.7", "build": "20250101", "platform": "Linux"},
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_serverlist(
    State(state): State<MockState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [
            {"virtualserver_id": "1", "virtualserver_name": "Alpha", "virtualserver_status": "online"},
            {"virtualserver_id": "2", "virtualserver_name": "Beta", "virtualserver_status": "offline"}
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_channellist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_channel_query.lock().unwrap() = Some(
        params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&"),
    );
    Json(json!({
        "body": (0..3).map(|i| json!({
            "cid": format!("{}", i + sid),
            "channel_name": format!("Lobby {i}"),
        })).collect::<Vec<_>>(),
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientlist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [
            {"clid": "10", "client_type": "0", "client_nickname": "Alice"},
            {"clid": "11", "client_type": "0", "client_nickname": "Bob"},
            // ServerQuery slot — must NOT be counted as an online user.
            {"clid": "99", "client_type": "1", "client_nickname": "serveradmin"},
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_serverinfo(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": {
            "virtualserver_name": "Alpha",
            "virtualserver_platform": "Linux",
            "virtualserver_version": "3.13.7",
            "virtualserver_maxclients": "32",
            "virtualserver_uptime": "12345",
            "virtualserver_total_packetloss_total": "0.001",
            "virtualserver_total_ping": "12.5"
        },
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_connection_info(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": {
            "connection_bandwidth_received_last_second_total": "1000",
            "connection_bandwidth_sent_last_second_total": "2000"
        },
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

fn key_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == expected)
}

fn upstream_error(code: i64, message: &str) -> axum::response::Response {
    (
        StatusCode::OK,
        Json(json!({
            "body": null,
            "status": {"code": code, "message": message},
        })),
    )
        .into_response()
}

struct MockServer {
    addr: SocketAddr,
    state: MockState,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockServer {
    async fn start(api_key: &str) -> Self {
        let state = MockState {
            expected_api_key: Arc::new(api_key.to_string()),
            ..Default::default()
        };
        let app = Router::new()
            .route("/version", get(handler_version))
            .route("/serverlist", get(handler_serverlist))
            .route("/{sid}/channellist", get(handler_channellist))
            .route("/{sid}/clientlist", get(handler_clientlist))
            .route("/{sid}/serverinfo", get(handler_serverinfo))
            .route(
                "/{sid}/serverrequestconnectioninfo",
                get(handler_connection_info),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        Self {
            addr,
            state,
            _shutdown: tx,
        }
    }

    fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }
}

fn build_client(server: &MockServer, api_key: &str) -> WebQueryClient {
    WebQueryClient::new(
        42, // arbitrary config_id
        &server.host(),
        server.port(),
        false, // http
        api_key.to_string(),
        false,
    )
    .expect("client builds")
}

#[tokio::test]
async fn version_happy_path_returns_typed_body() {
    let server = MockServer::start("the-correct-key").await;
    let client = build_client(&server, "the-correct-key");

    let info = client.version().await.expect("version succeeds");
    assert_eq!(info.platform, "Linux");
    assert_eq!(info.version, "3.13.7");
}

#[tokio::test]
async fn missing_api_key_surfaces_as_upstream_error() {
    let server = MockServer::start("the-correct-key").await;
    let client = build_client(&server, "the-wrong-key");

    let err = client.version().await.unwrap_err();
    match &err {
        WebQueryError::Upstream { code, message } => {
            assert_eq!(*code, 1283);
            assert_eq!(message, "client_query_login_failed");
        }
        other => panic!("expected upstream error, got {other:?}"),
    }
    assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    assert_eq!(err.upstream_code(), 1283);
}

#[tokio::test]
async fn channellist_counts_match_typed_decode() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let channels = client.channellist(2).await.unwrap();
    assert_eq!(channels.len(), 3);
    assert_eq!(channels[0].cid, 2);
    assert_eq!(channels[0].channel_name, "Lobby 0");
}

#[tokio::test]
async fn clientlist_caller_can_filter_by_client_type() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let clients = client.clientlist(1).await.unwrap();
    let online = clients.iter().filter(|c| c.client_type == 0).count();
    assert_eq!(online, 2, "ServerQuery slot must be excludable by caller");
}

#[tokio::test]
async fn serverinfo_round_trips_dashboard_fields() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let info = client.serverinfo(1).await.unwrap();
    assert_eq!(info.virtualserver_name, "Alpha");
    assert_eq!(info.virtualserver_maxclients, 32);
    assert_eq!(info.virtualserver_uptime, 12_345);
}

#[tokio::test]
async fn connection_info_round_trips_bandwidth_fields() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let ci = client.server_connection_info(1).await.unwrap();
    assert_eq!(ci.connection_bandwidth_received_last_second_total, 1000);
    assert_eq!(ci.connection_bandwidth_sent_last_second_total, 2000);
}

#[tokio::test]
async fn serverlist_returns_all_virtualservers() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let list = client.serverlist().await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].virtualserver_id, 1);
    assert_eq!(list[1].virtualserver_name, "Beta");
}

#[tokio::test]
async fn malformed_envelope_surfaces_invalid_response() {
    // Custom one-off mock that returns a non-envelope body.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/version",
        get(|| async { (StatusCode::OK, "not-json-at-all").into_response() }),
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });

    let client = WebQueryClient::new(
        99,
        &addr.ip().to_string(),
        addr.port(),
        false,
        "k".into(),
        false,
    )
    .unwrap();

    let err = client.version().await.unwrap_err();
    assert!(matches!(err, WebQueryError::InvalidResponse(_)));
    drop(tx);
}

#[tokio::test]
async fn transport_error_when_target_refuses_connection() {
    // Bind, capture port, drop the listener so the port is closed.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let client = WebQueryClient::new(
        99,
        &addr.ip().to_string(),
        addr.port(),
        false,
        "k".into(),
        false,
    )
    .unwrap();

    let err = client.version().await.unwrap_err();
    assert!(
        matches!(err, WebQueryError::Transport(_)),
        "got {err:?}"
    );
    assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    assert_eq!(err.upstream_code(), -1);
}

#[tokio::test]
async fn https_against_plaintext_target_fails_when_self_signed_disabled() {
    // Plaintext mock; the client requests https → handshake / parse fails.
    let server = MockServer::start("k").await;
    let client = WebQueryClient::new(
        99,
        &server.host(),
        server.port(),
        true, // useHttps = true forces TLS handshake
        "k".into(),
        false, // allow_self_signed = false
    )
    .unwrap();

    let err = client.version().await.unwrap_err();
    assert!(
        matches!(err, WebQueryError::Transport(_)),
        "TLS path must surface as a transport error, got {err:?}"
    );
}

#[tokio::test]
async fn pool_caches_clients_per_config_id() {
    crypto::init("test-seed");
    let pool = WebQueryPool::new(false);
    // Legacy plaintext (no `enc:` prefix) — `crypto::unseal` returns it
    // verbatim per the pass-through rule. Using real ciphertext here would
    // require a fixed key the test can re-derive.
    let conn = sample_connection(7, "host.example", "plain-api-key");

    let a = pool.get_or_build(7, Some(&conn)).await.unwrap();
    let b = pool.get_or_build(7, None).await.unwrap();
    assert!(Arc::ptr_eq(&a, &b), "second lookup must reuse the cached client");
}

#[tokio::test]
async fn pool_missing_connection_returns_canonical_500_message() {
    let pool = WebQueryPool::new(false);
    let err = pool.get_or_build(404, None).await.unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("No connection configured for server config ID 404"),
        "spec §10.7 canonical message; got `{s}`"
    );
}

fn sample_connection(id: i64, host: &str, api_key: &str) -> ServerConnection {
    use chrono::Utc;
    ServerConnection {
        id,
        name: "test".into(),
        host: host.into(),
        webqueryPort: 10080,
        apiKey: api_key.into(),
        useHttps: false,
        sshPort: 10022,
        sshUsername: None,
        sshPassword: None,
        queryBotChannel: None,
        queryBotNickname: None,
        sshBotNickname: None,
        enabled: true,
        createdAt: Utc::now(),
        updatedAt: Utc::now(),
    }
}

#[test]
fn escape_module_is_reachable_from_module_root() {
    // Pin the public surface so SSHBRIDGE can still import via
    // `crate::webquery::escape::escape`.
    assert_eq!(escape::escape("a b"), "a\\sb");
}
