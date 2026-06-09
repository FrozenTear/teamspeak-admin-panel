//! Flow-engine REST surface tests — PURA-242 (v1.1) extended for the v2
//! graph contract (PURA-278, `docs/flows/v2/http-api.md` §7).
//!
//! Runs the real axum router (so the `RequireAuth` / `RequireAdmin`
//! extractor chain and the `BasicDispatcher`-backed engine are in the
//! loop) against an in-memory SurrealDB. Covers the §7 surface: graph
//! `POST` round-trip + `400 graph_invalid`, the legacy `definition` body
//! stored as a v2 envelope, `POST /validate` errors+warnings, `POST
//! /convert` + `409`s, `GET /runs/{runId}` populated-vs-`[]`, the PATCH
//! definition-swap lock, the `RequireAdmin` gate, and the `ErrorBody` shape.
//!
//! Filter: `cargo test -p ts6-manager-server flow::routes_tests`.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;
use ts6_manager_shared::flows::v2::{NodeId, NodeResult, NodeStatus};
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
    crypto::init("test-seed-pura-278-flow-routes");

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
        yt_api_key: Arc::new(std::sync::RwLock::new(None)),
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
            username: format!("flow-{role}-{}", uuid_like()),
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

/// A cheap unique suffix so two `seed_token` calls in one test never collide
/// on the `users.username` unique index.
fn uuid_like() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
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

/// A legacy v1.1 `{ definition }` create body — still accepted as a
/// back-compat courtesy (`http-api.md` §2.2).
fn legacy_flow_body(name: &str, server_id: i64, enabled: bool) -> String {
    json!({
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

/// A minimal valid v2 graph: a trigger node feeding one action node.
fn valid_graph() -> Value {
    json!({
        "nodes": [
            { "id": "t", "position": { "x": 0.0, "y": 0.0 },
              "kind": "trigger", "config": { "kind": "manualFire" } },
            { "id": "a", "position": { "x": 0.0, "y": 120.0 },
              "kind": "action", "config": { "kind": "logLine", "message": "{{ trigger.kind }}" } }
        ],
        "edges": [
            { "id": "e0", "from": { "node": "t", "port": "out" },
              "to": { "node": "a", "port": "in" } }
        ]
    })
}

/// A v2 `{ graph }` create body.
fn graph_flow_body(name: &str, server_id: i64, enabled: bool) -> String {
    json!({
        "name": name,
        "serverConfigId": server_id,
        "virtualServerId": 1,
        "enabled": enabled,
        "graph": valid_graph()
    })
    .to_string()
}

// ── Create ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_with_graph_body_round_trips_as_v2() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&graph_flow_body("graph-flow", server_id, false)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "graph-flow");
    assert_eq!(body["flowVersion"], 2, "body: {body}");
    assert!(body["graph"]["nodes"].is_array(), "body: {body}");
    assert_eq!(body["graph"]["nodes"][0]["kind"], "trigger");
    assert!(
        body["definition"].is_null(),
        "v2 flow carries no definition"
    );
    assert!(body["lastRun"].is_null());
}

#[tokio::test]
async fn create_with_legacy_definition_is_stored_as_a_v2_envelope() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    // A v1.1-shaped `{ definition }` body is projected and stored as v2.
    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&legacy_flow_body("legacy-flow", server_id, false)),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    // `http-api.md` §7.2 — read-back is `flowVersion: 2`, a graph.
    assert_eq!(body["flowVersion"], 2, "body: {body}");
    assert!(body["graph"]["nodes"].is_array(), "body: {body}");
    // The projected path graph: trigger + one action node.
    assert_eq!(body["graph"]["nodes"][0]["kind"], "trigger");
    assert_eq!(body["graph"]["nodes"][1]["kind"], "action");
}

#[tokio::test]
async fn create_graph_rejects_a_cycle_with_graph_invalid() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    // t → a → b → a — a cycle.
    let body = json!({
        "name": "cyclic",
        "serverConfigId": server_id,
        "virtualServerId": 1,
        "graph": {
            "nodes": [
                { "id": "t", "position": { "x": 0.0, "y": 0.0 },
                  "kind": "trigger", "config": { "kind": "manualFire" } },
                { "id": "a", "position": { "x": 0.0, "y": 1.0 },
                  "kind": "action", "config": { "kind": "logLine", "message": "x" } },
                { "id": "b", "position": { "x": 0.0, "y": 2.0 },
                  "kind": "action", "config": { "kind": "logLine", "message": "y" } }
            ],
            "edges": [
                { "id": "e0", "from": { "node": "t", "port": "out" }, "to": { "node": "a", "port": "in" } },
                { "id": "e1", "from": { "node": "a", "port": "out" }, "to": { "node": "b", "port": "in" } },
                { "id": "e2", "from": { "node": "b", "port": "out" }, "to": { "node": "a", "port": "in" } }
            ]
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
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"], "graph_invalid", "body: {body}");
    let codes: Vec<&str> = body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"graph_cycle"), "codes: {codes:?}");
}

