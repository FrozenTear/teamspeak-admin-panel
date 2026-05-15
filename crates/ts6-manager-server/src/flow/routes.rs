//! v1.1 flow-engine REST surface — PURA-242 (F-impl-routes).
//!
//! Implements the contract in `docs/flows/http-api.md` verbatim:
//!
//! | Method   | Path                     | Auth          |
//! | -------- | ------------------------ | ------------- |
//! | `GET`    | `/api/flows`             | `RequireAuth` |
//! | `POST`   | `/api/flows`             | `RequireAdmin`|
//! | `GET`    | `/api/flows/{id}`        | `RequireAuth` |
//! | `PATCH`  | `/api/flows/{id}`        | `RequireAdmin`|
//! | `DELETE` | `/api/flows/{id}`        | `RequireAdmin`|
//! | `POST`   | `/api/flows/{id}/fire`   | `RequireAdmin`|
//! | `GET`    | `/api/flows/{id}/runs`   | `RequireAuth` |
//!
//! ## State
//!
//! The router carries its own [`FlowApiState`] rather than the crate-wide
//! `AppState`. That wrapper bundles `AppState` (so the shared
//! `RequireAuth` / `RequireAdmin` extractors keep working via `FromRef`)
//! with the [`FlowEngineHandle`] the engine layer (PURA-241) exposes and a
//! small per-flow fire-rate bucket. `AppState` itself is left untouched —
//! the `FromRef<FlowApiState> for AppState` impl is exactly the seam the
//! generic extractor design in `auth::extractors` was built for.
//!
//! ## Error envelope
//!
//! Every non-2xx the handlers emit serialises `flows::ErrorBody`
//! (`{ "error": <discriminant>, "message": <human> }`) per `http-api.md`
//! §4. The `401` / `403` rejections are produced upstream by the
//! `RequireAuth` / `RequireAdmin` extractors and use the shared
//! `auth::ErrorResponse` envelope — same split every other router in the
//! crate (`routes::music_bots`, `routes::metrics`) lives with.
//!
//! ## Action dispatch
//!
//! Mounting the routes does not require the production action dispatcher.
//! `main.rs` boots the engine with `BasicDispatcher` (PURA-241): `logLine`
//! actions run for real, the other three kinds fail loudly. Wiring the
//! production dispatcher (`ts6Command` templating, `musicBotCommand`,
//! `webhookOut` + SSRF) is tracked as a follow-up child — it needs the
//! Security Engineer for the `webhookOut` blocklist and is out of scope
//! for this REST-surface ticket.

// Handler / helper fallible functions return `Result<_, Response>` — the
// `Err` variant is an axum `Response`, which is large. That is the
// idiomatic axum error-as-response pattern throughout this module, so the
// `result_large_err` lint is suppressed module-wide rather than per-fn.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use ts6_manager_shared::flows::{
    Action, CreateFlowRequest, ErrorBody, FireFlowRequest, FireFlowResponse, Flow, FlowDefinition,
    FlowId, FlowRun, FlowRunId, FlowRunSummary, ListFlowsResponse, ListRunsResponse,
    UpdateFlowRequest,
};

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAdmin, RequireAuth};
use crate::flow::FlowEngineHandle;
use crate::flow::engine::{FireError, parse_definition};
use crate::flow::trigger::ParsedTrigger;
use crate::repos::bot_flow_runs::{self, BotFlowRun};
use crate::repos::bot_flows::{self, BotFlow, BotFlowUpdate, NewBotFlow};

/// `http-api.md` §3.1 — flow name length cap.
const MAX_NAME_LEN: usize = 120;
/// `http-api.md` §3.1 — action-list length cap.
const MAX_ACTIONS: usize = 8;
/// `http-api.md` §3.5 — default / max run-history page size.
const DEFAULT_RUN_LIMIT: usize = 25;
const MAX_RUN_LIMIT: usize = 200;
/// `http-api.md` §1 — per-flow `POST /fire` soft cap of 1 fire / 2 s.
const FIRE_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Router state for the flow surface. Wraps the crate-wide [`AppState`] so
/// the shared auth extractors compose, and adds the engine handle plus the
/// per-flow fire-rate bucket.
#[derive(Clone)]
pub struct FlowApiState {
    pub app: AppState,
    pub engine: FlowEngineHandle,
    /// Per-flow timestamp of the last accepted `POST /fire`. Guards the
    /// `http-api.md` §1 soft rate-limit. `std::sync::Mutex` — the critical
    /// section is a single map lookup, never held across `.await`.
    fire_limiter: Arc<StdMutex<HashMap<i64, Instant>>>,
}

