//! Flow-engine REST surface — v1.1 (PURA-242) extended to the v2 graph
//! contract (PURA-278, `docs/flows/v2/http-api.md`).
//!
//! | Method   | Path                            | Auth          |
//! | -------- | ------------------------------- | ------------- |
//! | `GET`    | `/api/flows`                    | `RequireAuth` |
//! | `POST`   | `/api/flows`                    | `RequireAdmin`|
//! | `GET`    | `/api/flows/{id}`               | `RequireAuth` |
//! | `PATCH`  | `/api/flows/{id}`               | `RequireAdmin`|
//! | `DELETE` | `/api/flows/{id}`               | `RequireAdmin`|
//! | `POST`   | `/api/flows/{id}/fire`          | `RequireAdmin`|
//! | `GET`    | `/api/flows/{id}/runs`          | `RequireAuth` |
//! | `GET`    | `/api/flows/{id}/runs/{runId}`  | `RequireAuth` |
//! | `POST`   | `/api/flows/validate`           | `RequireAdmin`|
//! | `POST`   | `/api/flows/{id}/convert`       | `RequireAdmin`|
//!
//! ## v2 wire surface (`http-api.md` §2)
//!
//! `POST` / `PATCH` accept the untagged spec: a `{ graph }` body (v2 —
//! canonical) or a back-compat `{ definition }` body (v1.1 — projected via
//! [`project_legacy`] and stored as a v2 envelope). A graph body runs
//! structural + expression + sub-flow validation before insert; a
//! structural failure answers `400` with the [`GraphInvalidBody`] `errors`
//! array rather than the bare `ErrorBody`. Responses are the v2 [`FlowView`]
//! / [`FlowRunView`], each carrying an explicit `flowVersion`.
//!
//! ## State
//!
//! The router carries [`FlowApiState`] — `AppState` (so the shared
//! `RequireAuth` / `RequireAdmin` extractors compose via `FromRef`) plus the
//! [`FlowEngineHandle`] and a per-flow fire-rate bucket.
//!
//! ## Error envelope
//!
//! Non-2xx bodies serialise `flows::ErrorBody` (`{ error, message }`) per
//! `http-api.md` §4 — except a graph-validation `400`, which carries the
//! `errors` array ([`GraphInvalidBody`]). `401` / `403` come from the
//! extractors and use the shared `auth::ErrorResponse` envelope.

// Handler / helper fallible functions return `Result<_, Response>` — the
// `Err` variant is an axum `Response`, which is large. That is the
// idiomatic axum error-as-response pattern throughout this module, so the
// `result_large_err` lint is suppressed module-wide rather than per-fn.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use ts6_manager_shared::flows::v2::{
    CreateFlowBody, FlowGraph, FlowRunSummaryView, FlowRunView, FlowSpec, FlowVersion, FlowView,
    GraphInvalidBody, ListFlowsView, ListRunsView, UpdateFlowBody, ValidateGraphRequest,
    ValidateGraphResponse, ValidationError, decode_flow_data, encode_flow_data, flow_version,
    project_legacy,
};
use ts6_manager_shared::flows::{
    Action, ErrorBody, FireFlowRequest, FireFlowResponse, FlowDefinition, FlowId, FlowRunId,
};

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAdmin, RequireAuth};
use crate::flow::FlowEngineHandle;
use crate::flow::engine::{FireError, commands, graph, parse_definition};
use crate::flow::trigger::ParsedTrigger;
use crate::repos::bot_flow_runs::{self, BotFlowRun};
use crate::repos::bot_flows::{self, BotFlow, BotFlowUpdate, NewBotFlow};

