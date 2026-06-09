//! Spec §7.4 + `docs/admin/http-api.md` — v1.1 admin user management.
//!
//! Routes (all admin-only via [`RequireAdmin`]):
//!
//! | Method | Path | |
//! |---|---|---|
//! | `GET` | `/api/users` | List + role/enabled/q filters. |
//! | `POST` | `/api/users` | Create (validates password complexity). |
//! | `GET` | `/api/users/{id}` | Detail. |
//! | `PATCH` | `/api/users/{id}` | Edit (role/enabled/displayName/password). |
//! | `DELETE` | `/api/users/{id}` | Hard-delete (user_set_null_admin_audit nulls historic rows). |
//! | `GET` | `/api/users/{id}/sessions` | List refresh-token rows for target. |
//! | `DELETE` | `/api/users/{id}/sessions/{sid}` | Revoke one family-wide. |
//!
//! Authorization:
//! - The [`RequireAdmin`] extractor enforces the §6.4 + §6.6 role gate.
//!   No bare `is_admin()` checks live in this module.
//!
//! Self-action + last-enabled-admin protections per
//! `docs/admin/architecture.md` §5.3:
//! - Self-disable, self-role-demote-away-from-admin, self-delete return 400.
//! - Any mutation that would leave zero enabled admins returns 400.
//!
//! Session revocation per architecture.md §5.4:
//! - `enabled: false`, `password` set, and `DELETE` revoke every session for
//!   the target. `role` changes do NOT — DB-current role per spec §6.4.1
//!   takes effect on next request.
//!
//! Audit emission per `docs/admin/audit-shape.md` §2.1. One row per logical
//! change; the catch-all `userPatched` row is emitted first, followed by
//! sub-event rows for role/enabled/password transitions.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use ts6_manager_shared::admin::{AdminSession, AdminUser, AdminUserCreate, AdminUserPatch};
use ts6_manager_shared::auth::ErrorResponse;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequireAdmin};
use crate::auth::permissions;
use crate::auth::{complexity, password};
use crate::repos::{refresh_tokens, user_permissions, users};

const VALID_ROLES: &[&str] = &["admin", "moderator", "viewer"];
const USERNAME_MIN: usize = 1;
const USERNAME_MAX: usize = 64;
const DISPLAY_NAME_MIN: usize = 1;
const DISPLAY_NAME_MAX: usize = 128;

/// Build the user-management sub-router. Absolute paths — `merge` at the
/// top-level alongside the rest of the API surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/users", axum::routing::get(list).post(create))
        .route(
            "/api/users/{id}",
            axum::routing::get(detail)
                .patch(patch_user)
                .delete(delete_user),
        )
        .route(
            "/api/users/{id}/sessions",
            axum::routing::get(list_sessions),
        )
        .route(
            "/api/users/{id}/sessions/{sid}",
            axum::routing::delete(revoke_session),
        )
        .route(
            "/api/users/{id}/permissions",
            axum::routing::get(list_permissions).put(replace_permissions),
        )
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UsersQuery {
    role: Option<String>,
    enabled: Option<bool>,
    q: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Query(filter): Query<UsersQuery>,
) -> Result<Json<Vec<AdminUser>>, Response> {
    let rows = users::list(&state.db).await.map_err(|e| {
        tracing::error!(err = %e, "list_users: db query failed");
        internal()
    })?;

    let role_filter = filter.role.as_deref();
    let q_lower = filter.q.as_deref().map(|s| s.to_lowercase());

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(r) = role_filter
            && row.role != r
        {
            continue;
        }
        if let Some(want) = filter.enabled
            && row.enabled != want
        {
            continue;
        }
        if let Some(ref needle) = q_lower
            && !row.username.to_lowercase().contains(needle)
            && !row.displayName.to_lowercase().contains(needle)
        {
            continue;
        }
        let active = refresh_tokens::count_active_for_user(&state.db, row.id)
            .await
            .unwrap_or(0);
        out.push(to_wire(&row, active));
    }
    Ok(Json(out))
}

