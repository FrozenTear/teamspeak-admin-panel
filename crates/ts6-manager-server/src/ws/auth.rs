//! WS handshake authentication — PURA-70.
//!
//! The WS hub accepts two credential types on the handshake URL
//! (`/ws?token=…`):
//!
//! 1. **Access JWT** (operator-facing topics). Same path the Phase 1
//!    placeholder used; delegates to
//!    [`crate::auth::ws_handshake::authenticate_token`] so token shape /
//!    DB-role-wins / disabled-user behaviour stays in one place.
//! 2. **Widget token** (`server:{id}:widget` topic only). A URL-safe
//!    random string stored on `widget.token`; resolves to a single
//!    `(serverConfigId, virtualServerId)` pair the principal can
//!    subscribe to. Widget tokens never grant access to operator topics.
//!
//! The lookup tries the JWT path first (the common case for operators)
//! and falls back to the widget path on JWT failure. A token that
//! validates as neither closes the upgrade with `401`.

use crate::app_state::AppState;
use crate::auth::extractors::AuthUser;
use crate::auth::ws_handshake::{WsAuthError, authenticate_token};
use crate::repos::widgets;

/// Connection-level credential. Lives for the lifetime of the WebSocket.
#[derive(Debug, Clone)]
pub enum Principal {
    /// Authenticated operator. `role` is the **DB-current** role at
    /// handshake time (re-checked in [`authenticate_token`] per
    /// spec §6.4.1). `grants` is the set of `server_config.id` values
    /// the user has explicit per-server access to. Admins have an
    /// implicit grant on every server — represented by `is_admin = true`
    /// rather than expanding the grant set, to avoid stale reads if a
    /// new server is added mid-connection.
    User(UserPrincipal),
    /// Anonymous widget viewer. Authorised to subscribe ONLY to
    /// `server:{server_config_id}:widget`.
    Widget(WidgetPrincipal),
}

#[derive(Debug, Clone)]
pub struct UserPrincipal {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub is_admin: bool,
    pub is_at_least_moderator: bool,
}

impl From<AuthUser> for UserPrincipal {
    fn from(u: AuthUser) -> Self {
        Self {
            is_admin: u.is_admin(),
            is_at_least_moderator: u.is_at_least_moderator(),
            user_id: u.id,
            username: u.username,
            role: u.role,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WidgetPrincipal {
    pub widget_id: i64,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticateError {
    #[error("token did not match a JWT or widget credential")]
    Unauthorized,
    #[error("auth backend error")]
    Backend,
}

/// Resolve a handshake `?token=…` value to a [`Principal`].
///
/// Try the JWT path first. On `InvalidOrExpired` (the only error class
/// that means "the token shape is fine but isn't a JWT we recognise"),
/// try the widget-token table. `Disabled` and `Backend` errors short-
/// circuit — a JWT-shaped token whose user has been disabled MUST NOT
/// fall back to the widget path because doing so would let a disabled
/// user reuse their old JWT for anonymous widget access.
pub async fn resolve_principal(
    state: &AppState,
    token: &str,
) -> Result<Principal, AuthenticateError> {
    match authenticate_token(state, token).await {
        Ok(user) => Ok(Principal::User(user.into())),
        Err(WsAuthError::InvalidOrExpired) => resolve_widget(state, token).await,
        Err(WsAuthError::Disabled) => Err(AuthenticateError::Unauthorized),
        Err(WsAuthError::Backend) => Err(AuthenticateError::Backend),
    }
}

async fn resolve_widget(state: &AppState, token: &str) -> Result<Principal, AuthenticateError> {
    let widget = widgets::find_by_token(&state.db, token)
        .await
        .map_err(|_| AuthenticateError::Backend)?
        .ok_or(AuthenticateError::Unauthorized)?;
    Ok(Principal::Widget(WidgetPrincipal {
        widget_id: widget.id,
        server_config_id: widget.serverConfigId,
        virtual_server_id: widget.virtualServerId,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::jwt;
    use crate::auth::password;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{users, widgets as widget_repo};
    use crate::webquery::WebQueryPool;
    use std::sync::Arc;
    use std::time::Duration;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
        }
    }

    async fn seed_user(state: &AppState, role: &str, enabled: bool) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
            &state.db,
            users::NewUser {
                username: "alice".into(),
                passwordHash: hash,
                displayName: "Alice".into(),
                role: role.into(),
                enabled,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn jwt_path_resolves_user() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "admin", true).await;
        let token = jwt::mint_access(
            uid,
            "alice",
            "admin",
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();

        let p = resolve_principal(&state, &token).await.unwrap();
        match p {
            Principal::User(u) => {
                assert_eq!(u.user_id, uid);
                assert!(u.is_admin);
            }
            _ => panic!("expected User principal"),
        }
    }

    #[tokio::test]
    async fn widget_token_resolves_widget_principal() {
        let state = fresh_state().await;
        let widget = widget_repo::insert(
            &state.db,
            widget_repo::NewWidget {
                name: "lobby".into(),
                token: "tok-XYZ".into(),
                serverConfigId: 5,
                virtualServerId: 1,
                theme: "auto".into(),
                showChannelTree: true,
                showClients: true,
                hideEmptyChannels: false,
                maxChannelDepth: 5,
            },
        )
        .await
        .unwrap();

        let p = resolve_principal(&state, "tok-XYZ").await.unwrap();
        match p {
            Principal::Widget(w) => {
                assert_eq!(w.widget_id, widget.id);
                assert_eq!(w.server_config_id, 5);
                assert_eq!(w.virtual_server_id, 1);
            }
            _ => panic!("expected Widget principal"),
        }
    }

    #[tokio::test]
    async fn unknown_token_rejected() {
        let state = fresh_state().await;
        let err = resolve_principal(&state, "neither-jwt-nor-widget")
            .await
            .unwrap_err();
        assert!(matches!(err, AuthenticateError::Unauthorized));
    }

    #[tokio::test]
    async fn disabled_user_does_not_fall_back_to_widget() {
        // Mint a JWT for a now-disabled user; even if we created a widget
        // whose token happened to equal that JWT verbatim (impossible in
        // practice — different shape — but the reasoning matters), the
        // resolver MUST surface the disabled state, not silently downgrade
        // to anonymous widget access.
        let state = fresh_state().await;
        let uid = seed_user(&state, "viewer", true).await;
        let token = jwt::mint_access(
            uid,
            "alice",
            "viewer",
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();
        users::update(
            &state.db,
            uid,
            users::UserUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = resolve_principal(&state, &token).await.unwrap_err();
        assert!(matches!(err, AuthenticateError::Unauthorized));
    }
}