/// `http-api.md` §3.1 — flow name length cap.
const MAX_NAME_LEN: usize = 120;
/// `http-api.md` §3.1 — action-list length cap (legacy `definition` body).
const MAX_ACTIONS: usize = 8;
/// `http-api.md` §3.5 — default / max run-history page size.
const DEFAULT_RUN_LIMIT: usize = 25;
const MAX_RUN_LIMIT: usize = 200;
/// `http-api.md` §1 — per-flow `POST /fire` soft cap of 1 fire / 2 s.
const FIRE_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// Sub-flow reference-graph traversal cap — matches the engine's runtime
/// `MAX_DEPTH`. Nesting past this is reported as `subflow_cycle`.
const SUBFLOW_MAX_DEPTH: usize = 5;

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
/// then `merge`s the result into the top-level app. The static
/// `/api/flows/validate` path coexists with the `/api/flows/{id}` param
/// route — `matchit` resolves the static segment first.
pub fn router() -> Router<FlowApiState> {
    Router::new()
        .route("/api/flows", get(list_flows).post(create_flow))
        .route(
            "/api/flows/{id}",
            get(get_flow).patch(update_flow).delete(delete_flow),
        )
        .route("/api/flows/validate", post(validate_graph_route))
        .route("/api/flows/{id}/fire", post(fire_flow))
        .route("/api/flows/{id}/convert", post(convert_flow))
        .route("/api/flows/{id}/runs", get(list_runs))
        .route("/api/flows/{id}/runs/{runId}", get(get_run))
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

/// `400 graph_invalid` — `http-api.md` §4. Carries the full `errors` array
/// so the canvas can render every structural failure inline.
fn graph_invalid(errors: Vec<ValidationError>) -> Response {
    (StatusCode::BAD_REQUEST, Json(GraphInvalidBody::new(errors))).into_response()
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

/// Legacy `definition`-body validation (`http-api.md` §3.1) — action-list
/// bounds, the per-command whitelist, and a parseable trigger. Applied only
/// to a back-compat `{ definition }` body; a `{ graph }` body goes through
/// [`graph::validate_graph`] instead.
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
        match action {
            Action::Ts6Command { command, args } => {
                commands::validate_ts6_command(command, args)
                    .map_err(|e| validation(format!("definition.actions[{i}]: {e}")))?;
            }
            Action::MusicBotCommand { command, args, .. } => {
                commands::validate_music_bot_command(command, args)
                    .map_err(|e| validation(format!("definition.actions[{i}]: {e}")))?;
            }
            Action::WebhookOut { url, .. } => {
                if url.trim().is_empty() {
                    return Err(validation(format!(
                        "definition.actions[{i}].url must not be empty"
                    )));
                }
            }
            Action::LogLine { .. } => {}
            Action::Moderate { rule_key, .. } => {
                if rule_key.trim().is_empty() {
                    return Err(validation(format!(
                        "definition.actions[{i}].ruleKey must not be empty"
                    )));
                }
            }
        }
    }
    ParsedTrigger::parse(&def.trigger)
        .map_err(|e| validation(format!("definition.trigger: {e}")))?;
    Ok(())
}

/// Parse a JSON request body, surfacing serde errors as `400 validation`
/// with the serde message (`http-api.md` §4). Done by hand rather than via
/// `axum::Json` because that extractor answers `422` with a plain-text body
/// — neither matches the spec envelope.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| validation(e.to_string()))
}

// ---- Spec handling -----------------------------------------------------

/// Sub-flow ids a graph references via its `subflow` / `parallel` nodes.
fn referenced_subflows(graph: &FlowGraph) -> Vec<FlowId> {
    use ts6_manager_shared::flows::v2::NodeKind;
    graph
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Subflow { sub_flow_id } => Some(*sub_flow_id),
            NodeKind::Parallel { sub_flow_id, .. } => Some(*sub_flow_id),
            _ => None,
        })
        .collect()
}

