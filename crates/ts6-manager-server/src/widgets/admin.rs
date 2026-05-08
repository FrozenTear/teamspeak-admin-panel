//! Operator widget CRUD — `/api/widgets` (spec §7.27, PURA-89).
//!
//! Mounted under `/api/widgets`. Auth contract:
//!
//! - `GET` (list, detail) → [`crate::auth::extractors::RequireAuth`]; any role.
//! - `POST` / `PATCH` / `DELETE` / `regenerate-token` →
//!   [`crate::auth::extractors::RequireModerator`] (admin OR moderator). Spec
//!   §7.27 originally says "Y+admin"; the issue scope and §6.13 RBAC table
//!   ratify "admin or moderator" so this matches the canonical operator
//!   surface.
//!
//! Cache invalidation:
//!
//! - `PATCH` and `DELETE` invalidate the public-data cache under the row's
//!   *current* token (spec §7.29).
//! - `POST /{id}/regenerate-token` invalidates under the **old** token before
//!   minting a new one — this is what makes the old URL 404 immediately.
//!
//! Tokens:
//!
//! - 21-character URL-safe random strings drawn from `OsRng` (spec §26.1). No
//!   `nanoid` crate dep — we already use this alphabet for refresh-token
//!   family ids in `crate::auth::refresh::generate_family_id`.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rand::{Rng, rngs::OsRng};
use serde::Serialize;
use ts6_manager_shared::widgets::{
    CreateWidgetRequest, UpdateWidgetRequest, WidgetEmbedUrls, WidgetSummary,
};

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAuth, RequireModerator};
use crate::repos::server_connections::{self, ServerConnection};
use crate::repos::widgets::{self as widget_repo, NewWidget, Widget, WidgetUpdate};

/// Spec §26.1 — token alphabet. URL-safe (`-` / `_`), 64 symbols / 6 bits per
/// char. Same character set as `nanoid` and as
/// [`crate::auth::refresh::generate_family_id`] (which we deliberately
/// don't depend on so the widget surface doesn't reach into the auth crate's
/// internals).
const TOKEN_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
const TOKEN_LENGTH: usize = 21;

/// Build the operator widgets sub-router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/widgets", get(list).post(create))
        .route("/api/widgets/{id}", get(detail).patch(patch).delete(delete))
        .route("/api/widgets/{id}/regenerate-token", post(regenerate_token))
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

fn err(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.to_string(),
            details: None,
        }),
    )
        .into_response()
}

fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "Not found")
}

/// Generate a fresh 21-character URL-safe token. ~125 bits of entropy.
fn generate_token() -> String {
    let mut rng = OsRng;
    let mut s = String::with_capacity(TOKEN_LENGTH);
    for _ in 0..TOKEN_LENGTH {
        let idx: usize = rng.gen_range(0..TOKEN_ALPHABET.len());
        s.push(TOKEN_ALPHABET[idx] as char);
    }
    s
}

/// Translate one widget row + its (optional) joined `server_connection` into
/// the wire-shape [`WidgetSummary`]. The route layer is responsible for
/// fetching the join row — keeping the conversion pure makes it easy to
/// unit-test.
fn summary_from(row: Widget, server: Option<&ServerConnection>) -> WidgetSummary {
    let embed_urls = WidgetEmbedUrls::for_token(&row.token);
    WidgetSummary {
        id: row.id,
        name: row.name,
        token: row.token,
        server_config_id: row.serverConfigId,
        virtual_server_id: row.virtualServerId,
        theme: row.theme,
        show_channel_tree: row.showChannelTree,
        show_clients: row.showClients,
        hide_empty_channels: row.hideEmptyChannels,
        max_channel_depth: row.maxChannelDepth,
        server_name: server.map(|s| s.name.clone()),
        server_host: server.map(|s| s.host.clone()),
        embed_urls,
        created_at: row.createdAt.to_rfc3339(),
        updated_at: row.updatedAt.to_rfc3339(),
    }
}

