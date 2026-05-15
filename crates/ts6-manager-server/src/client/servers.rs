//! Typed REST client for `/api/servers` — list, create, and patch.

use ts6_manager_shared::servers::{CreateServerRequest, PatchServerRequest, ServerSummary};

use crate::client::api::{ApiError, authorized_get_json, authorized_patch_json, authorized_post_json};
use crate::client::session::RefreshGate;

pub async fn list(gate: &RefreshGate, base: &str) -> Result<Vec<ServerSummary>, ApiError> {
    authorized_get_json(gate, base, "/api/servers").await
}

pub async fn create(
    gate: &RefreshGate,
    base: &str,
    req: &CreateServerRequest,
) -> Result<ServerSummary, ApiError> {
    authorized_post_json(gate, base, "/api/servers", Some(req)).await
}

pub async fn patch(
    gate: &RefreshGate,
    base: &str,
    id: i64,
    req: &PatchServerRequest,
) -> Result<ServerSummary, ApiError> {
    let path = format!("/api/servers/{id}");
    authorized_patch_json(gate, base, &path, req).await
}