impl FlowApiState {
    pub fn new(app: AppState, engine: FlowEngineHandle) -> Self {
        Self {
            app,
            engine,
            fire_limiter: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

/// Lets the generic `RequireAuth` / `RequireAdmin` extractors — defined as
/// `impl<S> FromRequestParts<S> where AppState: FromRef<S>` — run unchanged
/// on a `Router<FlowApiState>`.
impl FromRef<FlowApiState> for AppState {
    fn from_ref(state: &FlowApiState) -> AppState {
        state.app.clone()
    }
}

/// Build the flow sub-router. `main.rs` calls `.with_state(FlowApiState)`
/// then `merge`s the result into the top-level app alongside the other
/// eleven routers — the absolute paths line up with `http-api.md` §1.
pub fn router() -> Router<FlowApiState> {
    Router::new()
        .route("/api/flows", get(list_flows).post(create_flow))
        .route(
            "/api/flows/{id}",
            get(get_flow).patch(update_flow).delete(delete_flow),
        )
        .route("/api/flows/{id}/fire", axum::routing::post(fire_flow))
        .route("/api/flows/{id}/runs", get(list_runs))
}

// ---- Error helpers -----------------------------------------------------

/// Construct a `flows::ErrorBody` response. `code` is the stable
/// discriminant clients branch on (`http-api.md` §4); `message` is human.
fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: code.to_string(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn validation(message: impl Into<String>) -> Response {
    error(StatusCode::BAD_REQUEST, "validation", message)
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "flow not found")
}

fn name_taken() -> Response {
    error(
        StatusCode::CONFLICT,
        "name_taken",
        "a flow with this name already exists for the server / virtual server",
    )
}

fn internal() -> Response {
    // Body intentionally vague — `http-api.md` §4 `internal` row.
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

// ---- Validation --------------------------------------------------------

fn validate_name(name: &str) -> Result<(), Response> {
    if name.trim().is_empty() {
        return Err(validation("name must not be empty"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(validation(format!(
            "name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    Ok(())
}

/// `http-api.md` §3.1 — action-list bounds, non-empty command strings, and
/// a parseable trigger (the cron-expression check lives in
/// [`ParsedTrigger::parse`]).
///
/// The per-command whitelist named in §3.1
/// (`flow::engine::commands::mod.rs`) is deferred to the production
/// action-dispatcher follow-up — that module does not exist yet and the
/// whitelist is the dispatcher's contract, not the router's. Here we only
/// reject structurally-broken actions.
fn validate_definition(def: &FlowDefinition) -> Result<(), Response> {
    if def.actions.is_empty() {
        return Err(validation("definition.actions must not be empty"));
    }
    if def.actions.len() > MAX_ACTIONS {
        return Err(validation(format!(
            "definition.actions: at most {MAX_ACTIONS} actions"
        )));
    }
    for (i, action) in def.actions.iter().enumerate() {
        let command = match action {
            Action::Ts6Command { command, .. } | Action::MusicBotCommand { command, .. } => {
                Some(command)
            }
            Action::WebhookOut { .. } | Action::LogLine { .. } => None,
        };
        if let Some(command) = command
            && command.trim().is_empty()
        {
            return Err(validation(format!(
                "definition.actions[{i}].command must not be empty"
            )));
        }
    }
    ParsedTrigger::parse(&def.trigger)
        .map_err(|e| validation(format!("definition.trigger: {e}")))?;
    Ok(())
}

/// Parse a JSON request body, surfacing serde errors as `400 validation`
/// with the serde message (`http-api.md` §4 — e.g. the
/// `trigger.kind: unknown variant` example). Done by hand rather than via
/// `axum::Json` because that extractor answers `422` on a data error and
/// emits a plain-text body — neither matches the spec envelope.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| validation(e.to_string()))
}

// ---- Wire conversion ---------------------------------------------------

/// Compact run shape for `Flow.last_run` and the flatten base of [`FlowRun`].
fn run_summary(run: &BotFlowRun) -> FlowRunSummary {
    FlowRunSummary {
        id: FlowRunId(run.id),
        status: run.status,
        started_at: run.startedAt,
        finished_at: run.finishedAt,
        duration_ms: run
            .finishedAt
            .map(|f| (f - run.startedAt).num_milliseconds().max(0) as u64),
    }
}

fn to_wire_run(run: BotFlowRun) -> FlowRun {
    FlowRun {
        summary: run_summary(&run),
        flow_id: FlowId(run.flowId),
        trigger: run.trigger,
        error: run.error,
        action_results: run.actionResults,
    }
}

/// Translate a `bot_flow` row into the wire [`Flow`]. The JSON-encoded
/// `flowData` column is decoded into the typed [`FlowDefinition`]; a
/// corrupt column is a `500` (it should be impossible — the router is the
/// only writer and validates before every write).
fn to_wire_flow(row: BotFlow, last_run: Option<FlowRunSummary>) -> Result<Flow, Response> {
    let definition = parse_definition(&row.flowData).ok_or_else(internal)?;
    Ok(Flow {
        id: FlowId(row.id),
        name: row.name,
        description: row.description,
        server_config_id: row.serverConfigId,
        virtual_server_id: row.virtualServerId,
        enabled: row.enabled,
        definition,
        created_at: row.createdAt,
        updated_at: row.updatedAt,
        last_run,
    })
}

/// Read `flow.last_run` — the latest run row, projected to a summary.
async fn last_run_summary(
    state: &FlowApiState,
    flow_id: i64,
) -> Result<Option<FlowRunSummary>, Response> {
    let latest = bot_flow_runs::latest_for_flow(&state.app.db, flow_id)
        .await
        .map_err(|_| internal())?;
    Ok(latest.as_ref().map(run_summary))
}

/// Name uniqueness per `(serverConfigId, virtualServerId)` — `http-api.md`
/// §3.1. `exclude` skips the row being PATCHed so a no-op rename passes.
async fn name_is_taken(
    state: &FlowApiState,
    server_config_id: i64,
    virtual_server_id: i64,
    name: &str,
    exclude: Option<i64>,
) -> Result<bool, Response> {
    let siblings = bot_flows::list_for_server(&state.app.db, server_config_id)
        .await
        .map_err(|_| internal())?;
    Ok(siblings
        .into_iter()
        .any(|f| f.virtualServerId == virtual_server_id && f.name == name && Some(f.id) != exclude))
}

// ---- Handlers ----------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListFlowsQuery {
    virtual_server_id: Option<i64>,
    enabled: Option<bool>,
}

/// `GET /api/flows` — list flows, optional `?virtualServerId=` / `?enabled=`.
async fn list_flows(
    _user: RequireAuth,
    State(state): State<FlowApiState>,
    Query(query): Query<ListFlowsQuery>,
) -> Response {
    let rows = match bot_flows::list(&state.app.db).await {
        Ok(rows) => rows,
        Err(_) => return internal(),
    };
    let mut flows = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(vsid) = query.virtual_server_id
            && row.virtualServerId != vsid
        {
            continue;
        }
        if let Some(enabled) = query.enabled
            && row.enabled != enabled
        {
            continue;
        }
        let last_run = match last_run_summary(&state, row.id).await {
            Ok(lr) => lr,
            Err(resp) => return resp,
        };
        match to_wire_flow(row, last_run) {
            Ok(flow) => flows.push(flow),
            Err(resp) => return resp,
        }
    }
    (StatusCode::OK, Json(ListFlowsResponse { flows })).into_response()
}

/// `POST /api/flows` — create a flow (`RequireAdmin`).
async fn create_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    body: Bytes,
) -> Response {
    let req: CreateFlowRequest = match parse_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };
    if let Err(resp) = validate_name(&req.name) {
        return resp;
    }
    if let Err(resp) = validate_definition(&req.definition) {
        return resp;
    }
    match name_is_taken(
        &state,
        req.server_config_id,
        req.virtual_server_id,
        &req.name,
        None,
    )
    .await
    {
        Ok(true) => return name_taken(),
        Ok(false) => {}
        Err(resp) => return resp,
    }

    let flow_data = match serde_json::to_string(&req.definition) {
        Ok(s) => s,
        Err(_) => return internal(),
    };
    let row = match bot_flows::insert(
        &state.app.db,
        NewBotFlow {
            name: req.name,
            description: req.description,
            flowData: flow_data,
            serverConfigId: req.server_config_id,
            virtualServerId: req.virtual_server_id,
            enabled: req.enabled,
        },
    )
    .await
    {
        Ok(row) => row,
        Err(_) => return internal(),
    };

    // Registering the trigger is best-effort — the row is already
    // persisted, so an `enable` failure must not 500 a successful create.
    if row.enabled
        && let Err(e) = state.engine.enable(FlowId(row.id)).await
    {
        tracing::warn!(error = %e, flow.id = row.id, "flow create: engine enable failed");
    }

    match to_wire_flow(row, None) {
        Ok(flow) => (StatusCode::CREATED, Json(flow)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /api/flows/{id}` — fetch one flow.
async fn get_flow(
    _user: RequireAuth,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
) -> Response {
    let row = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    let last_run = match last_run_summary(&state, id).await {
        Ok(lr) => lr,
        Err(resp) => return resp,
    };
    match to_wire_flow(row, last_run) {
        Ok(flow) => (StatusCode::OK, Json(flow)).into_response(),
        Err(resp) => resp,
    }
}

/// `PATCH /api/flows/{id}` — partial update (`RequireAdmin`).
async fn update_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let req: UpdateFlowRequest = match parse_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };
    let current = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };

    // §3.2 — a `definition` swap is only legal while the flow is disabled.
    if req.definition.is_some() && current.enabled {
        return error(
            StatusCode::CONFLICT,
            "definition_swap_locked",
            "cannot replace definition while the flow is enabled; disable it first",
        );
    }

    if let Some(name) = &req.name
        && let Err(resp) = validate_name(name)
    {
        return resp;
    }
    if let Some(definition) = &req.definition
        && let Err(resp) = validate_definition(definition)
    {
        return resp;
    }

    // Uniqueness check against the post-patch `(name, virtualServerId)`.
    let effective_name = req.name.as_deref().unwrap_or(&current.name);
    let effective_vsid = req.virtual_server_id.unwrap_or(current.virtualServerId);
    if (req.name.is_some() || req.virtual_server_id.is_some())
        && match name_is_taken(
            &state,
            current.serverConfigId,
            effective_vsid,
            effective_name,
            Some(id),
        )
        .await
        {
            Ok(taken) => taken,
            Err(resp) => return resp,
        }
    {
        return name_taken();
    }

    let mut update = BotFlowUpdate {
        name: req.name.clone(),
        description: req.description.clone(),
        virtualServerId: req.virtual_server_id,
        enabled: req.enabled,
        flowData: None,
    };
    if let Some(definition) = &req.definition {
        update.flowData = Some(match serde_json::to_string(definition) {
            Ok(s) => s,
            Err(_) => return internal(),
        });
    }

    let updated = match bot_flows::update(&state.app.db, id, update).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };

    // Engine trigger registration follows the enabled-state transition.
    match (current.enabled, updated.enabled) {
        (false, true) => {
            if let Err(e) = state.engine.enable(FlowId(id)).await {
                tracing::warn!(error = %e, flow.id = id, "flow patch: engine enable failed");
            }
        }
        (true, false) => state.engine.disable(FlowId(id)),
        _ => {}
    }

    let last_run = match last_run_summary(&state, id).await {
        Ok(lr) => lr,
        Err(resp) => return resp,
    };
    match to_wire_flow(updated, last_run) {
        Ok(flow) => (StatusCode::OK, Json(flow)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteFlowQuery {
    #[serde(default)]
    force: bool,
}

/// `DELETE /api/flows/{id}` — delete a flow (`RequireAdmin`).
///
/// Default: `409 run_in_flight` if a run is in flight. `?force=true` marks
/// in-flight runs `interrupted` first, then deletes. Run rows cascade via
/// the `bot_flow_cascade_runs` schema event (migration `0009`).
async fn delete_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
    Query(query): Query<DeleteFlowQuery>,
) -> Response {
    if bot_flows::find_by_id(&state.app.db, id)
        .await
        .map(|f| f.is_none())
        .unwrap_or(true)
    {
        // Either the flow is gone or the read failed; a missing flow is the
        // far more likely cause and `404` is the safe answer for both.
        return not_found();
    }

    // Per-flow serial execution means at most one in-flight run per flow,
    // and it is always the most recently started — `latest_for_flow` is
    // sufficient to detect it.
    let in_flight = match bot_flow_runs::latest_for_flow(&state.app.db, id).await {
        Ok(Some(run))
            if matches!(
                run.status,
                ts6_manager_shared::flows::FlowRunStatus::InFlight
            ) =>
        {
            Some(run.id)
        }
        Ok(_) => None,
        Err(_) => return internal(),
    };

    if let Some(run_id) = in_flight {
        if query.force {
            if let Err(e) = state.engine.interrupt_runs_for_flow(FlowId(id)).await {
                tracing::warn!(error = %e, flow.id = id, "force-delete: interrupt sweep failed");
                return internal();
            }
        } else {
            return error(
                StatusCode::CONFLICT,
                "run_in_flight",
                format!("run {run_id} is in-flight; pass ?force=true to interrupt"),
            );
        }
    }

    state.engine.disable(FlowId(id));
    match bot_flows::delete(&state.app.db, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => internal(),
    }
}

/// `POST /api/flows/{id}/fire` — manual fire (`RequireAdmin`).
async fn fire_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    // Empty body is allowed — `FireFlowRequest` defaults to no context.
    let req: FireFlowRequest = if body.is_empty() {
        FireFlowRequest::default()
    } else {
        match parse_body(&body) {
            Ok(req) => req,
            Err(resp) => return resp,
        }
    };
    let context = match req.context {
        Some(serde_json::Value::Object(map)) => Some(map),
        Some(_) => return validation("context must be a JSON object"),
        None => None,
    };

    // §1 per-flow soft rate-limit (1 fire / 2 s).
    {
        let mut limiter = state.fire_limiter.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(prev) = limiter.get(&id)
            && prev.elapsed() < FIRE_MIN_INTERVAL
        {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "per-flow fire rate limit hit; retry shortly",
            );
        }
        limiter.insert(id, Instant::now());
    }

    let run_id = match state.engine.fire(FlowId(id), context).await {
        Ok(run_id) => run_id,
        Err(FireError::NotFound(_)) => return not_found(),
        Err(FireError::Busy(_)) => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "a run for this flow is still in flight",
            );
        }
        Err(FireError::EngineSaturated) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_saturated",
                "flow engine is saturated; retry shortly",
            );
        }
        Err(FireError::MalformedFlow(..)) | Err(FireError::Persist(_)) => return internal(),
    };

    // The engine returns only the row id; read the row back for the
    // authoritative `startedAt`. A race that loses the row falls back to
    // `now` so the 202 still carries a sane timestamp.
    let started_at = bot_flow_runs::find_by_id(&state.app.db, run_id.0)
        .await
        .ok()
        .flatten()
        .map(|run| run.startedAt)
        .unwrap_or_else(Utc::now);

    (
        StatusCode::ACCEPTED,
        Json(FireFlowResponse {
            run_id,
            flow_id: FlowId(id),
            started_at,
        }),
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRunsQuery {
    limit: Option<usize>,
    cursor: Option<i64>,
}

/// `GET /api/flows/{id}/runs` — keyset-paginated run history.
async fn list_runs(
    _user: RequireAuth,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
    Query(query): Query<ListRunsQuery>,
) -> Response {
    if bot_flows::find_by_id(&state.app.db, id)
        .await
        .map(|f| f.is_none())
        .unwrap_or(true)
    {
        return not_found();
    }

    let limit = query
        .limit
        .unwrap_or(DEFAULT_RUN_LIMIT)
        .clamp(1, MAX_RUN_LIMIT);
    let rows = match bot_flow_runs::list_for_flow(&state.app.db, id, limit, query.cursor).await {
        Ok(rows) => rows,
        Err(_) => return internal(),
    };
    // A full page means there may be more; the keyset cursor is the last
    // (smallest) run id, since the page is ordered `startedAt DESC, id DESC`.
    let next_cursor = if rows.len() == limit {
        rows.last().map(|r| FlowRunId(r.id))
    } else {
        None
    };
    let runs = rows.into_iter().map(to_wire_run).collect();

    (StatusCode::OK, Json(ListRunsResponse { runs, next_cursor })).into_response()
}