async fn create(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    request_meta: RequestMeta,
    Json(req): Json<AdminUserCreate>,
) -> Result<(StatusCode, Json<AdminUser>), Response> {
    // 1. Username validation (lowercased; ASCII-only `[a-z0-9._-]+`).
    let username = validate_username(&req.username)?;

    // 2. Display-name length check.
    let display_name = req.display_name.trim().to_string();
    if !(DISPLAY_NAME_MIN..=DISPLAY_NAME_MAX).contains(&display_name.chars().count()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "displayName must be 1..=128 characters",
        ));
    }

    // 3. Role validation (default `viewer`).
    let role = match req.role.as_deref() {
        Some(r) if VALID_ROLES.contains(&r) => r.to_string(),
        Some(_) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "role must be 'admin', 'moderator', or 'viewer'",
            ));
        }
        None => "viewer".to_string(),
    };

    // 4. Password complexity (spec §6.2.2 verbatim strings).
    if let Err(rule) = complexity::validate(&req.password) {
        return Err(err(StatusCode::BAD_REQUEST, rule.message()));
    }

    // 5. Duplicate-username check (409 per http-api.md §3.1).
    if users::find_by_username(&state.db, &username)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "create_user: dupe-check query failed");
            internal()
        })?
        .is_some()
    {
        return Err(err(StatusCode::CONFLICT, "Username already exists"));
    }

    // 6. Hash + insert.
    let pw = req.password.clone();
    let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "create_user: hash join failed");
            internal()
        })?
        .map_err(|e| {
            tracing::error!(err = %e, "create_user: hash failed");
            internal()
        })?;

    let new = users::NewUser {
        username,
        passwordHash: hash,
        displayName: display_name,
        role: role.clone(),
        enabled: true,
    };
    let row = users::insert(&state.db, new).await.map_err(|e| {
        tracing::error!(err = %e, "create_user: insert failed");
        internal()
    })?;

    // 7. Audit `userCreated` per audit-shape.md §2.2.
    audit::record(
        &state.db,
        Event {
            actor: admin,
            kind: AuditKind::UserCreated,
            target: Some(Target::user(row.id, row.username.clone())),
            payload: Some(serde_json::json!({
                "role": role,
                "enabled": true,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: request_meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(to_wire(&row, 0))))
}

async fn detail(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Path(id): Path<i64>,
) -> Result<Json<AdminUser>, Response> {
    let row = users::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "detail_user: query failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "User not found"))?;
    let active = refresh_tokens::count_active_for_user(&state.db, row.id)
        .await
        .unwrap_or(0);
    Ok(Json(to_wire(&row, active)))
}

async fn patch_user(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    request_meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<AdminUserPatch>,
) -> Result<Json<AdminUser>, Response> {
    // 1. Lookup target.
    let target = users::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "patch_user: lookup failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "User not found"))?;

    // 2. Reject empty patches.
    if req.display_name.is_none()
        && req.role.is_none()
        && req.enabled.is_none()
        && req.password.is_none()
    {
        return Err(err(StatusCode::BAD_REQUEST, "No mutable fields supplied"));
    }

    // 3. Validate field shapes BEFORE applying anything.
    if let Some(ref name) = req.display_name {
        let trimmed = name.trim();
        if !(DISPLAY_NAME_MIN..=DISPLAY_NAME_MAX).contains(&trimmed.chars().count()) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "displayName must be 1..=128 characters",
            ));
        }
    }
    if let Some(ref r) = req.role
        && !VALID_ROLES.contains(&r.as_str())
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "role must be 'admin', 'moderator', or 'viewer'",
        ));
    }
    if let Some(ref pw) = req.password
        && let Err(rule) = complexity::validate(pw)
    {
        return Err(err(StatusCode::BAD_REQUEST, rule.message()));
    }

    // 4. Last-enabled-admin protection (http-api.md §3.2 + example 4.3).
    //
    // Checked BEFORE the self-action rules: example 4.3 demotes the sole
    // bootstrap admin via a self-PATCH and expects the last-admin message,
    // not the self-demote message. When another enabled admin exists the
    // check passes and control falls through to the self-action rules,
    // which then catch a self-disable / self-demote with their own message.
    let demoting_target_from_admin =
        target.role == "admin" && req.role.as_deref().map(|r| r != "admin").unwrap_or(false);
    let disabling_admin_target =
        target.role == "admin" && target.enabled && matches!(req.enabled, Some(false));
    if (demoting_target_from_admin || disabling_admin_target)
        && users::count_enabled_admins_excluding(&state.db, target.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, id, "patch_user: admin-count query failed");
                internal()
            })?
            == 0
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Cannot remove the last enabled admin",
        ));
    }

    // 5. Self-action protections (architecture.md §5.3 table).
    if admin.id == target.id {
        if matches!(req.enabled, Some(false)) {
            return Err(err(StatusCode::BAD_REQUEST, "Cannot disable yourself"));
        }
        if let Some(ref new_role) = req.role
            && target.role == "admin"
            && new_role != "admin"
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Cannot demote yourself from admin",
            ));
        }
    }

    // 6. Apply the non-password merge (displayName / role / enabled).
    let old_role = target.role.clone();
    let old_enabled = target.enabled;
    let patch = users::UserUpdate {
        displayName: req.display_name.clone(),
        role: req.role.clone(),
        enabled: req.enabled,
    };
    let updated = users::update(&state.db, target.id, patch)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "patch_user: update failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "User not found"))?;

    // 7. Password reset path.
    if let Some(ref new_pw) = req.password {
        let pw = new_pw.clone();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .map_err(|e| {
                tracing::error!(err = %e, "patch_user: hash join failed");
                internal()
            })?
            .map_err(|e| {
                tracing::error!(err = %e, "patch_user: hash failed");
                internal()
            })?;
        users::set_password_hash(&state.db, target.id, hash)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, id, "patch_user: set_password_hash failed");
                internal()
            })?;
    }

    // 8. Session revocation per architecture.md §5.4.
    let sessions_revoked = if matches!(req.enabled, Some(false)) || req.password.is_some() {
        let pre = refresh_tokens::list_for_user(&state.db, target.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, id, "patch_user: pre-revoke list failed");
                internal()
            })?;
        let n = pre.len() as i64;
        let _ = refresh_tokens::delete_all_for_user(&state.db, target.id).await;
        n
    } else {
        0
    };

    // 9. Audit events — catch-all first, then sub-events per audit-shape.md §2.2.
    let mut fields = Vec::new();
    if req.display_name.is_some() {
        fields.push("displayName");
    }
    if req.role.is_some() {
        fields.push("role");
    }
    if req.enabled.is_some() {
        fields.push("enabled");
    }
    if req.password.is_some() {
        fields.push("password");
    }
    audit::record(
        &state.db,
        Event {
            actor: admin.clone(),
            kind: AuditKind::UserPatched,
            target: Some(Target::user(updated.id, updated.username.clone())),
            payload: Some(serde_json::json!({ "fields": fields })),
            outcome: Outcome::Success,
            error_msg: None,
            request: request_meta.clone(),
        },
    )
    .await;

    if let Some(new_enabled) = req.enabled
        && new_enabled != old_enabled
    {
        let kind = if new_enabled {
            AuditKind::UserEnabled
        } else {
            AuditKind::UserDisabled
        };
        audit::record(
            &state.db,
            Event {
                actor: admin.clone(),
                kind,
                target: Some(Target::user(updated.id, updated.username.clone())),
                payload: None,
                outcome: Outcome::Success,
                error_msg: None,
                request: request_meta.clone(),
            },
        )
        .await;
    }

    if let Some(ref new_role) = req.role
        && new_role != &old_role
    {
        audit::record(
            &state.db,
            Event {
                actor: admin.clone(),
                kind: AuditKind::UserRoleChanged,
                target: Some(Target::user(updated.id, updated.username.clone())),
                payload: Some(serde_json::json!({ "from": old_role, "to": new_role })),
                outcome: Outcome::Success,
                error_msg: None,
                request: request_meta.clone(),
            },
        )
        .await;
    }

    if req.password.is_some() {
        audit::record(
            &state.db,
            Event {
                actor: admin,
                kind: AuditKind::UserPasswordReset,
                target: Some(Target::user(updated.id, updated.username.clone())),
                payload: Some(serde_json::json!({ "sessionsRevoked": sessions_revoked })),
                outcome: Outcome::Success,
                error_msg: None,
                request: request_meta,
            },
        )
        .await;
    }

    let active = refresh_tokens::count_active_for_user(&state.db, updated.id)
        .await
        .unwrap_or(0);
    Ok(Json(to_wire(&updated, active)))
}

