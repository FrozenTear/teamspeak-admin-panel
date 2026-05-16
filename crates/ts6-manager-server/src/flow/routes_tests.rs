//! Flow-engine REST surface tests — PURA-242 (`http-api.md` §6).
//!
//! Runs the real axum router (so the `RequireAuth` / `RequireAdmin`
//! extractor chain and the `BasicDispatcher`-backed engine are in the
//! loop) against an in-memory SurrealDB. Covers the §6 test surface:
//! POST round-trips, the PATCH definition-swap lock, the DELETE
//! in-flight-run guard, `POST /fire` -> `GET /runs` visibility, the
//! `RequireAdmin` gate on every write route, and the `ErrorBody`
//! envelope shape.
//!
//! Filter: `cargo test -p ts6-manager-server flow::routes_tests`.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::Mutex;
use tower::ServiceExt;
use ts6_manager_shared::flows::{ActionResult, ActionStatus, FlowRunStatus};

use super::engine::{BasicDispatcher, EngineDeps, FlowEngine};
use super::routes::{self, FlowApiState};
use crate::app_state::AppState;
use crate::auth::{jwt, password};
use crate::control::ControlBackendPool;
use crate::crypto;
use crate::db::{connect_in_memory, migrations};
use crate::repos::{bot_flow_runs, bot_flows, server_connections, users};
use crate::webquery::WebQueryPool;
use crate::ws::Hub;

/// Boot schema + a server row, build the engine on `BasicDispatcher`, and
/// return the wired `FlowApiState`. The `FlowEngine` is handed back so the
/// caller keeps its background tasks alive for the test's lifetime.
async fn setup() -> (FlowApiState, FlowEngine, i64) {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations");
    crypto::init("test-seed-pura-242-flow-routes");

    let server_id = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "primary".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
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
    .expect("seed server")
    .id;

    let control = ControlBackendPool::new(false, db.clone());
    let app = AppState {
        db: db.clone(),
        jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
        jwt_access_expiry: Duration::from_secs(900),
        jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
        setup_lock: Arc::new(Mutex::new(())),
        webquery: WebQueryPool::new(false),
        control,
        ws_hub: Hub::new(),
        widget_cache: crate::widgets::WidgetCache::new(),
        music_bots: crate::music_bots::MusicBotService::default_for_tests(),
        sidecar: None,
        ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
        moq_public_url: None,
        yt_cookie: Arc::new(std::sync::RwLock::new(None)),
        data_dir: std::path::PathBuf::from("./data"),
        trusted_proxy_hops: 0,
    };

    let engine = FlowEngine::start(EngineDeps::new(db.clone(), Arc::new(BasicDispatcher)))
        .await
        .expect("engine start");
    let state = FlowApiState::new(app, engine.handle());
    (state, engine, server_id)
}

fn app(state: FlowApiState) -> Router {
    Router::new().merge(routes::router()).with_state(state)
}

/// Mint an access token for a freshly-seeded user with the given role.
async fn seed_token(state: &FlowApiState, role: &str) -> String {
    let pw = "Hunter2!ok".to_string();
    let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
        .await
        .unwrap()
        .unwrap();
    let row = users::insert(
        &state.app.db,
        users::NewUser {
            username: format!("flow-{role}"),
            passwordHash: hash,
            displayName: role.into(),
            role: role.into(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    jwt::mint_access(
        row.id,
        &row.username,
        &row.role,
        state.app.jwt_access_expiry,
        &state.app.jwt_secret,
    )
    .unwrap()
}

/// Fire one request through the router and collect `(status, json-or-null)`.
async fn send(
    router: Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

fn manual_log_flow_body(name: &str, server_id: i64, enabled: bool) -> String {
    serde_json::json!({
        "name": name,
        "serverConfigId": server_id,
        "virtualServerId": 1,
        "enabled": enabled,
        "definition": {
            "trigger": { "kind": "manualFire" },
            "actions": [ { "kind": "logLine", "message": "hello" } ]
        }
    })
    .to_string()
}

#[tokio::test]
async fn create_flow_returns_201_and_round_trips() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&manual_log_flow_body("welcome", server_id, false)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "welcome");
    assert_eq!(body["serverConfigId"], server_id);
    assert!(body["id"].is_number());
    assert!(body["lastRun"].is_null(), "fresh flow has no last run");
    assert_eq!(body["definition"]["trigger"]["kind"], "manualFire");
}

#[tokio::test]
async fn create_flow_rejects_bad_cron() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let body = serde_json::json!({
        "name": "broken-cron",
        "serverConfigId": server_id,
        "virtualServerId": 1,
        "definition": {
            "trigger": { "kind": "cron", "expression": "not a cron expression" },
            "actions": [ { "kind": "logLine", "message": "x" } ]
        }
    })
    .to_string();

    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&body),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation", "body: {body}");
}