/// `GET /api/widgets` — every row, with the joined server_connection.
async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
) -> Result<Json<Vec<WidgetSummary>>, Response> {
    let rows = widget_repo::list(&state.db).await.map_err(|e| {
        tracing::error!(err = %e, "widgets admin: list failed");
        internal()
    })?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // The join is N+1 against `server_connection.id` — Phase 2 expects a
        // small handful of widgets per deployment, so a per-row lookup is
        // fine. If this becomes hot we promote to a single SurrealQL `RELATE`
        // query; for now correctness > micro-optimisation.
        let server = match server_connections::find_by_id(&state.db, row.serverConfigId).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, widget_id = row.id, "widgets admin: join lookup failed");
                None
            }
        };
        out.push(summary_from(row, server.as_ref()));
    }
    Ok(Json(out))
}

/// `GET /api/widgets/{id}`.
async fn detail(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<i64>,
) -> Result<Json<WidgetSummary>, Response> {
    let row = widget_repo::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: detail lookup failed");
            internal()
        })?
        .ok_or_else(not_found)?;
    let server = server_connections::find_by_id(&state.db, row.serverConfigId)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: detail join failed");
            internal()
        })?;
    Ok(Json(summary_from(row, server.as_ref())))
}

/// `POST /api/widgets`.
async fn create(
    State(state): State<AppState>,
    RequireModerator(_user): RequireModerator,
    Json(req): Json<CreateWidgetRequest>,
) -> Result<(StatusCode, Json<WidgetSummary>), Response> {
    if req.name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name must not be empty"));
    }

    // Confirm the target server exists. A widget that points at a deleted
    // server is permanently 404 from the public side, so reject the bind at
    // creation time rather than letting the row land orphaned.
    let server = server_connections::find_by_id(&state.db, req.server_config_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "widgets admin: server lookup on create failed");
            internal()
        })?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "serverConfigId does not resolve"))?;

    let new = NewWidget {
        name: req.name,
        token: generate_token(),
        serverConfigId: req.server_config_id,
        virtualServerId: req.virtual_server_id,
        theme: req.theme.unwrap_or_else(|| "dark".into()),
        showChannelTree: req.show_channel_tree.unwrap_or(true),
        showClients: req.show_clients.unwrap_or(true),
        hideEmptyChannels: req.hide_empty_channels.unwrap_or(false),
        maxChannelDepth: req.max_channel_depth.unwrap_or(5),
    };
    let row = widget_repo::insert(&state.db, new).await.map_err(|e| {
        tracing::error!(err = %e, "widgets admin: insert failed");
        internal()
    })?;
    Ok((StatusCode::CREATED, Json(summary_from(row, Some(&server)))))
}

/// `PATCH /api/widgets/{id}`. Spec §7.27 / §7.29 — MUST invalidate the
/// public-data cache on success. We snapshot the *current* token before the
/// MERGE and use it as the cache key, which keeps the contract sound even on
/// concurrent regenerate-token calls (the worst case is an extra invalidation,
/// never a stale cache hit).
async fn patch(
    State(state): State<AppState>,
    RequireModerator(_user): RequireModerator,
    Path(id): Path<i64>,
    Json(req): Json<UpdateWidgetRequest>,
) -> Result<Json<WidgetSummary>, Response> {
    let existing = widget_repo::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: patch pre-lookup failed");
            internal()
        })?
        .ok_or_else(not_found)?;
    let old_token = existing.token.clone();

    let patch = WidgetUpdate {
        name: req.name,
        theme: req.theme,
        showChannelTree: req.show_channel_tree,
        showClients: req.show_clients,
        hideEmptyChannels: req.hide_empty_channels,
        maxChannelDepth: req.max_channel_depth,
    };
    let updated = widget_repo::update(&state.db, id, patch)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: update failed");
            internal()
        })?
        .ok_or_else(not_found)?;

    // Spec §7.29 — drop the cache entry under the **old** token. The token
    // is unchanged on PATCH (only `regenerate-token` rotates it), so the new
    // and old tokens are equal here in practice; using `old_token` keeps the
    // invariant explicit and lets a future "PATCH also rotates" addition
    // continue to work without re-reading the row.
    state.widget_cache.invalidate(&old_token).await;

    let server = server_connections::find_by_id(&state.db, updated.serverConfigId)
        .await
        .ok()
        .flatten();
    Ok(Json(summary_from(updated, server.as_ref())))
}