async fn delete_user(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    request_meta: RequestMeta,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    let target = users::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "delete_user: lookup failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "User not found"))?;

    // Self-delete refused (spec §7.4 verbatim).
    if admin.id == target.id {
        return Err(err(StatusCode::BAD_REQUEST, "Cannot delete yourself"));
    }

    // Last-enabled-admin protection.
    if target.role == "admin"
        && target.enabled
        && users::count_enabled_admins_excluding(&state.db, target.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, id, "delete_user: admin-count failed");
                internal()
            })?
            == 0
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Cannot delete the last enabled admin",
        ));
    }

    let sessions = refresh_tokens::list_for_user(&state.db, target.id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "delete_user: pre-revoke list failed");
            internal()
        })?;
    let sessions_revoked = sessions.len() as i64;

    // Emit `userDeleted` BEFORE the user row goes away (http-api.md §3.2 step 5):
    // the set-null cascade fires after the user row deletes and clears
    // `targetId` on the durable row. Emitting first preserves `targetId` for the
    // event detail; the cascade then clears it for forensic durability.
    audit::record(
        &state.db,
        Event {
            actor: admin,
            kind: AuditKind::UserDeleted,
            target: Some(Target::user(target.id, target.username.clone())),
            payload: Some(serde_json::json!({
                "role": target.role,
                "enabled": target.enabled,
                "sessionsRevoked": sessions_revoked,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: request_meta,
        },
    )
    .await;

    users::delete(&state.db, target.id).await.map_err(|e| {
        tracing::error!(err = %e, id, "delete_user: repo delete failed");
        internal()
    })?;

    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Path(id): Path<i64>,
) -> Result<Json<Vec<AdminSession>>, Response> {
    // 404 when the user doesn't exist (don't leak via empty list).
    if users::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "list_sessions: user lookup failed");
            internal()
        })?
        .is_none()
    {
        return Err(err(StatusCode::NOT_FOUND, "User not found"));
    }

    let mut rows = refresh_tokens::list_for_user(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, id, "list_sessions: query failed");
            internal()
        })?;
    // http-api.md §3.3: "Order by `createdAt DESC`."
    rows.sort_by_key(|r| std::cmp::Reverse(r.createdAt));
    Ok(Json(rows.into_iter().map(session_to_wire).collect()))
}