/// `subflow_missing` / `subflow_cycle` (`http-api.md` §6) — a DB-backed
/// pass that [`graph::validate_graph`] cannot do (it has no flow set).
/// Walks the static sub-flow reference graph: an unknown id is
/// `subflow_missing`; nesting past [`SUBFLOW_MAX_DEPTH`] — which only an
/// unbounded reference cycle (or pathological nesting) produces — is
/// `subflow_cycle`. A DB read failure surfaces as `Err(internal)`.
async fn subflow_errors(
    state: &FlowApiState,
    graph: &FlowGraph,
) -> Result<Vec<ValidationError>, Response> {
    let mut errors = Vec::new();
    let mut seen_missing: HashSet<i64> = HashSet::new();
    let mut cycle_flagged = false;
    let mut work: Vec<(i64, usize)> = referenced_subflows(graph)
        .into_iter()
        .map(|f| (f.0, 1))
        .collect();

    while let Some((fid, depth)) = work.pop() {
        if depth > SUBFLOW_MAX_DEPTH {
            if !cycle_flagged {
                errors.push(ValidationError::new(
                    "subflow_cycle",
                    format!(
                        "sub-flow references nest deeper than the depth-{SUBFLOW_MAX_DEPTH} cap \
                         — a reference cycle is the usual cause"
                    ),
                ));
                cycle_flagged = true;
            }
            continue;
        }
        match bot_flows::find_by_id(&state.app.db, fid).await {
            Ok(Some(row)) => {
                if let Ok(child) = decode_flow_data(&row.flowData) {
                    for c in referenced_subflows(&child) {
                        work.push((c.0, depth + 1));
                    }
                }
            }
            Ok(None) => {
                if seen_missing.insert(fid) {
                    errors.push(ValidationError::new(
                        "subflow_missing",
                        format!(
                            "sub-flow {fid} is referenced by a subflow/parallel node but does \
                             not exist"
                        ),
                    ));
                }
            }
            Err(_) => return Err(internal()),
        }
    }
    Ok(errors)
}

/// Validate a graph and encode it to a `flowData` blob. A structural /
/// expression / sub-flow failure short-circuits to `400 graph_invalid`.
async fn graph_flow_data(state: &FlowApiState, graph: &FlowGraph) -> Result<String, Response> {
    let report = graph::validate_graph(graph);
    let mut errors = report.errors;
    errors.extend(graph::validate_expressions(graph));
    errors.extend(subflow_errors(state, graph).await?);
    if !errors.is_empty() {
        return Err(graph_invalid(errors));
    }
    Ok(encode_flow_data(graph))
}

/// Validate a [`FlowSpec`] and encode it to the canonical v2 `flowData`
/// envelope. A legacy `{ definition }` body is whitelist-validated then
/// projected ([`project_legacy`]) — `http-api.md` §2.2 stores it as a v2
/// envelope, so a v1.1-shaped body never produces a `flowVersion: 1` row.
async fn spec_flow_data(state: &FlowApiState, spec: &FlowSpec) -> Result<String, Response> {
    match spec {
        FlowSpec::Graph { graph } => graph_flow_data(state, graph).await,
        FlowSpec::Legacy { definition } => {
            validate_definition(definition)?;
            Ok(encode_flow_data(&project_legacy(definition)))
        }
    }
}

// ---- Wire conversion ---------------------------------------------------

/// Compact run summary with the flow's [`FlowVersion`] badge.
fn run_summary_view(run: &BotFlowRun, flow_version: FlowVersion) -> FlowRunSummaryView {
    FlowRunSummaryView {
        id: FlowRunId(run.id),
        status: run.status,
        flow_version,
        started_at: run.startedAt,
        finished_at: run.finishedAt,
        duration_ms: run
            .finishedAt
            .map(|f| (f - run.startedAt).num_milliseconds().max(0) as u64),
    }
}

/// Project a run row to the wire [`FlowRunView`]. The list endpoint passes
/// `include_node_results = false` to stay light (`http-api.md` §3.2); the
/// `runs/{runId}` detail endpoint passes `true`.
fn to_run_view(
    run: BotFlowRun,
    flow_version: FlowVersion,
    include_node_results: bool,
) -> FlowRunView {
    let summary = run_summary_view(&run, flow_version);
    FlowRunView {
        summary,
        flow_id: FlowId(run.flowId),
        trigger: run.trigger,
        error: run.error,
        action_results: run.actionResults,
        node_results: if include_node_results {
            run.nodeResults
        } else {
            Vec::new()
        },
    }
}

