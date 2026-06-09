//! Per-server access checks for the Phase 2 control surface — PURA-71.
//!
//! Two gates:
//!
//! - [`check_read`] — the caller (already authenticated by [`crate::auth::extractors::RequireAuth`])
//!   must be admin OR have a `server_user_grant` row for `configId`.
//! - [`check_write`] — admin OR (moderator AND `server_user_grant`). Viewer
//!   role can never mutate, even on servers they have a grant on.
//!
//! Both also resolve the `server_connection` row by id and return it so the
//! handler doesn't have to refetch. Missing row → `404`. DB error → `500`.

use axum::http::StatusCode;
use axum::response::Response;

use crate::app_state::AppState;
use crate::auth::extractors::AuthUser;
use crate::repos::{server_connections::ServerConnection, server_user_grants};

use super::{err, internal, not_found};

/// Read access. Returns the resolved [`ServerConnection`].
pub async fn check_read(
    state: &AppState,
    user: &AuthUser,
    config_id: i64,
) -> Result<ServerConnection, Response> {
    let connection = resolve_connection(state, config_id).await?;
    if user.is_admin() {
        return Ok(connection);
    }
    let granted = server_user_grants::exists(&state.db, user.id, config_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, user_id = user.id, config_id, "control: grant lookup failed");
            internal()
        })?;
    if !granted {
        // Spec §6.4.2 — missing-grant ⇒ 403, not 404. (Pure absence of
        // the row would also be 404 if we leaked existence; we don't —
        // the connection lookup ran ahead of the grant check.)
        return Err(err(StatusCode::FORBIDDEN, "Insufficient permissions"));
    }
    Ok(connection)
}

/// Write access. Returns the resolved [`ServerConnection`].
pub async fn check_write(
    state: &AppState,
    user: &AuthUser,
    config_id: i64,
) -> Result<ServerConnection, Response> {
    if !user.is_at_least_moderator() {
        // Even with a grant, viewers cannot mutate. Resolve the row
        // first so a viewer poking a non-existent server still gets
        // `404` rather than `403` — matches the §7.0.2 surface used by
        // every other route.
        let _connection = resolve_connection(state, config_id).await?;
        return Err(err(StatusCode::FORBIDDEN, "Insufficient permissions"));
    }
    check_read(state, user, config_id).await
}

/// Admin-only write access — used by the PURA-373 moderation surfaces
/// (server groups, channel groups, tokens, messages). Spec §7.9–7.16 mark
/// every mutating row "Y+admin"; unlike [`check_write`] a moderator with a
/// grant is **not** sufficient here. Reads on those surfaces still go
/// through [`check_read`] (any operator with server access).
///
/// Resolves the row first so a non-admin poking a non-existent server
/// still gets `404` rather than `403`, matching the rest of the §7.0.2
/// surface.
pub async fn check_admin(
    state: &AppState,
    user: &AuthUser,
    config_id: i64,
) -> Result<ServerConnection, Response> {
    let connection = resolve_connection(state, config_id).await?;
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, "Insufficient permissions"));
    }
    Ok(connection)
}

async fn resolve_connection(
    state: &AppState,
    config_id: i64,
) -> Result<ServerConnection, Response> {
    crate::repos::server_connections::find_by_id(&state.db, config_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, config_id, "control: server_connection lookup failed");
            internal()
        })?
        .ok_or_else(not_found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::extractors::AuthUser;
    use crate::auth::password;
    use crate::crypto;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{server_connections::NewServerConnection, users};
    use crate::webquery::WebQueryPool;
    use crate::ws::Hub;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-pura-71");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
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
            yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
        }
    }

    async fn seed_user(state: &AppState, name: &str, role: &str) -> AuthUser {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        let row = users::insert(
            &state.db,
            users::NewUser {
                username: name.into(),
                passwordHash: hash,
                displayName: name.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        AuthUser {
            id: row.id,
            username: row.username,
            display_name: row.displayName,
            role: row.role,
            enabled: row.enabled,
        }
    }

    async fn seed_server(state: &AppState, name: &str) -> i64 {
        crate::repos::server_connections::insert(
            &state.db,
            NewServerConnection {
                name: name.into(),
                host: "ts.example.com".into(),
                webqueryPort: 10080,
                apiKey: crypto::seal("k").unwrap(),
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
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn admin_reads_any_server() {
        let state = fresh_state().await;
        let admin = seed_user(&state, "a", "admin").await;
        let sid = seed_server(&state, "S").await;
        let row = check_read(&state, &admin, sid).await.unwrap();
        assert_eq!(row.id, sid);
    }

    #[tokio::test]
    async fn viewer_without_grant_is_forbidden() {
        let state = fresh_state().await;
        let viewer = seed_user(&state, "v", "viewer").await;
        let sid = seed_server(&state, "S").await;
        let resp = check_read(&state, &viewer, sid).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn viewer_with_grant_can_read_but_not_write() {
        let state = fresh_state().await;
        let viewer = seed_user(&state, "v", "viewer").await;
        let sid = seed_server(&state, "S").await;
        server_user_grants::insert(&state.db, viewer.id, sid)
            .await
            .unwrap();
        // Read OK.
        check_read(&state, &viewer, sid).await.unwrap();
        // Write rejected.
        let resp = check_write(&state, &viewer, sid).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn moderator_with_grant_can_write() {
        let state = fresh_state().await;
        let modr = seed_user(&state, "m", "moderator").await;
        let sid = seed_server(&state, "S").await;
        server_user_grants::insert(&state.db, modr.id, sid)
            .await
            .unwrap();
        check_write(&state, &modr, sid).await.unwrap();
    }

    #[tokio::test]
    async fn moderator_without_grant_is_forbidden() {
        let state = fresh_state().await;
        let modr = seed_user(&state, "m", "moderator").await;
        let sid = seed_server(&state, "S").await;
        let resp = check_write(&state, &modr, sid).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn check_admin_allows_admin_rejects_moderator() {
        // PURA-373 — moderation writes are admin-only; a moderator with a
        // grant (which `check_write` would accept) is rejected here.
        let state = fresh_state().await;
        let admin = seed_user(&state, "a", "admin").await;
        let modr = seed_user(&state, "m", "moderator").await;
        let sid = seed_server(&state, "S").await;
        server_user_grants::insert(&state.db, modr.id, sid)
            .await
            .unwrap();
        check_admin(&state, &admin, sid).await.unwrap();
        let resp = check_admin(&state, &modr, sid).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn check_admin_missing_server_yields_404_before_403() {
        let state = fresh_state().await;
        let modr = seed_user(&state, "m", "moderator").await;
        let resp = check_admin(&state, &modr, 9999).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_server_yields_404() {
        let state = fresh_state().await;
        let admin = seed_user(&state, "a", "admin").await;
        let resp = check_read(&state, &admin, 9999).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
