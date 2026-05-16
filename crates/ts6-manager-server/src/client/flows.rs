//! Typed REST client for `/api/flows` and friends (PURA-243).
//!
//! Wraps the flow-engine REST surface (PURA-242) so the Dioxus flow pages
//! don't sprinkle URL strings or `serde_json::Value` parsing through their
//! bodies. Every call goes through the shared [`crate::client::api`]
//! helpers so the single-flight refresh contract holds.
//!
//! Wire types come straight from [`ts6_manager_shared::flows`] — the FE
//! never redefines a JSON shape. The contract follows
//! `docs/flows/http-api.md`.

use std::sync::Arc;

use ts6_manager_shared::flows as wire;
use ts6_manager_shared::flows::v2;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

// ---- Flows --------------------------------------------------------------

/// `GET /api/flows[?virtualServerId=…]` — flat list (no pagination).
pub async fn list_flows(
    gate: Arc<RefreshGate>,
    virtual_server_id: Option<i64>,
) -> Result<Vec<wire::Flow>, ApiError> {
    let path = match virtual_server_id {
        Some(vs) => format!("/api/flows?virtualServerId={vs}"),
        None => "/api/flows".to_string(),
    };
    api::authorized_get_json::<wire::ListFlowsResponse>(&gate, &api::api_base(), &path)
        .await
        .map(|r| r.flows)
}

/// `GET /api/flows/{id}`.
pub async fn get_flow(gate: Arc<RefreshGate>, flow: wire::FlowId) -> Result<wire::Flow, ApiError> {
    let path = format!("/api/flows/{}", flow.0);
    api::authorized_get_json::<wire::Flow>(&gate, &api::api_base(), &path).await
}

/// `POST /api/flows` — admin-only. The `enabled` field on the request
/// body controls whether the engine registers the trigger immediately.
pub async fn create_flow(
    gate: Arc<RefreshGate>,
    body: &wire::CreateFlowRequest,
) -> Result<wire::Flow, ApiError> {
    api::authorized_post_json::<_, wire::Flow>(&gate, &api::api_base(), "/api/flows", Some(body))
        .await
}

/// `PATCH /api/flows/{id}` — admin-only. Returns the patched row.
pub async fn update_flow(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    body: &wire::UpdateFlowRequest,
) -> Result<wire::Flow, ApiError> {
    let path = format!("/api/flows/{}", flow.0);
    api::authorized_patch_json::<_, wire::Flow>(&gate, &api::api_base(), &path, body).await
}