/// Translate a `bot_flow` row into the wire [`FlowView`]. The `flowData`
/// column decodes into a v2 `graph` (a `version: 2` envelope) or a v1.1
/// `definition` (a row with no `version` key); a corrupt column is a `500`
/// — the router validates before every write, so it should be impossible.
fn to_flow_view(
    row: BotFlow,
    version: FlowVersion,
    last_run: Option<FlowRunSummaryView>,
) -> Result<FlowView, Response> {
    let (graph, definition) = if version == 2 {
        (
            Some(decode_flow_data(&row.flowData).map_err(|_| internal())?),
            None,
        )
    } else {
        (
            None,
            Some(parse_definition(&row.flowData).ok_or_else(internal)?),
        )
    };
    Ok(FlowView {
        id: FlowId(row.id),
        name: row.name,
        description: row.description,
        server_config_id: row.serverConfigId,
        virtual_server_id: row.virtualServerId,
        enabled: row.enabled,
        flow_version: version,
        graph,
        definition,
        created_at: row.createdAt,
        updated_at: row.updatedAt,
        last_run,
    })
}

/// The `flowVersion` a `flowData` blob represents — `1` for a legacy linear
/// flow, `2` for a graph envelope. A blob that is not even valid JSON is a
/// corrupt row → `500`.
fn version_of(flow_data: &str) -> Result<FlowVersion, Response> {
    flow_version(flow_data).map_err(|_| internal())
}

/// Read `flow.lastRun` — the latest run row, projected to a summary.
async fn last_run_summary_view(
    state: &FlowApiState,
    flow_id: i64,
    flow_version: FlowVersion,
) -> Result<Option<FlowRunSummaryView>, Response> {
    let latest = bot_flow_runs::latest_for_flow(&state.app.db, flow_id)
        .await
        .map_err(|_| internal())?;
    Ok(latest.as_ref().map(|r| run_summary_view(r, flow_version)))
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
        let version = match version_of(&row.flowData) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let last_run = match last_run_summary_view(&state, row.id, version).await {
            Ok(lr) => lr,
            Err(resp) => return resp,
        };
        match to_flow_view(row, version, last_run) {
            Ok(flow) => flows.push(flow),
            Err(resp) => return resp,
        }
    }
    (StatusCode::OK, Json(ListFlowsView { flows })).into_response()
}

