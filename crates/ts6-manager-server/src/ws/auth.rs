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
    /// Access-token `exp` (unix seconds) captured at handshake. The
    /// session loop closes the socket once this instant has passed.
    pub access_exp: i64,
}

impl UserPrincipal {
    pub(crate) fn from_user(u: AuthUser, access_exp: i64) -> Self {
        Self {
            is_admin: u.is_admin(),
            is_at_least_moderator: u.is_at_least_moderator(),
            user_id: u.id,
            username: u.username,
            role: u.role,
            access_exp,
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
        Ok(user) => {
            let access_exp = crate::auth::jwt::verify_access(token, &state.jwt_secret)
                .map(|claims| claims.exp)
                .unwrap_or(0);
            Ok(Principal::User(UserPrincipal::from_user(user, access_exp)))
        }
        Err(WsAuthError::InvalidOrExpired) => resolve_widget(state, token).await,
        Err(WsAuthError::Disabled) => Err(AuthenticateError::Unauthorized),
        Err(WsAuthError::Backend) => Err(AuthenticateError::Backend),
    }
}

/// Re-check a live socket's credential.
///
/// Operators: the access token must still be unexpired, and the user
/// row must still exist, be enabled, and hold the same role captured at
/// handshake. Widgets: the row must still exist. A database error fails
/// closed.
pub(crate) async fn credential_still_valid(
    db: &crate::db::Database,
    principal: &Principal,
) -> bool {
    match principal {
        Principal::User(user) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(i64::MAX);
            if now >= user.access_exp {
                return false;
            }
            match crate::repos::users::find_by_id(db, user.user_id).await {
                Ok(Some(row)) => row.enabled && row.role == user.role,
                _ => false,
            }
        }
        Principal::Widget(widget) => {
            matches!(
                crate::repos::widgets::find_by_id(db, widget.widget_id).await,
                Ok(Some(_))
            )
        }
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
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: None,
            yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            music_dir: std::path::PathBuf::from("/data/music"),
            proxy_trust: crate::web::proxy::ProxyTrust::direct(),
            bug_reports: crate::bug_reports::unconfigured_sink(),
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

    #[tokio::test]
    async fn live_session_closes_when_role_changes_or_token_expires() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "moderator", true).await;
        let token = jwt::mint_access(
            uid,
            "alice",
            "moderator",
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();
        let principal = resolve_principal(&state, &token).await.unwrap();
        assert!(credential_still_valid(&state.db, &principal).await);

        users::update(
            &state.db,
            uid,
            users::UserUpdate {
                role: Some("viewer".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!credential_still_valid(&state.db, &principal).await);

        users::update(
            &state.db,
            uid,
            users::UserUpdate {
                role: Some("moderator".into()),
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!credential_still_valid(&state.db, &principal).await);

        let expired = Principal::User(UserPrincipal {
            user_id: uid,
            username: "alice".into(),
            role: "moderator".into(),
            is_admin: false,
            is_at_least_moderator: true,
            access_exp: 1,
        });
        assert!(!credential_still_valid(&state.db, &expired).await);
    }

    #[tokio::test]
    async fn widget_session_closes_when_the_row_is_gone() {
        let state = fresh_state().await;
        let widget = widget_repo::insert(
            &state.db,
            widget_repo::NewWidget {
                name: "lobby".into(),
                token: "tok-live".into(),
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
        let principal = Principal::Widget(WidgetPrincipal {
            widget_id: widget.id,
            server_config_id: 5,
            virtual_server_id: 1,
        });
        assert!(credential_still_valid(&state.db, &principal).await);
        widget_repo::delete(&state.db, widget.id).await.unwrap();
        assert!(!credential_still_valid(&state.db, &principal).await);
    }
}