async fn revoke_session(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    request_meta: RequestMeta,
    Path((uid, sid)): Path<(i64, i64)>,
) -> Result<StatusCode, Response> {
    // Resolve `sid` first; 404 if absent OR not owned by `uid` (don't leak
    // cross-user existence per http-api.md §3.3 step 2).
    let row = refresh_tokens::find_by_id(&state.db, sid)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, sid, "revoke_session: lookup failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Session not found"))?;
    if row.userId != uid {
        return Err(err(StatusCode::NOT_FOUND, "Session not found"));
    }

    // Family-wide revoke per http-api.md §3.3 step 3.
    let family = match row.family.as_deref() {
        Some(f) => f.to_string(),
        None => {
            // Family is None for any row that predates §6.5; delete the single
            // row by token in that case.
            let token = row.token.clone();
            let _ = refresh_tokens::delete_by_token(&state.db, &token).await;
            audit::record(
                &state.db,
                Event {
                    actor: admin,
                    kind: AuditKind::SessionRevoked,
                    target: Some(Target::session(sid)),
                    payload: Some(serde_json::json!({
                        "family": null,
                        "rowsDeleted": 1,
                    })),
                    outcome: Outcome::Success,
                    error_msg: None,
                    request: request_meta,
                },
            )
            .await;
            return Ok(StatusCode::NO_CONTENT);
        }
    };
    let deleted = refresh_tokens::delete_by_family(&state.db, &family)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, family = %family, "revoke_session: family delete failed");
            internal()
        })?;

    audit::record(
        &state.db,
        Event {
            actor: admin,
            kind: AuditKind::SessionRevoked,
            target: Some(Target::session(sid)),
            payload: Some(serde_json::json!({
                "family": family,
                "rowsDeleted": deleted,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: request_meta,
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// The error path returns an already-built `Response` (large), the ok path a
// `String` (small). The helper is only called from `create`, which already
// returns the same large-`Response` error type — boxing here would just move
// the allocation. Matches the `video_sources` precedent.
#[allow(clippy::result_large_err)]
fn validate_username(raw: &str) -> Result<String, Response> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(USERNAME_MIN..=USERNAME_MAX).contains(&lower.chars().count()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "username must be 1..=64 characters",
        ));
    }
    if !lower
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "username may only contain a-z, 0-9, '.', '_', '-'",
        ));
    }
    Ok(lower)
}