/// `DELETE /api/widgets/{id}`. Cache-invalidates first so a concurrent reader
/// can't race past the eviction and re-warm the entry.
async fn delete(
    State(state): State<AppState>,
    RequireModerator(_user): RequireModerator,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    let existing = match widget_repo::find_by_id(&state.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_found()),
        Err(e) => {
            tracing::error!(err = %e, widget_id = id, "widgets admin: delete pre-lookup failed");
            return Err(internal());
        }
    };
    state.widget_cache.invalidate(&existing.token).await;
    widget_repo::delete(&state.db, id).await.map_err(|e| {
        tracing::error!(err = %e, widget_id = id, "widgets admin: delete failed");
        internal()
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/widgets/{id}/regenerate-token`. Spec §26.4: the old token MUST
/// 404 on the next public-route call.
///
/// Order matters here:
///
/// 1. Look up the row to capture the old token.
/// 2. Invalidate the cache under the old token (so even if step 3 races
///    against a public-route reader, the eviction has already happened).
/// 3. Mint + persist the new token.
///
/// If step 3 fails we leave the cache entry evicted — the next public read
/// will simply re-fetch from upstream under the same (still-current) old
/// token. No correctness loss.
async fn regenerate_token(
    State(state): State<AppState>,
    RequireModerator(_user): RequireModerator,
    Path(id): Path<i64>,
) -> Result<Json<WidgetSummary>, Response> {
    let existing = widget_repo::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: regenerate pre-lookup failed");
            internal()
        })?
        .ok_or_else(not_found)?;
    let old_token = existing.token.clone();

    state.widget_cache.invalidate(&old_token).await;

    let new_token = generate_token();
    let updated = widget_repo::set_token(&state.db, id, &new_token)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, widget_id = id, "widgets admin: set_token failed");
            internal()
        })?
        .ok_or_else(not_found)?;

    let server = server_connections::find_by_id(&state.db, updated.serverConfigId)
        .await
        .ok()
        .flatten();
    Ok(Json(summary_from(updated, server.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwt, password};
    use crate::crypto;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::server_connections::NewServerConnection;
    use crate::repos::users;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;
    use ts6_manager_shared::widgets::WidgetData;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-pura-89");
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
        }
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    async fn seed_user(state: &AppState, name: &str, role: &str) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
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
        .unwrap()
        .id
    }

    fn mint(state: &AppState, id: i64, name: &str, role: &str) -> String {
        jwt::mint_access(id, name, role, state.jwt_access_expiry, &state.jwt_secret).unwrap()
    }

    fn auth(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    async fn seed_server(state: &AppState) -> i64 {
        crate::repos::server_connections::insert(
            &state.db,
            NewServerConnection {
                name: "Primary".into(),
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

    fn create_body(server_id: i64) -> CreateWidgetRequest {
        CreateWidgetRequest {
            name: "Public".into(),
            server_config_id: server_id,
            virtual_server_id: 1,
            theme: Some("light".into()),
            show_channel_tree: Some(true),
            show_clients: Some(false),
            hide_empty_channels: Some(true),
            max_channel_depth: Some(3),
        }
    }

    async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("expected JSON, got {:?}: {e}", String::from_utf8_lossy(&bytes))
        })
    }

    fn json<T: serde::Serialize>(v: &T) -> Body {
        Body::from(serde_json::to_vec(v).unwrap())
    }

    fn fixture_data(theme: &str) -> WidgetData {
        use ts6_manager_shared::widgets::WidgetServer;
        WidgetData {
            name: "fix".into(),
            theme: theme.into(),
            // Slice E adds `serverConfigId` so the public-page SPA can derive
            // its WS topic; the value doesn't matter for cache-eviction tests.
            server_config_id: 1,
            show_channel_tree: true,
            show_clients: true,
            hide_empty_channels: false,
            max_channel_depth: 5,
            server: WidgetServer {
                name: "TS".into(),
                clients_online: 0,
                max_clients: 32,
                uptime_seconds: 0,
                platform: "TeamSpeak".into(),
                version: String::new(),
            },
            channels: Vec::new(),
        }
    }

    // ---------------------------------------------------------------------
    // Token generator
    // ---------------------------------------------------------------------

    #[test]
    fn generate_token_is_21_url_safe_chars() {
        let t = generate_token();
        assert_eq!(t.chars().count(), 21);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "non-URL-safe char in {t:?}"
        );
    }

    #[test]
    fn generate_token_is_random() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two consecutive tokens collided");
    }

    // ---------------------------------------------------------------------
    // RBAC
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn list_requires_auth() {
        let state = fresh_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/widgets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_works_for_any_role() {
        let state = fresh_state().await;
        for role in ["admin", "moderator", "viewer"] {
            let uid = seed_user(&state, role, role).await;
            let token = mint(&state, uid, role, role);
            let resp = app(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/api/widgets")
                        .header("authorization", auth(&token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "role={role} must list");
        }
    }

    #[tokio::test]
    async fn create_requires_at_least_moderator() {
        let state = fresh_state().await;
        let server = seed_server(&state).await;

        let viewer = seed_user(&state, "view", "viewer").await;
        let vt = mint(&state, viewer, "view", "viewer");
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&vt))
                    .header("content-type", "application/json")
                    .body(json(&create_body(server)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        for role in ["moderator", "admin"] {
            let uid = seed_user(&state, role, role).await;
            let t = mint(&state, uid, role, role);
            let resp = app(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/widgets")
                        .header("authorization", auth(&t))
                        .header("content-type", "application/json")
                        .body(json(&create_body(server)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED, "role={role} must create");
        }
    }

    // ---------------------------------------------------------------------
    // Round-trip — POST returns a token + embed URLs that round-trip the
    // configured visibility flags through the repo (the §26.4 verification
    // contract minus the public-route step, which is owned by Slice A).
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn post_then_get_round_trips_visibility_flags() {
        let state = fresh_state().await;
        let server = seed_server(&state).await;
        let aid = seed_user(&state, "a", "admin").await;
        let token = mint(&state, aid, "a", "admin");

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&token))
                    .header("content-type", "application/json")
                    .body(json(&create_body(server)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: WidgetSummary = read_json(resp).await;

        // Token shape is on the wire as 21 URL-safe chars.
        assert_eq!(created.token.chars().count(), 21);
        // Embed URLs name the token verbatim.
        assert_eq!(created.embed_urls.data_url, format!("/api/widget/{}/data", created.token));
        assert_eq!(created.embed_urls.svg_url, format!("/api/widget/{}/image.svg", created.token));
        assert_eq!(created.embed_urls.png_url, format!("/api/widget/{}/image.png", created.token));
        assert_eq!(created.embed_urls.page_url, format!("/widget/{}", created.token));
        // Server join made it onto the response.
        assert_eq!(created.server_name.as_deref(), Some("Primary"));
        // Visibility flags survived the round-trip.
        assert_eq!(created.theme, "light");
        assert!(!created.show_clients);
        assert!(created.hide_empty_channels);
        assert_eq!(created.max_channel_depth, 3);

        // GET /api/widgets/{id} returns the same row (including the same token).
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/widgets/{}", created.id))
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let fetched: WidgetSummary = read_json(resp).await;
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.token, created.token);
        assert_eq!(fetched.theme, "light");
    }

    #[tokio::test]
    async fn create_rejects_unknown_server_config_id() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "a", "admin").await;
        let t = mint(&state, aid, "a", "admin");
        let mut body = create_body(9999);
        body.server_config_id = 9999;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(json(&body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ---------------------------------------------------------------------
    // Cache invalidation — §7.29: PATCH / DELETE / regenerate-token MUST
    // drop the entry under the (old) token.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn patch_invalidates_widget_cache() {
        let state = fresh_state().await;
        let server = seed_server(&state).await;
        let aid = seed_user(&state, "a", "admin").await;
        let t = mint(&state, aid, "a", "admin");

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(json(&create_body(server)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let created: WidgetSummary = read_json(resp).await;

        // Pre-warm the cache as if a public route had served the data already.
        state
            .widget_cache
            .insert(created.token.clone(), fixture_data("light"))
            .await;
        assert!(state.widget_cache.get(&created.token).await.is_some());

        let patch_body = serde_json::json!({ "theme": "neon" });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/widgets/{}", created.id))
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&patch_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let updated: WidgetSummary = read_json(resp).await;
        assert_eq!(updated.theme, "neon");
        assert_eq!(updated.token, created.token, "PATCH must not rotate the token");

        // Cache MUST be cold after PATCH (spec §7.29 / §26.4).
        assert!(
            state.widget_cache.get(&created.token).await.is_none(),
            "PATCH must invalidate the public-data cache"
        );
    }

    #[tokio::test]
    async fn delete_invalidates_widget_cache_and_returns_204() {
        let state = fresh_state().await;
        let server = seed_server(&state).await;
        let aid = seed_user(&state, "a", "admin").await;
        let t = mint(&state, aid, "a", "admin");

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(json(&create_body(server)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let created: WidgetSummary = read_json(resp).await;
        state
            .widget_cache
            .insert(created.token.clone(), fixture_data("light"))
            .await;

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/widgets/{}", created.id))
                    .header("authorization", auth(&t))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        assert!(
            state.widget_cache.get(&created.token).await.is_none(),
            "DELETE must invalidate cache"
        );
        assert!(
            widget_repo::find_by_id(&state.db, created.id).await.unwrap().is_none(),
            "row must be gone"
        );
    }

    #[tokio::test]
    async fn regenerate_token_invalidates_old_token_and_mints_new_one() {
        let state = fresh_state().await;
        let server = seed_server(&state).await;
        let aid = seed_user(&state, "a", "admin").await;
        let t = mint(&state, aid, "a", "admin");

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/widgets")
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(json(&create_body(server)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let created: WidgetSummary = read_json(resp).await;
        let old_token = created.token.clone();
        state
            .widget_cache
            .insert(old_token.clone(), fixture_data("light"))
            .await;

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/widgets/{}/regenerate-token", created.id))
                    .header("authorization", auth(&t))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rotated: WidgetSummary = read_json(resp).await;

        assert_ne!(rotated.token, old_token, "token must rotate");
        assert_eq!(rotated.token.chars().count(), 21);
        assert_eq!(
            rotated.embed_urls.data_url,
            format!("/api/widget/{}/data", rotated.token)
        );

        // Old token: cache cold (§26.4 / §7.29).
        assert!(
            state.widget_cache.get(&old_token).await.is_none(),
            "old token cache entry must be evicted"
        );
        // Old token is no longer resolvable from the repo — i.e. the public
        // route's `find_by_token` will return None and emit 404.
        assert!(
            widget_repo::find_by_token(&state.db, &old_token).await.unwrap().is_none(),
            "old token must not resolve any row → public route 404s"
        );
        // New token resolves to the same row.
        let new_row = widget_repo::find_by_token(&state.db, &rotated.token)
            .await
            .unwrap()
            .expect("new token resolves");
        assert_eq!(new_row.id, created.id);
    }

    #[tokio::test]
    async fn patch_unknown_id_is_404() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "a", "admin").await;
        let t = mint(&state, aid, "a", "admin");
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/widgets/9999")
                    .header("authorization", auth(&t))
                    .header("content-type", "application/json")
                    .body(Body::from(b"{\"theme\":\"dark\"}".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
