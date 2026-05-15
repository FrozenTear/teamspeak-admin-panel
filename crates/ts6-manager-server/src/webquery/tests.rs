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
    captured_query: Arc<std::sync::Mutex<Option<(String, String)>>>,
    request_count: Arc<AtomicU64>,
}

async fn handler_version(State(state): State<MockState>, headers: HeaderMap) -> impl IntoResponse {
    state.request_count.fetch_add(1, Ordering::SeqCst);
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    // TS6 wire shape captured against `6.0.0-beta9`: singleton commands
    // wrap `body` in a one-element array. The legacy `body: {...}` form is
    // exercised by `singleton_envelope_accepts_legacy_object_shape` below.
    Json(json!({
        "body": [{"version": "3.13.7", "build": "20250101", "platform": "Linux"}],
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
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    // Capture the raw query string (not the parsed map) so flag-only tests
    // can assert the exact `-uid&-away` wire format TS6 expects.
    *state.captured_channel_query.lock().unwrap() = raw;
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
    // TS6 singleton wire shape: `body` wrapped in a one-element array.
    Json(json!({
        "body": [{
            "virtualserver_name": "Alpha",
            "virtualserver_platform": "Linux",
            "virtualserver_version": "3.13.7",
            "virtualserver_maxclients": "32",
            "virtualserver_uptime": "12345",
            "virtualserver_total_packetloss_total": "0.001",
            "virtualserver_total_ping": "12.5"
        }],
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
        "body": [{
            "connection_bandwidth_received_last_second_total": "1000",
            "connection_bandwidth_sent_last_second_total": "2000"
        }],
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
            .route("/hostinfo", get(handler_hostinfo))
            .route("/{sid}/channellist", get(handler_channellist))
            .route("/{sid}/clientlist", get(handler_clientlist))
            .route("/{sid}/serverinfo", get(handler_serverinfo))
            .route(
                "/{sid}/serverrequestconnectioninfo",
                get(handler_connection_info),
            )
            // Phase 2 (PURA-68) handlers.
            .route("/{sid}/clientinfo", get(handler_clientinfo))
            .route("/{sid}/clientdblist", get(handler_clientdblist))
            .route("/{sid}/channelinfo", get(handler_channelinfo))
            .route("/{sid}/channelclientlist", get(handler_channelclientlist))
            .route("/{sid}/logview", get(handler_logview))
            .route("/{sid}/banlist", get(handler_banlist))
            .route("/{sid}/banadd", get(handler_banadd))
            .route("/{sid}/bandel", get(handler_bandel))
            .route("/{sid}/bandelall", get(handler_bandelall))
            .route("/{sid}/clientkick", get(handler_clientkick))
            .route("/{sid}/clientpoke", get(handler_clientpoke))
            .route("/{sid}/clientmove", get(handler_clientmove))
            .route("/{sid}/clientedit", get(handler_clientedit))
            .route(
                "/{sid}/channelclientpermlist",
                get(handler_channelclientpermlist),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
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
    // PURA-220: refused-connection paths now carry the typed `Connect`
    // kind so the §7.0.2 envelope and the dashboard banner pick up
    // "connect: …" prefixes instead of reqwest's `Display` blob.
    match &err {
        WebQueryError::Transport(ct) => {
            assert_eq!(ct.kind, WebQueryTransportKind::Connect);
            assert!(
                ct.message.contains(&format!("{}:{}", addr.ip(), addr.port())),
                "operator message must reference the dial target: {}",
                ct.message
            );
        }
        other => panic!("expected Transport, got {other:?}"),
    }
    assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    assert_eq!(err.upstream_code(), -1);
    // §7.0.2 details renders the typed prefix end-to-end.
    let details = err.upstream_message();
    assert!(
        details.starts_with("connect: "),
        "expected connect-prefixed details, got `{details}`"
    );
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
    // PURA-220: keep the variant assertion permissive — reqwest may
    // surface a TLS-over-plaintext failure as a connect-class error on
    // some builds. The point of the test is that the typed envelope
    // flows through, regardless of the exact bucket the classifier
    // picks for an HTTP-talking-server-mistaken-for-TLS shape.
    match &err {
        WebQueryError::Transport(ct) => {
            assert!(
                matches!(
                    ct.kind,
                    WebQueryTransportKind::Tls
                        | WebQueryTransportKind::Connect
                        | WebQueryTransportKind::Other
                ),
                "TLS-over-plaintext should classify as Tls/Connect/Other, got {:?}",
                ct.kind
            );
        }
        other => panic!("TLS path must surface as a transport error, got {other:?}"),
    }
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
    assert!(
        Arc::ptr_eq(&a, &b),
        "second lookup must reuse the cached client"
    );
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
    // PURA-220: §7.0.2 `details` envelope renders the canonical message
    // prefixed with the typed `other:` class.
    match &err {
        WebQueryError::Transport(ct) => {
            assert_eq!(ct.kind, WebQueryTransportKind::Other);
        }
        other => panic!("expected Transport, got {other:?}"),
    }
    let details = err.upstream_message();
    assert!(
        details.starts_with("other: ") && details.contains("404"),
        "expected `other:`-prefixed details, got `{details}`"
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
        // D-SSH-AUTH (PURA-77) defaults — webquery path, password auth.
        controlPath: "webquery".into(),
        sshAuthMethod: "password".into(),
        sshPrivateKey: None,
        sshKeyAgentSocket: None,
        sshHostKeyFingerprint: None,
    }
}

#[test]
fn escape_module_is_reachable_from_module_root() {
    // Pin the public surface so SSHBRIDGE can still import via
    // `crate::webquery::escape::escape`.
    assert_eq!(escape::escape("a b"), "a\\sb");
}

// =========================================================================
// Phase 2 (PURA-68) — handlers + tests for the full command surface
// =========================================================================

async fn handler_hostinfo(State(state): State<MockState>, headers: HeaderMap) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [{
            "instance_uptime": "12345",
            "host_timestamp_utc": "1700000000",
            "virtualservers_running_total": "2",
            "virtualservers_total_clients_online": "10",
            "virtualservers_total_channels_online": "20",
            "virtualservers_total_maxclients": "64",
            "connection_bandwidth_sent_last_second_total": "1024",
            "connection_bandwidth_received_last_second_total": "2048",
            "connection_packets_sent_total": "10000",
            "connection_packets_received_total": "9999",
            "connection_bytes_sent_total": "5000000",
            "connection_bytes_received_total": "4999999",
        }],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientinfo(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    let clid = params.get("clid").cloned().unwrap_or_default();
    Json(json!({
        "body": [{
            "client_unique_identifier": format!("uid-{clid}="),
            "client_nickname": "Alice",
            "client_database_id": "1000",
            "cid": "5",
            "client_type": "0",
            "client_idle_time": "5000",
            "client_lastconnected": "1700000000",
            "client_input_muted": "0",
            "client_output_muted": "1",
            "client_country": "DE",
            "connection_client_ip": "203.0.113.10",
            "client_servergroups": "8,9",
        }],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientdblist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("start").cloned().unwrap_or_default(),
        params.get("duration").cloned().unwrap_or_default(),
    ));
    Json(json!({
        "body": [
            {
                "cldbid": "42",
                "client_unique_identifier": "uid-42=",
                "client_nickname": "Bob",
                "client_created": "1690000000",
                "client_lastconnected": "1700000000",
                "client_totalconnections": "37",
                "client_lastip": "10.0.0.1",
            },
            {
                "cldbid": "43",
                "client_unique_identifier": "uid-43=",
                "client_nickname": "Carol",
                "client_created": "1691000000",
                "client_lastconnected": "1700100000",
                "client_totalconnections": "5",
                "client_lastip": "10.0.0.2",
            }
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_channelinfo(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [{
            "channel_name": "Default Channel",
            "channel_topic": "topic",
            "channel_description": "desc",
            "channel_codec": "4",
            "channel_codec_quality": "10",
            "channel_maxclients": "-1",
            "channel_maxfamilyclients": "-1",
            "channel_order": "0",
            "pid": "0",
            "channel_flag_permanent": "1",
            "channel_needed_talk_power": "0",
        }],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_channelclientlist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [
            {"clid": "10", "cid": "1", "client_database_id": "1000",
             "client_type": "0", "client_nickname": "Alice"},
            {"clid": "11", "cid": "1", "client_database_id": "1001",
             "client_type": "0", "client_nickname": "Bob"},
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_logview(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("lines").cloned().unwrap_or_default(),
        params.get("reverse").cloned().unwrap_or_default(),
    ));
    Json(json!({
        "body": [
            {"last_pos": "2048", "file_size": "8192",
             "l": "2024-01-01 INFO ServerLib started"},
            {"l": "2024-01-01 INFO listener bound to 9987/UDP"},
            {"l": "2024-01-01 INFO sql update done"},
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_banlist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": [
            {
                "banid": "7",
                "ip": "10.0.0.5",
                "uid": "uid=",
                "name": "",
                "created": "1700000000",
                "duration": "0",
                "reason": "Spamming",
                "invokername": "operator",
                "invokercldbid": "1",
                "invokeruid": "op-uid=",
                "enforcements": "1",
                "lastnickname": "Spammer",
            }
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_banadd(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("ip").cloned().unwrap_or_default(),
        params.get("banreason").cloned().unwrap_or_default(),
    ));
    Json(json!({
        "body": [{"banid": "11"}],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_bandel(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("banid").cloned().unwrap_or_default(),
        String::new(),
    ));
    Json(json!({
        "body": {},
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_bandelall(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    Json(json!({
        "body": {},
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientkick(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("clid").cloned().unwrap_or_default(),
        params.get("reasonid").cloned().unwrap_or_default(),
    ));
    // PURA-193 regression: real TS6 `6.0.0-beta9` returns `body: null` for
    // no-return mutations. Keep the rest of the write-path handlers on
    // `body: {}` so both wire shapes stay covered.
    Json(json!({
        "body": null,
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientpoke(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("clid").cloned().unwrap_or_default(),
        params.get("msg").cloned().unwrap_or_default(),
    ));
    Json(json!({
        "body": {},
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientmove(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("clid").cloned().unwrap_or_default(),
        params.get("cid").cloned().unwrap_or_default(),
    ));
    // PURA-193 — second no-return mutation exercising the `body: null`
    // success branch through the full client. `clientkick`/`clientmove`/
    // `clientedit`/`bandel*` all share the parser path under audit.
    Json(json!({
        "body": null,
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_clientedit(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(_sid): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    *state.captured_query.lock().unwrap() = Some((
        params.get("clid").cloned().unwrap_or_default(),
        params
            .get("CLIENT_INPUT_MUTED")
            .cloned()
            .unwrap_or_default(),
    ));
    Json(json!({
        "body": {},
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

async fn handler_channelclientpermlist(
    State(state): State<MockState>,
    headers: HeaderMap,
    Path(sid): Path<i64>,
) -> impl IntoResponse {
    if !key_matches(&headers, &state.expected_api_key) {
        return upstream_error(1283, "client_query_login_failed");
    }
    // sid=999 → exercise the 1281 → [] coercion path.
    if sid == 999 {
        return upstream_error(1281, "database_empty_result");
    }
    Json(json!({
        "body": [
            {
                "permid": "12345",
                "permsid": "i_channel_needed_modify_power",
                "permvalue": "75",
                "permnegated": "0",
                "permskip": "0",
            }
        ],
        "status": {"code": 0, "message": "ok"},
    }))
    .into_response()
}

#[tokio::test]
async fn hostinfo_returns_typed_counters() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let host = client.hostinfo().await.unwrap();
    assert_eq!(host.virtualservers_running_total, 2);
    assert_eq!(host.virtualservers_total_clients_online, 10);
    assert_eq!(host.connection_bandwidth_received_last_second_total, 2048);
}

#[tokio::test]
async fn clientinfo_returns_full_metadata() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let info = client.clientinfo(1, 10).await.unwrap();
    assert_eq!(info.client_unique_identifier, "uid-10=");
    assert_eq!(info.cid, 5);
    assert_eq!(info.connection_client_ip, "203.0.113.10");
    assert_eq!(info.client_output_muted, 1);
}

#[tokio::test]
async fn clientdblist_passes_pagination_and_decodes() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let rows = client.clientdblist(1, 25, 50).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cldbid, 42);
    assert_eq!(rows[1].client_lastip, "10.0.0.2");
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("25".to_string(), "50".to_string()));
}

#[tokio::test]
async fn channelinfo_round_trips_full_payload() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let info = client.channelinfo(1, 5).await.unwrap();
    assert_eq!(info.channel_name, "Default Channel");
    assert_eq!(info.channel_codec, 4);
    assert_eq!(info.channel_flag_permanent, 1);
    assert_eq!(info.channel_maxclients, -1);
}

#[tokio::test]
async fn channelclientlist_returns_only_listed_channel_clients() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let rows = client.channelclientlist(1, 1).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].clid, 10);
    assert_eq!(rows[1].client_database_id, 1001);
}

#[tokio::test]
async fn logview_passes_pagination_params_and_keeps_metadata_on_first_row() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let rows = client.logview(1, 100, true, false, Some(0)).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].last_pos, Some(2048));
    assert_eq!(rows[0].file_size, Some(8192));
    assert!(rows[0].l.contains("ServerLib started"));
    assert_eq!(rows[1].last_pos, None);
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("100".to_string(), "1".to_string()));
}

#[tokio::test]
async fn banlist_decodes_banentries() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let bans = client.banlist(1).await.unwrap();
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].banid, 7);
    assert_eq!(bans[0].reason, "Spamming");
    assert_eq!(bans[0].invokername, "operator");
}

#[tokio::test]
async fn banadd_returns_new_banid_and_passes_params() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let id = client
        .banadd(
            1,
            &BanAddParams {
                ip: Some("10.0.0.99"),
                banreason: Some("Trolling"),
                time: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(id, 11);
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("10.0.0.99".to_string(), "Trolling".to_string()));
}

#[tokio::test]
async fn bandel_passes_banid() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client.bandel(1, 7).await.unwrap();
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("7".to_string(), String::new()));
}

#[tokio::test]
async fn bandelall_succeeds_with_unit_body() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");
    client.bandelall(1).await.unwrap();
}

#[tokio::test]
async fn clientkick_passes_clid_and_reasonid() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client
        .clientkick(1, 10, 5, Some("Idle timeout"))
        .await
        .unwrap();
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("10".to_string(), "5".to_string()));
}

#[tokio::test]
async fn clientpoke_passes_clid_and_msg() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client.clientpoke(1, 10, "wake up").await.unwrap();
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("10".to_string(), "wake up".to_string()));
}

#[tokio::test]
async fn clientmove_passes_clid_and_target_cid() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client.clientmove(1, 10, 7, None).await.unwrap();
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("10".to_string(), "7".to_string()));
}

#[tokio::test]
async fn client_set_muted_writes_input_muted_property() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client
        .client_set_muted(1, 10, Some(true), None)
        .await
        .unwrap();
    let captured = server.state.captured_query.lock().unwrap().clone().unwrap();
    assert_eq!(captured, ("10".to_string(), "1".to_string()));
}

#[tokio::test]
async fn client_set_muted_with_no_changes_is_noop() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    client.client_set_muted(1, 10, None, None).await.unwrap();
    // No upstream call should fire — captured_query stays None.
    assert!(server.state.captured_query.lock().unwrap().is_none());
}

#[tokio::test]
async fn channelclientpermlist_returns_typed_rows() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let perms = client.channelclientpermlist(1, 5, 1000).await.unwrap();
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0].permid, 12_345);
    assert_eq!(perms[0].permvalue, 75);
    assert_eq!(perms[0].permsid, "i_channel_needed_modify_power");
}

#[tokio::test]
async fn channelclientpermlist_translates_1281_to_empty() {
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    // sid=999 returns upstream code 1281 in the mock; the helper must
    // translate it to an empty Vec (spec §10.6).
    let perms = client.channelclientpermlist(999, 5, 1000).await.unwrap();
    assert!(perms.is_empty());
}

#[tokio::test]
async fn flag_suffix_renders_query_string_flag_chain() {
    // PURA-102 (Defect 2): TS6 rejects path-concat (`clientlist-uid-away`)
    // with code 1538. Bare-key query form (`?-uid&-away`) is the upstream's
    // own documented input. Cover empty and prefix-tolerant inputs.
    assert_eq!(super::flag_suffix(&[]), "");
    assert_eq!(super::flag_suffix(&["uid", "away"]), "?-uid&-away");
    assert_eq!(super::flag_suffix(&["-uid", "-away"]), "?-uid&-away");
}

#[tokio::test]
async fn clientlist_with_flags_emits_query_string_flags() {
    // PURA-102 (Defect 2): wire-level proof that `clientlist -uid -away`
    // hits the upstream as `clientlist?-uid&-away` (no path concat, no
    // `=` after the flags). Captured by [`handler_clientlist`] from the
    // raw request URI.
    let server = MockServer::start("k").await;
    let client = build_client(&server, "k");

    let _ = client
        .clientlist_with_flags(1, &["uid", "away"])
        .await
        .expect("clientlist_with_flags decodes the response");

    let raw = server
        .state
        .captured_channel_query
        .lock()
        .unwrap()
        .clone()
        .expect("handler must have captured the request query");
    assert_eq!(
        raw, "-uid&-away",
        "TS6 expects bare-key query flags; got `{raw}`"
    );
}

#[test]
fn unwrap_singleton_body_accepts_legacy_object_shape() {
    // PURA-102 (Defect 1): the singleton dispatch must still pass through
    // the legacy `body: {...}` form so TS3-shaped fixtures and any future
    // proxies that unwrap the array on our behalf keep decoding cleanly.
    let v = json!({"version": "3.13.7", "build": "20250101", "platform": "Linux"});
    let unwrapped = super::unwrap_singleton_body(v.clone()).unwrap();
    assert_eq!(unwrapped, v);
    let info: VersionInfo = serde_json::from_value(unwrapped).unwrap();
    assert_eq!(info.version, "3.13.7");
}

#[test]
fn unwrap_singleton_body_accepts_ts6_array_wrap() {
    // PURA-102 (Defect 1): TS6 always wraps singleton bodies in a one-
    // element array. Dispatch must unwrap it transparently.
    let v = json!([{"version": "6.0.0-beta9", "build": "1776774292", "platform": "Linux"}]);
    let unwrapped = super::unwrap_singleton_body(v).unwrap();
    let info: VersionInfo = serde_json::from_value(unwrapped).unwrap();
    assert_eq!(info.version, "6.0.0-beta9");
    assert_eq!(info.build, "1776774292");
}

#[test]
fn unwrap_singleton_body_rejects_multi_element_array() {
    // Defensive: a multi-element array on a singleton call indicates a
    // wiring mistake (a list-shaped command got routed through `get_one`).
    // Surface it as `InvalidResponse` so we get a clear error instead of
    // silently dropping the extra rows.
    let v = json!([
        {"version": "a", "build": "1", "platform": "Linux"},
        {"version": "b", "build": "2", "platform": "Linux"}
    ]);
    let err = super::unwrap_singleton_body(v).unwrap_err();
    assert!(matches!(err, WebQueryError::InvalidResponse(_)));
}

#[test]
fn unwrap_singleton_body_rejects_empty_array() {
    let err = super::unwrap_singleton_body(json!([])).unwrap_err();
    assert!(matches!(err, WebQueryError::InvalidResponse(_)));
}

// =========================================================================
// Env-gated integration test against a local TS6 host.
//
// Set `TS6_INTEGRATION_HOST=<host>` plus `TS6_INTEGRATION_API_KEY=<key>`
// (and optionally `TS6_INTEGRATION_PORT`, `TS6_INTEGRATION_HTTPS=1`) to
// opt in. Without these env vars, the test no-ops so CI / local dev stay
// hermetic.
// =========================================================================
#[tokio::test]
async fn integration_against_local_ts6_host_when_env_configured() {
    let Ok(host) = std::env::var("TS6_INTEGRATION_HOST") else {
        eprintln!("skipping: TS6_INTEGRATION_HOST not set");
        return;
    };
    let api_key = std::env::var("TS6_INTEGRATION_API_KEY")
        .expect("TS6_INTEGRATION_HOST set without TS6_INTEGRATION_API_KEY");
    let port: u16 = std::env::var("TS6_INTEGRATION_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10080);
    let use_https = std::env::var("TS6_INTEGRATION_HTTPS")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let allow_self_signed = std::env::var("TS6_INTEGRATION_ALLOW_SELF_SIGNED")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let client = WebQueryClient::new(0, &host, port, use_https, api_key, allow_self_signed)
        .expect("client builds");

    // Cheap probes — no mutations against the real host.
    let version = client.version().await.expect("version probe ok");
    assert!(!version.version.is_empty());

    let host_info = client.hostinfo().await.expect("hostinfo ok");
    assert!(host_info.virtualservers_running_total >= 0);

    let servers = client.serverlist().await.expect("serverlist ok");
    if let Some(first) = servers.first() {
        let info = client
            .serverinfo(first.virtualserver_id)
            .await
            .expect("serverinfo ok");
        assert!(!info.virtualserver_name.is_empty());

        // Read paths only — verifies the typed surface decodes the real
        // response shape without mutating upstream state.
        let _ = client
            .clientlist_with_flags(first.virtualserver_id, &["uid", "away"])
            .await;
        let _ = client.banlist(first.virtualserver_id).await;
    }
}

// =========================================================================
// PURA-193 — envelope decode: `status.code=0 && body=null` is success for
// no-return mutations (`clientkick`, `clientmove`, `clientedit`, `bandel*`,
// `servernotifyregister`/`servernotifyunregister`, empty `sendtextmessage`).
// Direct unit tests on the parser so the regression can't come back via a
// targeted refactor of the request path.
// =========================================================================

#[test]
fn envelope_null_body_is_success_for_unit_body() {
    let raw = r#"{"body": null, "status": {"code": 0, "message": "ok"}}"#;
    let envelope: Envelope = serde_json::from_str(raw).expect("envelope parses");
    let decoded: UnitBody = envelope.into_body().expect("null body decodes as UnitBody");
    match decoded {
        UnitBody::Object(serde_json::Value::Null) => {}
        other => panic!("expected UnitBody::Object(Null), got {other:?}"),
    }
}

#[test]
fn envelope_missing_body_is_success_for_unit_body() {
    // Some upstreams elide the field entirely instead of emitting `null`.
    let raw = r#"{"status": {"code": 0, "message": "ok"}}"#;
    let envelope: Envelope = serde_json::from_str(raw).expect("envelope parses");
    envelope
        .into_body::<UnitBody>()
        .expect("missing body decodes as UnitBody");
}

#[test]
fn envelope_null_body_still_rejected_for_list_models() {
    let raw = r#"{"body": null, "status": {"code": 0, "message": "ok"}}"#;
    let envelope: Envelope = serde_json::from_str(raw).expect("envelope parses");
    let err = envelope
        .into_body::<Vec<ClientEntry>>()
        .expect_err("list shape must reject null body");
    assert!(
        matches!(err, WebQueryError::InvalidResponse(_)),
        "expected InvalidResponse, got {err:?}"
    );
}

#[test]
fn envelope_null_body_does_not_mask_upstream_error() {
    // Real upstreams send `body: null` alongside non-zero status codes — the
    // upstream error must win regardless of body shape.
    let raw = r#"{"body": null, "status": {"code": 1283, "message": "client_query_login_failed"}}"#;
    let envelope: Envelope = serde_json::from_str(raw).expect("envelope parses");
    let err = envelope
        .into_body::<UnitBody>()
        .expect_err("non-zero status must surface");
    match err {
        WebQueryError::Upstream { code, message } => {
            assert_eq!(code, 1283);
            assert_eq!(message, "client_query_login_failed");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}