fn to_wire(row: &users::User, active_session_count: i64) -> AdminUser {
    AdminUser {
        id: row.id,
        username: row.username.clone(),
        display_name: row.displayName.clone(),
        role: row.role.clone(),
        enabled: row.enabled,
        created_at: row.createdAt,
        updated_at: row.updatedAt,
        last_login_at: row.lastLoginAt,
        active_session_count,
    }
}

fn session_to_wire(row: refresh_tokens::RefreshToken) -> AdminSession {
    AdminSession {
        id: row.id,
        family: row.family,
        created_at: row.createdAt,
        expires_at: row.expiresAt,
        replaced_by: row.replacedBy,
    }
}

fn err(status: StatusCode, body: &str) -> Response {
    (status, Json(ErrorResponse::new(body))).into_response()
}

fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
}

// ── Permission grant surface (PURA-284 / Phase 9.0-rbac) ─────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UserPermissionsResponse {
    user_id: i64,
    effective: Vec<String>,
    explicit_grants: Vec<String>,
}

#[derive(serde::Deserialize)]
struct UserPermissionsUpdate {
    permissions: Vec<String>,
}

/// `GET /api/users/{id}/permissions` — admin-only.
///
/// Returns the resolved effective permission set (role defaults ∪ explicit
/// grants, intersected with the catalog) and the raw explicit-grant list.
async fn list_permissions(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Path(id): Path<i64>,
) -> Result<Json<UserPermissionsResponse>, Response> {
    let user = users::find_by_id(&state.db, id)
        .await
        .map_err(|_| internal())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "user not found"))?;

    let grants = user_permissions::permissions_for_user(&state.db, id)
        .await
        .map_err(|_| internal())?;

    let effective = permissions::effective_permissions(&user.role, &grants)
        .into_iter()
        .collect();

    Ok(Json(UserPermissionsResponse {
        user_id: id,
        effective,
        explicit_grants: grants,
    }))
}