/// `POST /api/flows` — create a flow (`RequireAdmin`). Accepts the untagged
/// `{ graph }` / `{ definition }` spec; a graph body is structurally
/// validated before insert.
async fn create_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    body: Bytes,
) -> Response {
    let req: CreateFlowBody = match parse_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };
    let spec = match req.spec() {
        Ok(spec) => spec,
        Err(msg) => return validation(msg),
    };
    if let Err(resp) = validate_name(&req.name) {
        return resp;
    }
    let flow_data = match spec_flow_data(&state, &spec).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
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

    let version = match version_of(&row.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match to_flow_view(row, version, None) {
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
    let version = match version_of(&row.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let last_run = match last_run_summary_view(&state, id, version).await {
        Ok(lr) => lr,
        Err(resp) => return resp,
    };
    match to_flow_view(row, version, last_run) {
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
    let req: UpdateFlowBody = match parse_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };
    let spec = match req.spec() {
        Ok(s) => s,
        Err(msg) => return validation(msg),
    };
    let current = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };

    // §4 — a graph swap is only legal while the flow is disabled.
    if spec.is_some() && current.enabled {
        return error(
            StatusCode::CONFLICT,
            "definition_swap_locked",
            "cannot replace the definition while the flow is enabled; disable it first",
        );
    }

    if let Some(name) = &req.name
        && let Err(resp) = validate_name(name)
    {
        return resp;
    }

    // A graph swap is validated + encoded ahead of the write.
    let flow_data = match &spec {
        Some(s) => match spec_flow_data(&state, s).await {
            Ok(blob) => Some(blob),
            Err(resp) => return resp,
        },
        None => None,
    };

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

    let update = BotFlowUpdate {
        name: req.name.clone(),
        description: req.description.clone(),
        virtualServerId: req.virtual_server_id,
        enabled: req.enabled,
        flowData: flow_data,
    };

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

    let version = match version_of(&updated.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let last_run = match last_run_summary_view(&state, id, version).await {
        Ok(lr) => lr,
        Err(resp) => return resp,
    };
    match to_flow_view(updated, version, last_run) {
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

/// `POST /api/flows/validate` — structural + expression + sub-flow
/// validation of a graph without persisting (`http-api.md` §3.1). The
/// canvas calls this on every meaningful edit. `RequireAdmin`.
async fn validate_graph_route(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    body: Bytes,
) -> Response {
    let req: ValidateGraphRequest = match parse_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };
    let report = graph::validate_graph(&req.graph);
    let mut errors = report.errors;
    errors.extend(graph::validate_expressions(&req.graph));
    match subflow_errors(&state, &req.graph).await {
        Ok(more) => errors.extend(more),
        Err(resp) => return resp,
    }
    (
        StatusCode::OK,
        Json(ValidateGraphResponse {
            valid: errors.is_empty(),
            errors,
            warnings: report.warnings,
        }),
    )
        .into_response()
}

/// `POST /api/flows/{id}/convert` — project a legacy v1.1 flow to a v2
/// graph in place (`http-api.md` §3.3). `RequireAdmin`.
async fn convert_flow(
    _admin: RequireAdmin,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
) -> Response {
    let row = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    let version = match version_of(&row.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if version == 2 {
        return error(
            StatusCode::CONFLICT,
            "already_graph",
            "flow is already a v2 graph",
        );
    }
    if row.enabled {
        return error(
            StatusCode::CONFLICT,
            "definition_swap_locked",
            "disable the flow before converting; conversion changes the definition",
        );
    }
    let definition = match parse_definition(&row.flowData) {
        Some(def) => def,
        None => return internal(),
    };
    // `project_legacy` assigns the top-to-bottom node layout (§3.3).
    let flow_data = encode_flow_data(&project_legacy(&definition));
    let updated = match bot_flows::update(
        &state.app.db,
        id,
        BotFlowUpdate {
            flowData: Some(flow_data),
            ..BotFlowUpdate::default()
        },
    )
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    let last_run = match last_run_summary_view(&state, id, 2).await {
        Ok(lr) => lr,
        Err(resp) => return resp,
    };
    match to_flow_view(updated, 2, last_run) {
        Ok(flow) => (StatusCode::OK, Json(flow)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRunsQuery {
    limit: Option<usize>,
    cursor: Option<i64>,
}

/// `GET /api/flows/{id}/runs` — keyset-paginated run history. Summaries-only
/// payload: `nodeResults` is emitted empty to keep the history page light
/// (`http-api.md` §3.2).
async fn list_runs(
    _user: RequireAuth,
    State(state): State<FlowApiState>,
    Path(id): Path<i64>,
    Query(query): Query<ListRunsQuery>,
) -> Response {
    let flow = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    let version = match version_of(&flow.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

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
    let runs = rows
        .into_iter()
        .map(|r| to_run_view(r, version, false))
        .collect();

    (StatusCode::OK, Json(ListRunsView { runs, next_cursor })).into_response()
}

/// `GET /api/flows/{id}/runs/{runId}` — one run with the full `nodeResults`
/// array, the run-overlay source (`http-api.md` §3.2). `404` if the run is
/// unknown or not owned by `{id}`.
async fn get_run(
    _user: RequireAuth,
    State(state): State<FlowApiState>,
    Path((id, run_id)): Path<(i64, i64)>,
) -> Response {
    let flow = match bot_flows::find_by_id(&state.app.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    let version = match version_of(&flow.flowData) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let run = match bot_flow_runs::find_by_id(&state.app.db, run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(),
        Err(_) => return internal(),
    };
    // A run row that belongs to a different flow is, to this path, unknown.
    if run.flowId != id {
        return not_found();
    }
    (StatusCode::OK, Json(to_run_view(run, version, true))).into_response()
}