#[tokio::test]
async fn create_flow_duplicate_name_conflicts() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let router = app(state);

    let body = manual_log_flow_body("dupe", server_id, false);
    let (first, _) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&body),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);

    let (second, json) = send(
        router,
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&body),
    )
    .await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(json["error"], "name_taken", "body: {json}");
}

#[tokio::test]
async fn patch_definition_swap_blocked_while_enabled() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let router = app(state);

    // Create an *enabled* flow.
    let (status, created) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&manual_log_flow_body("live", server_id, true)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    // Swapping `definition` on a live flow is rejected.
    let patch = serde_json::json!({
        "definition": {
            "trigger": { "kind": "manualFire" },
            "actions": [ { "kind": "logLine", "message": "changed" } ]
        }
    })
    .to_string();
    let (status, body) = send(
        router,
        Method::PATCH,
        &format!("/api/flows/{id}"),
        Some(&token),
        Some(&patch),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "definition_swap_locked", "body: {body}");
}

#[tokio::test]
async fn delete_blocked_when_run_in_flight_then_force_succeeds() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let flow = bot_flows::insert(
        &state.app.db,
        bot_flows::NewBotFlow {
            name: "deletable".into(),
            description: None,
            flowData: serde_json::json!({
                "trigger": { "kind": "manualFire" },
                "actions": [ { "kind": "logLine", "message": "x" } ]
            })
            .to_string(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: false,
        },
    )
    .await
    .unwrap();

    // A run row stuck in flight blocks the default delete.
    bot_flow_runs::insert(
        &state.app.db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow.id,
            trigger: serde_json::json!({ "kind": "manualFire" }),
            status: FlowRunStatus::InFlight,
            actionResults: vec![ActionResult {
                index: 0,
                kind: "logLine".into(),
                status: ActionStatus::Skipped,
                duration_ms: 0,
                error: None,
            }],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();

    let router = app(state);
    let (status, body) = send(
        router.clone(),
        Method::DELETE,
        &format!("/api/flows/{}", flow.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "run_in_flight", "body: {body}");

    // `?force=true` interrupts the in-flight run and deletes the flow.
    let (status, _) = send(
        router,
        Method::DELETE,
        &format!("/api/flows/{}?force=true", flow.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn fire_produces_run_visible_on_runs() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let router = app(state);

    let (status, created) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&manual_log_flow_body("fire-me", server_id, false)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    let (status, fired) = send(
        router.clone(),
        Method::POST,
        &format!("/api/flows/{id}/fire"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {fired}");
    assert!(fired["runId"].is_number());
    assert_eq!(fired["flowId"], id);

    // The run row should be visible on `GET /runs` within 1 s.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let (status, runs) = send(
            router.clone(),
            Method::GET,
            &format!("/api/flows/{id}/runs"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if runs["runs"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            assert_eq!(runs["runs"][0]["flowId"], id);
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fired run never appeared on GET /runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn write_routes_require_admin() {
    let (state, _engine, server_id) = setup().await;
    let admin = seed_token(&state, "admin").await;
    let viewer = seed_token(&state, "viewer").await;
    let router = app(state);

    // Seed one flow with the admin token so the write routes have a target.
    let (status, created) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&admin),
        Some(&manual_log_flow_body("guarded", server_id, false)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    // Every write route rejects a non-admin token with 403.
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/flows".to_string(),
            Some(manual_log_flow_body("viewer-attempt", server_id, false)),
        ),
        (
            Method::PATCH,
            format!("/api/flows/{id}"),
            Some(r#"{"enabled":true}"#.to_string()),
        ),
        (Method::DELETE, format!("/api/flows/{id}"), None),
        (Method::POST, format!("/api/flows/{id}/fire"), None),
    ] {
        let (status, _) = send(
            router.clone(),
            method.clone(),
            &uri,
            Some(&viewer),
            body.as_deref(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} should be admin-only"
        );
    }

    // A read route still works for the viewer.
    let (status, _) = send(router, Method::GET, "/api/flows", Some(&viewer), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn unknown_flow_returns_not_found_envelope() {
    let (state, _engine, _server_id) = setup().await;
    let token = seed_token(&state, "viewer").await;

    let (status, body) = send(
        app(state),
        Method::GET,
        "/api/flows/9999999",
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    // The non-2xx envelope is `flows::ErrorBody { error, message }`.
    assert_eq!(body["error"], "not_found", "body: {body}");
    assert!(body["message"].is_string(), "body: {body}");
}