/// `PUT /api/users/{id}/permissions` — admin-only, replace-all semantics.
///
/// The body `{"permissions": [...]}` is validated against the catalog; any
/// unknown string is rejected 422. On success the grant set is atomically
/// replaced and a `userPermissionsChanged` audit row is emitted.
async fn replace_permissions(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    request_meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<UserPermissionsUpdate>,
) -> Result<Json<UserPermissionsResponse>, Response> {
    let user = users::find_by_id(&state.db, id)
        .await
        .map_err(|_| internal())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "user not found"))?;

    for p in &req.permissions {
        if !permissions::is_known_permission(p) {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("unknown permission: {p}"),
            ));
        }
    }

    user_permissions::replace_all(&state.db, id, admin.id, &req.permissions)
        .await
        .map_err(|_| internal())?;

    let grants = user_permissions::permissions_for_user(&state.db, id)
        .await
        .map_err(|_| internal())?;
    let effective: Vec<String> = permissions::effective_permissions(&user.role, &grants)
        .into_iter()
        .collect();

    audit::record(
        &state.db,
        Event {
            actor: admin.clone(),
            kind: AuditKind::UserPermissionsChanged,
            target: Some(Target::user(id, user.username)),
            payload: Some(serde_json::json!({ "permissions": &req.permissions })),
            outcome: Outcome::Success,
            error_msg: None,
            request: request_meta.clone(),
        },
    )
    .await;

    Ok(Json(UserPermissionsResponse {
        user_id: id,
        effective,
        explicit_grants: grants,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::jwt;
    use crate::db::{connect_in_memory, migrations};
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crate::crypto::init("test-seed-pura-235");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: crate::webquery::WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: None,
            yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
        }
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    async fn seed_user(state: &AppState, username: &str, role: &str) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
            &state.db,
            users::NewUser {
                username: username.into(),
                passwordHash: hash,
                displayName: username.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn mint_token(state: &AppState, id: i64, username: &str, role: &str) -> String {
        jwt::mint_access(
            id,
            username,
            role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    fn auth(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    fn body<T: serde::Serialize>(v: &T) -> Body {
        Body::from(serde_json::to_vec(v).unwrap())
    }

    async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    #[tokio::test]
    async fn list_users_requires_admin() {
        let state = fresh_state().await;
        let vid = seed_user(&state, "view", "viewer").await;
        let token = mint_token(&state, vid, "view", "viewer");
        let app = app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_then_list_round_trip() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let app = app(state.clone());

        let create = AdminUserCreate {
            username: "Moderator1".into(), // mixed case — handler lowercases
            password: "SecurePass123!".into(),
            display_name: "Moderator One".into(),
            role: Some("moderator".into()),
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/users")
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&create))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: AdminUser = read_json(resp).await;
        assert_eq!(created.username, "moderator1");
        assert_eq!(created.role, "moderator");
        assert!(created.enabled);
        assert_eq!(created.active_session_count, 0);

        // Pin the wire-level invariant: passwordHash / password never on the wire.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!raw.contains("passwordHash"), "passwordHash leaked: {raw}");
        let list: Vec<AdminUser> = serde_json::from_str(&raw).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn create_dupe_username_returns_409() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "admin").await;
        let token = mint_token(
            &state,
            seed_user(&state, "admin", "admin").await,
            "admin",
            "admin",
        );
        let app = app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/users")
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserCreate {
                        username: "alice".into(),
                        password: "SecurePass123!".into(),
                        display_name: "Alice".into(),
                        role: None,
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn create_rejects_weak_password_with_spec_message() {
        let state = fresh_state().await;
        let token = mint_token(
            &state,
            seed_user(&state, "admin", "admin").await,
            "admin",
            "admin",
        );
        let app = app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/users")
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserCreate {
                        username: "weakpw".into(),
                        password: "short".into(),
                        display_name: "X".into(),
                        role: None,
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let err: ErrorResponse = read_json(resp).await;
        assert!(err.error.starts_with("Password must"));
    }

    #[tokio::test]
    async fn patch_empty_body_returns_400() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "alice", "viewer").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let app = app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{target}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch::default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_disable_revokes_sessions_and_emits_two_audit_rows() {
        use crate::repos::admin_audit_log;
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "alice", "moderator").await;
        // Seed a refresh token for `target` so the revoke has something to delete.
        refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "tok-1".into(),
                userId: target,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("fam-1".into()),
            },
        )
        .await
        .unwrap();

        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{target}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch {
                        enabled: Some(false),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let updated: AdminUser = read_json(resp).await;
        assert!(!updated.enabled);
        assert_eq!(updated.active_session_count, 0);

        // Refresh token for `target` must be gone (architecture.md §5.4).
        let rows = refresh_tokens::list_for_user(&state.db, target)
            .await
            .unwrap();
        assert!(rows.is_empty(), "session must be revoked on disable");

        // audit-shape.md §2.2 — userPatched + userDisabled rows.
        let (rows, _) =
            admin_audit_log::list(&state.db, &admin_audit_log::ListFilter::default(), 50, 0)
                .await
                .unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&"userPatched"));
        assert!(kinds.contains(&"userDisabled"));
    }

    #[tokio::test]
    async fn patch_role_change_emits_role_changed_event_and_keeps_sessions() {
        use crate::repos::admin_audit_log;
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "alice", "viewer").await;
        // Sessions exist for target; role change must NOT revoke them.
        refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "tok-keep".into(),
                userId: target,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("fam-keep".into()),
            },
        )
        .await
        .unwrap();

        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{target}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch {
                        role: Some("moderator".into()),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let leftover = refresh_tokens::list_for_user(&state.db, target)
            .await
            .unwrap();
        assert_eq!(
            leftover.len(),
            1,
            "role change must NOT revoke target sessions per arch §5.4"
        );
        let (rows, _) = admin_audit_log::list(
            &state.db,
            &admin_audit_log::ListFilter {
                kind: Some("userRoleChanged".into()),
                ..Default::default()
            },
            50,
            0,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn patch_self_disable_returns_400() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{aid}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch {
                        enabled: Some(false),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// http-api.md example 4.3 — the sole bootstrap admin self-demoting to
    /// `viewer` is refused with the last-admin message (the last-admin check
    /// runs before the self-demote check so this exact scenario surfaces the
    /// "Cannot remove the last enabled admin" string).
    #[tokio::test]
    async fn patch_last_enabled_admin_self_demote_returns_400() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{aid}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch {
                        role: Some("viewer".into()),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let e: ErrorResponse = read_json(resp).await;
        assert_eq!(e.error, "Cannot remove the last enabled admin");
    }

    /// With a second enabled admin present, demoting one is permitted — the
    /// last-admin protection only fires when the count would hit zero.
    #[tokio::test]
    async fn patch_demote_admin_allowed_when_another_admin_exists() {
        let state = fresh_state().await;
        let actor = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "second", "admin").await;
        let token = mint_token(&state, actor, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/users/{target}"))
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(body(&AdminUserPatch {
                        role: Some("viewer".into()),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let updated: AdminUser = read_json(resp).await;
        assert_eq!(updated.role, "viewer");
    }

    #[tokio::test]
    async fn delete_self_returns_400() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/users/{aid}"))
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `count_enabled_admins_excluding` backs both the PATCH and DELETE
    /// last-admin guards. Pin its semantics: a disabled admin is not counted,
    /// and the excluded id is dropped from the tally.
    #[tokio::test]
    async fn count_enabled_admins_excluding_ignores_disabled_and_self() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let other = seed_user(&state, "other", "admin").await;
        // Two enabled admins; excluding one leaves one.
        assert_eq!(
            users::count_enabled_admins_excluding(&state.db, aid)
                .await
                .unwrap(),
            1
        );
        // Disable `other` — now excluding `aid` leaves zero enabled admins.
        users::update(
            &state.db,
            other,
            users::UserUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            users::count_enabled_admins_excluding(&state.db, aid)
                .await
                .unwrap(),
            0,
            "disabled admin must not count toward the enabled-admin tally"
        );
    }

    #[tokio::test]
    async fn list_sessions_returns_404_for_missing_user() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/users/9999/sessions")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn revoke_session_kills_entire_family() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "alice", "viewer").await;

        // Two rows in the same family; one out-of-family.
        let row1 = refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "fam1-a".into(),
                userId: target,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("famA".into()),
            },
        )
        .await
        .unwrap();
        refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "fam1-b".into(),
                userId: target,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("famA".into()),
            },
        )
        .await
        .unwrap();
        refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "famB-only".into(),
                userId: target,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("famB".into()),
            },
        )
        .await
        .unwrap();

        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/users/{target}/sessions/{}", row1.id))
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let remaining = refresh_tokens::list_for_user(&state.db, target)
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "famA rows must be gone, famB row remains"
        );
        assert_eq!(remaining[0].family.as_deref(), Some("famB"));
    }

    #[tokio::test]
    async fn revoke_session_cross_user_returns_404_without_leak() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let owner = seed_user(&state, "alice", "viewer").await;
        let other = seed_user(&state, "bob", "viewer").await;

        let row = refresh_tokens::insert(
            &state.db,
            refresh_tokens::NewRefreshToken {
                token: "tk".into(),
                userId: owner,
                expiresAt: chrono::Utc::now() + chrono::Duration::hours(1),
                family: Some("fam".into()),
            },
        )
        .await
        .unwrap();
        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/users/{other}/sessions/{}", row.id))
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Row owned by `owner` must still be present.
        let still = refresh_tokens::list_for_user(&state.db, owner)
            .await
            .unwrap();
        assert_eq!(still.len(), 1);
    }

    #[tokio::test]
    async fn delete_user_emits_audit_before_cascade_clears_target() {
        use crate::repos::admin_audit_log;
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let target = seed_user(&state, "alice", "viewer").await;

        let token = mint_token(&state, aid, "admin", "admin");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/users/{target}"))
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // user_set_null_admin_audit (migration 0009) MUST have nulled
        // targetId on the userDeleted row.
        let (rows, _) = admin_audit_log::list(
            &state.db,
            &admin_audit_log::ListFilter {
                kind: Some("userDeleted".into()),
                ..Default::default()
            },
            50,
            0,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].targetId.is_none(),
            "set-null event must clear targetId after user delete (audit-shape.md §6)"
        );
        assert_eq!(rows[0].targetLabel.as_deref(), Some("alice"));
    }
}
