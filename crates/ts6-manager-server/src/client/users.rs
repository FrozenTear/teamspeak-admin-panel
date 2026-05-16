//! Typed REST client for the v1.1 admin-user surface — `/api/users` CRUD
//! plus `/api/users/{id}/sessions`.
//!
//! Mirrors [`crate::client::servers`] modulo the wire types: every call
//! funnels through the shared [`RefreshGate`] so the single-flight
//! refresh-on-401 contract holds for the admin pages exactly as it does for
//! the rest of the SPA. Route shapes are pinned by `docs/admin/http-api.md`
//! §3; the handlers live in `ts6-manager-server::routes::users`.

use ts6_manager_shared::admin::{AdminSession, AdminUser, AdminUserCreate, AdminUserPatch};

use crate::client::api::{
    ApiError, authorized_delete, authorized_get_json, authorized_patch_json, authorized_post_json,
};
use crate::client::session::RefreshGate;

/// `GET /api/users` — full list, ordered `id ASC` (http-api.md §3.1).
pub async fn list(gate: &RefreshGate, base: &str) -> Result<Vec<AdminUser>, ApiError> {
    authorized_get_json(gate, base, "/api/users").await
}

/// `POST /api/users` — create a user, returns `201 AdminUser`.
pub async fn create(
    gate: &RefreshGate,
    base: &str,
    req: &AdminUserCreate,
) -> Result<AdminUser, ApiError> {
    authorized_post_json(gate, base, "/api/users", Some(req)).await
}

/// `PATCH /api/users/{id}` — partial update, returns the refreshed `AdminUser`.
pub async fn patch(
    gate: &RefreshGate,
    base: &str,
    id: i64,
    req: &AdminUserPatch,
) -> Result<AdminUser, ApiError> {
    let path = format!("/api/users/{id}");
    authorized_patch_json(gate, base, &path, req).await
}

/// `DELETE /api/users/{id}` — permanent removal, `204 No Content` on success.
pub async fn delete(gate: &RefreshGate, base: &str, id: i64) -> Result<(), ApiError> {
    let path = format!("/api/users/{id}");
    authorized_delete(gate, base, &path).await
}

/// `GET /api/users/{id}/sessions` — every refresh-token row for the user,
/// ordered `createdAt DESC` (http-api.md §3.3).
pub async fn list_sessions(
    gate: &RefreshGate,
    base: &str,
    id: i64,
) -> Result<Vec<AdminSession>, ApiError> {
    let path = format!("/api/users/{id}/sessions");
    authorized_get_json(gate, base, &path).await
}

/// `DELETE /api/users/{id}/sessions/{sid}` — revokes the whole session
/// family that `{sid}` belongs to (http-api.md §3.3 step 3).
pub async fn revoke_session(
    gate: &RefreshGate,
    base: &str,
    id: i64,
    sid: i64,
) -> Result<(), ApiError> {
    let path = format!("/api/users/{id}/sessions/{sid}");
    authorized_delete(gate, base, &path).await
}
