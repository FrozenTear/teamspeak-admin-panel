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