/// `DELETE /api/flows/{id}[?force=true]` — admin-only.
pub async fn delete_flow(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    force: bool,
) -> Result<(), ApiError> {
    let path = if force {
        format!("/api/flows/{}?force=true", flow.0)
    } else {
        format!("/api/flows/{}", flow.0)
    };
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

// ---- Runs ---------------------------------------------------------------

/// `POST /api/flows/{id}/fire` — admin-only. Optional operator-provided
/// `context` document is merged into the resolved trigger event.
pub async fn fire_flow(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    context: Option<serde_json::Value>,
) -> Result<wire::FireFlowResponse, ApiError> {
    let path = format!("/api/flows/{}/fire", flow.0);
    let body = wire::FireFlowRequest { context };
    api::authorized_post_json::<_, wire::FireFlowResponse>(
        &gate,
        &api::api_base(),
        &path,
        Some(&body),
    )
    .await
}

/// `GET /api/flows/{id}/runs[?limit=&cursor=]`. Returns the page and the
/// keyset cursor for the next call (`None` when the list is exhausted).
pub async fn list_runs(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    limit: Option<u32>,
    cursor: Option<wire::FlowRunId>,
) -> Result<wire::ListRunsResponse, ApiError> {
    let mut path = format!("/api/flows/{}/runs", flow.0);
    let mut sep = '?';
    if let Some(l) = limit {
        path.push(sep);
        path.push_str(&format!("limit={l}"));
        sep = '&';
    }
    if let Some(c) = cursor {
        path.push(sep);
        path.push_str(&format!("cursor={}", c.0));
    }
    api::authorized_get_json::<wire::ListRunsResponse>(&gate, &api::api_base(), &path).await
}

// ---- v2 graph surface ---------------------------------------------------
//
// The v2 flow-engine REST contract (`docs/flows/v2/http-api.md`), backed by
// the routes from [PURA-278](/PURA/issues/PURA-278). The v1.1 helpers above
// stay for the legacy pages until the canvas swap (PURA-267) migrates them;
// these are what the canvas builder calls. Wire types come from
// [`ts6_manager_shared::flows::v2`] — the FE never redefines a JSON shape.

/// `GET /api/flows[?virtualServerId=…]` decoded as the v2 [`v2::FlowView`]
/// list — each row carries `flowVersion` and the stored `graph`/`definition`.
pub async fn list_flow_views(
    gate: Arc<RefreshGate>,
    virtual_server_id: Option<i64>,
) -> Result<Vec<v2::FlowView>, ApiError> {
    let path = match virtual_server_id {
        Some(vs) => format!("/api/flows?virtualServerId={vs}"),
        None => "/api/flows".to_string(),
    };
    api::authorized_get_json::<v2::ListFlowsView>(&gate, &api::api_base(), &path)
        .await
        .map(|r| r.flows)
}

/// `GET /api/flows/{id}` decoded as the v2 [`v2::FlowView`].
pub async fn get_flow_view(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
) -> Result<v2::FlowView, ApiError> {
    let path = format!("/api/flows/{}", flow.0);
    api::authorized_get_json::<v2::FlowView>(&gate, &api::api_base(), &path).await
}

/// `POST /api/flows` with a v2 graph (or legacy definition) body —
/// admin-only. Returns the stored flow as a [`v2::FlowView`].
pub async fn create_graph_flow(
    gate: Arc<RefreshGate>,
    body: &v2::CreateFlowBody,
) -> Result<v2::FlowView, ApiError> {
    api::authorized_post_json::<_, v2::FlowView>(&gate, &api::api_base(), "/api/flows", Some(body))
        .await
}

/// `PATCH /api/flows/{id}` with a v2 graph swap (or other field) body —
/// admin-only. A graph swap is rejected `409 definition_swap_locked` while
/// the flow is enabled (`http-api.md` §4).
pub async fn update_graph_flow(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    body: &v2::UpdateFlowBody,
) -> Result<v2::FlowView, ApiError> {
    let path = format!("/api/flows/{}", flow.0);
    api::authorized_patch_json::<_, v2::FlowView>(&gate, &api::api_base(), &path, body).await
}

/// `POST /api/flows/validate` — admin-only. Validates a graph **without
/// persisting**; the canvas calls this on a short debounce after every
/// structural edit (`ui-brief.md` §4.4). A structurally-broken graph still
/// returns `200 OK` with `valid: false` — only a transport/auth failure is
/// an [`ApiError`].
pub async fn validate_graph(
    gate: Arc<RefreshGate>,
    graph: &v2::FlowGraph,
) -> Result<v2::ValidateGraphResponse, ApiError> {
    let body = v2::ValidateGraphRequest {
        graph: graph.clone(),
    };
    api::authorized_post_json::<_, v2::ValidateGraphResponse>(
        &gate,
        &api::api_base(),
        "/api/flows/validate",
        Some(&body),
    )
    .await
}

/// `POST /api/flows/{id}/convert` — admin-only. Converts a legacy v1.1 flow
/// to a v2 graph in place. `409 already_graph` if already v2;
/// `409 definition_swap_locked` if the flow is enabled (`http-api.md` §3.3).
pub async fn convert_flow(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
) -> Result<v2::FlowView, ApiError> {
    let path = format!("/api/flows/{}/convert", flow.0);
    api::authorized_post_json::<(), v2::FlowView>(&gate, &api::api_base(), &path, None).await
}

/// `GET /api/flows/{id}/runs/{runId}` — one run with the full `nodeResults`
/// array, the canvas run-overlay's data source (`http-api.md` §3.2).
pub async fn get_run_detail(
    gate: Arc<RefreshGate>,
    flow: wire::FlowId,
    run: wire::FlowRunId,
) -> Result<v2::FlowRunView, ApiError> {
    let path = format!("/api/flows/{}/runs/{}", flow.0, run.0);
    api::authorized_get_json::<v2::FlowRunView>(&gate, &api::api_base(), &path).await
}