#[tokio::test]
async fn create_graph_rejects_an_unknown_port() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let body = json!({
        "name": "bad-port",
        "serverConfigId": server_id,
        "virtualServerId": 1,
        "graph": {
            "nodes": [
                { "id": "t", "position": { "x": 0.0, "y": 0.0 },
                  "kind": "trigger", "config": { "kind": "manualFire" } },
                { "id": "a", "position": { "x": 0.0, "y": 1.0 },
                  "kind": "action", "config": { "kind": "logLine", "message": "x" } }
            ],
            "edges": [
                { "id": "e0", "from": { "node": "t", "port": "bogus" }, "to": { "node": "a", "port": "in" } }
            ]
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
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"], "graph_invalid", "body: {body}");
    let codes: Vec<&str> = body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"unknown_port"), "codes: {codes:?}");
}

#[tokio::test]
async fn create_flow_rejects_bad_cron() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    let body = json!({
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

    let body = graph_flow_body("dupe", server_id, false);
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

// ── Validate ────────────────────────────────────────────────────────────

#[tokio::test]
async fn validate_reports_errors_for_a_bad_graph() {
    let (state, _engine, _server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    // A node with no inbound edge — `port_unconnected`.
    let body = json!({
        "graph": {
            "nodes": [
                { "id": "t", "position": { "x": 0.0, "y": 0.0 },
                  "kind": "trigger", "config": { "kind": "manualFire" } },
                { "id": "stranded", "position": { "x": 0.0, "y": 1.0 },
                  "kind": "action", "config": { "kind": "logLine", "message": "x" } }
            ],
            "edges": []
        }
    })
    .to_string();

    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows/validate",
        Some(&token),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["valid"], false, "body: {body}");
    assert!(
        !body["errors"].as_array().unwrap().is_empty(),
        "body: {body}"
    );
}

#[tokio::test]
async fn validate_reports_warnings_for_a_valid_graph() {
    let (state, _engine, _server_id) = setup().await;
    let token = seed_token(&state, "admin").await;

    // trigger → branch; the named case `vip` is left unwired (a dead route,
    // advisory) while `default` carries the flow on — a valid graph.
    let body = json!({
        "graph": {
            "nodes": [
                { "id": "t", "position": { "x": 0.0, "y": 0.0 },
                  "kind": "trigger", "config": { "kind": "manualFire" } },
                { "id": "route", "position": { "x": 0.0, "y": 1.0 },
                  "kind": "branch", "cases": [ { "label": "vip", "when": "true" } ] },
                { "id": "a", "position": { "x": 0.0, "y": 2.0 },
                  "kind": "action", "config": { "kind": "logLine", "message": "x" } }
            ],
            "edges": [
                { "id": "e0", "from": { "node": "t", "port": "out" }, "to": { "node": "route", "port": "in" } },
                { "id": "e1", "from": { "node": "route", "port": "default" }, "to": { "node": "a", "port": "in" } }
            ]
        }
    })
    .to_string();

    let (status, body) = send(
        app(state),
        Method::POST,
        "/api/flows/validate",
        Some(&token),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["valid"], true, "warnings never block: {body}");
    let warn_codes: Vec<&str> = body["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(
        warn_codes.contains(&"unconnected_case"),
        "warnings: {warn_codes:?}"
    );
}

// ── Convert ─────────────────────────────────────────────────────────────

/// Insert a legacy (`flowVersion: 1`) flow row directly — the only way to
/// produce one now that the API always stores v2 envelopes.
async fn insert_legacy_flow(
    state: &FlowApiState,
    name: &str,
    server_id: i64,
    enabled: bool,
) -> i64 {
    bot_flows::insert(
        &state.app.db,
        bot_flows::NewBotFlow {
            name: name.into(),
            description: None,
            flowData: json!({
                "trigger": { "kind": "manualFire" },
                "actions": [ { "kind": "logLine", "message": "x" } ]
            })
            .to_string(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled,
        },
    )
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn convert_projects_a_legacy_flow_then_409s_on_a_second_call() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let id = insert_legacy_flow(&state, "to-convert", server_id, false).await;
    let router = app(state);

    let (status, body) = send(
        router.clone(),
        Method::POST,
        &format!("/api/flows/{id}/convert"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["flowVersion"], 2, "body: {body}");
    assert!(body["graph"]["nodes"].is_array(), "body: {body}");

    // A second convert — already a v2 graph.
    let (status, body) = send(
        router,
        Method::POST,
        &format!("/api/flows/{id}/convert"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "already_graph", "body: {body}");
}

#[tokio::test]
async fn convert_blocked_while_the_flow_is_enabled() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let id = insert_legacy_flow(&state, "enabled-legacy", server_id, true).await;

    let (status, body) = send(
        app(state),
        Method::POST,
        &format!("/api/flows/{id}/convert"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "definition_swap_locked", "body: {body}");
}

// ── Run detail ──────────────────────────────────────────────────────────

#[tokio::test]
async fn get_run_detail_carries_node_results_for_a_v2_run() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "viewer").await;
    let flow_id = insert_legacy_flow(&state, "with-runs", server_id, false).await;

    // A v2 run row — `nodeResults` populated, `actionResults` empty.
    let v2_run = bot_flow_runs::insert(
        &state.app.db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: json!({ "kind": "manualFire" }),
            status: FlowRunStatus::Ok,
            actionResults: vec![],
            nodeResults: vec![NodeResult {
                node_id: NodeId("a".into()),
                kind: "action".into(),
                status: NodeStatus::Ok,
                started_at: Utc::now(),
                finished_at: Some(Utc::now()),
                duration_ms: Some(7),
                error: None,
                output: Some(json!({ "ok": true })),
            }],
        },
    )
    .await
    .unwrap();

    // A legacy run row — `nodeResults` empty.
    let legacy_run = bot_flow_runs::insert(
        &state.app.db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: json!({ "kind": "manualFire" }),
            status: FlowRunStatus::Ok,
            actionResults: vec![ActionResult {
                index: 0,
                kind: "logLine".into(),
                status: ActionStatus::Ok,
                duration_ms: 1,
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
        Method::GET,
        &format!("/api/flows/{flow_id}/runs/{}", v2_run.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["nodeResults"].as_array().unwrap().len(),
        1,
        "body: {body}"
    );
    assert_eq!(body["nodeResults"][0]["nodeId"], "a");

    let (status, body) = send(
        router.clone(),
        Method::GET,
        &format!("/api/flows/{flow_id}/runs/{}", legacy_run.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["nodeResults"], json!([]), "legacy run: {body}");

    // An unknown run id is a 404 with the `ErrorBody` envelope.
    let (status, body) = send(
        router,
        Method::GET,
        &format!("/api/flows/{flow_id}/runs/9999999"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not_found", "body: {body}");
}

#[tokio::test]
async fn get_run_404s_when_the_run_belongs_to_another_flow() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "viewer").await;
    let flow_a = insert_legacy_flow(&state, "flow-a", server_id, false).await;
    let flow_b = insert_legacy_flow(&state, "flow-b", server_id, false).await;

    let run = bot_flow_runs::insert(
        &state.app.db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_b,
            trigger: json!({ "kind": "manualFire" }),
            status: FlowRunStatus::Ok,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();

    // The run is real, but addressed under the wrong flow id.
    let (status, body) = send(
        app(state),
        Method::GET,
        &format!("/api/flows/{flow_a}/runs/{}", run.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

// ── Patch / delete / fire (v1.1 carry-over) ─────────────────────────────

#[tokio::test]
async fn patch_graph_swap_blocked_while_enabled() {
    let (state, _engine, server_id) = setup().await;
    let token = seed_token(&state, "admin").await;
    let router = app(state);

    let (status, created) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&token),
        Some(&graph_flow_body("live", server_id, true)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_i64().unwrap();

    // Swapping the graph on a live flow is rejected.
    let patch = json!({ "graph": valid_graph() }).to_string();
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
    let flow_id = insert_legacy_flow(&state, "deletable", server_id, false).await;

    // A run row stuck in flight blocks the default delete.
    bot_flow_runs::insert(
        &state.app.db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: json!({ "kind": "manualFire" }),
            status: FlowRunStatus::InFlight,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();

    let router = app(state);
    let (status, body) = send(
        router.clone(),
        Method::DELETE,
        &format!("/api/flows/{flow_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "run_in_flight", "body: {body}");

    let (status, _) = send(
        router,
        Method::DELETE,
        &format!("/api/flows/{flow_id}?force=true"),
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
        Some(&graph_flow_body("fire-me", server_id, false)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
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
            // `http-api.md` §3.2 — the list stays light: no nodeResults.
            assert_eq!(runs["runs"][0]["nodeResults"], json!([]));
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fired run never appeared on GET /runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── Auth ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_routes_require_admin() {
    let (state, _engine, server_id) = setup().await;
    let admin = seed_token(&state, "admin").await;
    let viewer = seed_token(&state, "viewer").await;

    // Seed one flow with the admin token so the write routes have a target.
    let router = app(state);
    let (status, created) = send(
        router.clone(),
        Method::POST,
        "/api/flows",
        Some(&admin),
        Some(&graph_flow_body("guarded", server_id, false)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_i64().unwrap();

    // Every write route — including the v2 additions — rejects a non-admin.
    let validate_body = json!({ "graph": valid_graph() }).to_string();
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/flows".to_string(),
            Some(graph_flow_body("viewer-attempt", server_id, false)),
        ),
        (
            Method::PATCH,
            format!("/api/flows/{id}"),
            Some(r#"{"enabled":true}"#.to_string()),
        ),
        (Method::DELETE, format!("/api/flows/{id}"), None),
        (Method::POST, format!("/api/flows/{id}/fire"), None),
        (Method::POST, format!("/api/flows/{id}/convert"), None),
        (
            Method::POST,
            "/api/flows/validate".to_string(),
            Some(validate_body.clone()),
        ),
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
