//! v1.7 MoQ watch-together API.
//!
//! Panel call sequence (the static `/moq-spike/` player does not call
//! these routes; the watch page built on top of them will):
//!
//! 1. `POST /api/watch/sessions` with the panel access JWT and
//!    `{ "broadcast": "<moq namespace>" }`. `409 watch_session_active`
//!    when this user already holds a live session. Send `"replace": true`
//!    to revoke that session and its tickets.
//! 2. Open WebTransport to `relayUrl`. When `certHash` is present it is
//!    the lowercase SHA-256 hex of the relay certificate
//!    (`serverCertificateHashes`, algorithm `sha-256`). A null `certHash`
//!    means the sidecar fingerprint was not available and the player uses
//!    the OS trust store. Set `protocols` to `alpn` (`moq-lite-04`).
//! 3. Subscribe to `broadcast` / `videoTrack` and `broadcast` / `audioTrack`.
//! 4. `GET` and `PUT /api/watch/playhead/{broadcast}` with the access JWT
//!    to share one playhead for that broadcast. Writing requires the
//!    caller's live session to be on that broadcast.
//! 5. Before `ticketExpiresAt`, `POST /api/watch/tickets` with
//!    `{ "sessionId" }` to mint a fresh subscribe ticket for the same
//!    session.
//! 6. `DELETE /api/watch/sessions/{sessionId}` when the viewer leaves.
//!
//! `POST /api/watch/tickets/redeem` accepts the watch ticket (Bearer or
//! `{ "ticket" }`) and returns the same handoff. It is the only route
//! on this surface that does not take a panel access JWT. Every other
//! route uses [`crate::auth::extractors::RequireAuth`] and answers `401`
//! when that JWT is missing or invalid.
//!
//! Sessions are one per panel user, not one per broadcast: two accounts
//! may watch the same broadcast. The playhead is per broadcast. State is
//! process-local and idle sessions expire after 15 minutes without a
//! mint, refresh, redeem, or playhead call.
//!
//! Phase 1 does not present the ticket on the MoQ connection. The sidecar
//! listener stays `…/anon`. The ticket authorizes the panel to hand the
//! browser the relay URL, broadcast name, and cert hash, and it dies
//! when the session is released, replaced, or `exp` passes.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use ts6_manager_shared::auth::{ErrorResponse, auth_error_strings as msg};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::watch::{self, Admit};

/// WebTransport ALPN the sidecar advertises (ADR-0007).
const ALPN: &str = "moq-lite-04";
const VIDEO_TRACK: &str = "video";
const AUDIO_TRACK: &str = "audio";
/// A watch-together title is not a multi-day DVR. Reject runaway clocks.
const MAX_POSITION_MS: u64 = 48 * 60 * 60 * 1000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/watch/sessions", post(create_session))
        .route(
            "/api/watch/sessions/{session_id}",
            axum::routing::delete(delete_session),
        )
        .route("/api/watch/tickets", post(refresh_ticket))
        .route("/api/watch/tickets/redeem", post(redeem_ticket))
        .route(
            "/api/watch/playhead/{*broadcast}",
            get(get_playhead).put(put_playhead),
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSessionRequest {
    broadcast: String,
    #[serde(default)]
    replace: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshTicketRequest {
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct RedeemRequest {
    ticket: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayheadWrite {
    position_ms: u64,
    paused: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TicketGrant {
    session_id: String,
    ticket: String,
    ticket_expires_at: i64,
    relay_url: String,
    broadcast: String,
    cert_hash: Option<String>,
    alpn: &'static str,
    video_track: &'static str,
    audio_track: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RedeemResponse {
    session_id: String,
    ticket_expires_at: i64,
    relay_url: String,
    broadcast: String,
    cert_hash: Option<String>,
    alpn: &'static str,
    video_track: &'static str,
    audio_track: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayheadView {
    broadcast: String,
    position_ms: u64,
    paused: bool,
    updated_at: Option<i64>,
    updated_by: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchError {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    broadcast: Option<String>,
}

fn fail(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(WatchError {
            error: error.to_string(),
            session_id: None,
            broadcast: None,
        }),
    )
        .into_response()
}

fn fail_active(session_id: String, broadcast: String) -> Response {
    (
        StatusCode::CONFLICT,
        Json(WatchError {
            error: "watch_session_active".into(),
            session_id: Some(session_id),
            broadcast: Some(broadcast),
        }),
    )
        .into_response()
}

fn unauthorized(missing: bool) -> Response {
    let text = if missing {
        msg::NO_TOKEN
    } else {
        msg::INVALID_TOKEN
    };
    (StatusCode::UNAUTHORIZED, Json(ErrorResponse::new(text))).into_response()
}

fn relay_url(state: &AppState) -> Result<String, Response> {
    state
        .moq_public_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "moq_relay_unconfigured"))
}

fn checked_broadcast(raw: &str) -> Result<String, Response> {
    let name = raw.trim().trim_start_matches('/');
    if watch::valid_broadcast(name) {
        Ok(name.to_string())
    } else {
        Err(fail(StatusCode::BAD_REQUEST, "invalid_broadcast"))
    }
}

async fn cert_hash(state: &AppState) -> Option<String> {
    let sidecar = state.sidecar.as_ref()?;
    match sidecar.certificate_sha256().await {
        Ok(hash) => hash,
        Err(error) => {
            tracing::warn!(%error, "watch ticket: cert hash unavailable");
            None
        }
    }
}

fn grant_for(
    session_id: &str,
    ticket: String,
    ticket_expires_at: i64,
    relay_url: String,
    broadcast: &str,
    cert_hash: Option<String>,
) -> TicketGrant {
    TicketGrant {
        session_id: session_id.to_string(),
        ticket,
        ticket_expires_at,
        relay_url,
        broadcast: broadcast.to_string(),
        cert_hash,
        alpn: ALPN,
        video_track: VIDEO_TRACK,
        audio_track: AUDIO_TRACK,
    }
}

async fn create_session(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<TicketGrant>), Response> {
    let broadcast = checked_broadcast(&req.broadcast)?;
    let relay = relay_url(&state)?;
    let now = watch::unix_now();
    let session = match state.watch.admit(auth.id, &broadcast, req.replace, now) {
        Admit::Created(session) => session,
        Admit::Active {
            session_id,
            broadcast,
        } => return Err(fail_active(session_id, broadcast)),
    };
    let hash = cert_hash(&state).await;
    let (ticket, exp) =
        match watch::ticket::mint(auth.id, &session.id, &broadcast, &state.jwt_secret) {
            Ok(pair) => pair,
            Err(error) => {
                state.watch.release(auth.id, &session.id);
                tracing::error!(?error, user_id = auth.id, "watch ticket mint failed");
                return Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "internal"));
            }
        };
    tracing::info!(
        user_id = auth.id,
        broadcast = %broadcast,
        session_id = %session.id,
        replaced = req.replace,
        "watch session started"
    );
    Ok((
        StatusCode::CREATED,
        Json(grant_for(&session.id, ticket, exp, relay, &broadcast, hash)),
    ))
}

async fn delete_session(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(session_id): Path<String>,
) -> Result<StatusCode, Response> {
    if state.watch.release(auth.id, &session_id) {
        tracing::info!(user_id = auth.id, session_id = %session_id, "watch session released");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(fail(StatusCode::NOT_FOUND, "not_found"))
    }
}

async fn refresh_ticket(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(req): Json<RefreshTicketRequest>,
) -> Result<Json<TicketGrant>, Response> {
    let relay = relay_url(&state)?;
    let now = watch::unix_now();
    let session_id = req.session_id.trim();
    if session_id.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "invalid_session"));
    }
    let session = state
        .watch
        .touch_session(auth.id, session_id, now)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "not_found"))?;
    let hash = cert_hash(&state).await;
    let (ticket, exp) =
        watch::ticket::mint(auth.id, &session.id, &session.broadcast, &state.jwt_secret).map_err(
            |error| {
                tracing::error!(?error, user_id = auth.id, "watch ticket refresh failed");
                fail(StatusCode::INTERNAL_SERVER_ERROR, "internal")
            },
        )?;
    Ok(Json(grant_for(
        &session.id,
        ticket,
        exp,
        relay,
        &session.broadcast,
        hash,
    )))
}

async fn redeem_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<RedeemResponse>, Response> {
    let token = match bearer_token(&headers) {
        Some(token) => token,
        None if body.is_empty() => return Err(unauthorized(true)),
        None => {
            let parsed: RedeemRequest = serde_json::from_slice(&body)
                .map_err(|_| fail(StatusCode::BAD_REQUEST, "invalid_request"))?;
            let token = parsed.ticket.trim().to_string();
            if token.is_empty() {
                return Err(unauthorized(true));
            }
            token
        }
    };
    let claims =
        watch::ticket::verify(&token, &state.jwt_secret).map_err(|_| unauthorized(false))?;
    let now = watch::unix_now();
    let session = state
        .watch
        .touch_session(claims.uid, &claims.sid, now)
        .ok_or_else(|| unauthorized(false))?;
    if session.broadcast != claims.broadcast {
        return Err(unauthorized(false));
    }
    let relay = relay_url(&state)?;
    let hash = cert_hash(&state).await;
    Ok(Json(RedeemResponse {
        session_id: session.id,
        ticket_expires_at: claims.exp,
        relay_url: relay,
        broadcast: session.broadcast,
        cert_hash: hash,
        alpn: ALPN,
        video_track: VIDEO_TRACK,
        audio_track: AUDIO_TRACK,
    }))
}

async fn get_playhead(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(broadcast): Path<String>,
) -> Result<Json<PlayheadView>, Response> {
    let broadcast = require_playhead_session(&state, auth.id, &broadcast)?;
    Ok(Json(view_playhead(&state, &broadcast)))
}

async fn put_playhead(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(broadcast): Path<String>,
    Json(body): Json<PlayheadWrite>,
) -> Result<Json<PlayheadView>, Response> {
    if body.position_ms > MAX_POSITION_MS {
        return Err(fail(StatusCode::BAD_REQUEST, "invalid_playhead"));
    }
    let broadcast = require_playhead_session(&state, auth.id, &broadcast)?;
    let now = watch::unix_now();
    state
        .watch
        .put_playhead(&broadcast, body.position_ms, body.paused, auth.id, now);
    Ok(Json(view_playhead(&state, &broadcast)))
}

fn require_playhead_session(
    state: &AppState,
    user_id: i64,
    raw_broadcast: &str,
) -> Result<String, Response> {
    let broadcast = checked_broadcast(raw_broadcast)?;
    let now = watch::unix_now();
    let Some(session) = state.watch.live_session(user_id, now) else {
        return Err(fail(StatusCode::CONFLICT, "watch_session_required"));
    };
    if session.broadcast != broadcast {
        return Err(fail(StatusCode::CONFLICT, "watch_session_mismatch"));
    }
    if state.watch.touch_live(user_id, now).is_none() {
        return Err(fail(StatusCode::CONFLICT, "watch_session_required"));
    }
    Ok(broadcast)
}

fn view_playhead(state: &AppState, broadcast: &str) -> PlayheadView {
    match state.watch.playhead(broadcast) {
        Some(head) => PlayheadView {
            broadcast: broadcast.to_string(),
            position_ms: head.position_ms,
            paused: head.paused,
            updated_at: Some(head.updated_at),
            updated_by: Some(head.updated_by),
        },
        None => PlayheadView {
            broadcast: broadcast.to_string(),
            position_ms: 0,
            paused: true,
            updated_at: None,
            updated_by: None,
        },
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode as AxStatus};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    use crate::app_state::AppState;
    use crate::auth::{jwt, password};
    use crate::control::sidecar::SidecarClient;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::users;
    use crate::watch::ticket::{self, Claims};

    const CERT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const RELAY: &str = "https://relay.example:4443/anon";

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
            webquery: crate::webquery::WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: Some(RELAY.into()),
            yt_cookie: Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            music_dir: std::path::PathBuf::from("/data/music"),
            proxy_trust: crate::web::proxy::ProxyTrust::direct(),
            bug_reports: crate::bug_reports::unconfigured_sink(),
            watch: crate::watch::Store::new(),
        }
    }

    async fn boot_cert_sidecar() -> SidecarClient {
        let app = Router::new().route(
            "/certificate.sha256",
            get(|| async { format!("{CERT_HASH}\n") }),
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        SidecarClient::new(format!("http://127.0.0.1:{port}"))
    }

    async fn seed_token(state: &AppState, name: &str) -> (i64, String) {
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
                role: "viewer".into(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        let token = jwt::mint_access(
            row.id,
            &row.username,
            &row.role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();
        (row.id, token)
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    async fn call(
        state: AppState,
        method: Method,
        uri: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (AxStatus, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = app(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    #[tokio::test]
    async fn mint_returns_ticket_relay_broadcast_and_cert_hash() {
        let mut state = fresh_state().await;
        state.sidecar = Some(boot_cert_sidecar().await);
        let (user_id, token) = seed_token(&state, "watch-mint").await;

        let (status, bad) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "../etc"})),
        )
        .await;
        assert_eq!(status, AxStatus::BAD_REQUEST);
        assert_eq!(bad["error"], "invalid_broadcast");

        let (status, body) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{body}");
        assert_eq!(body["relayUrl"], RELAY);
        assert_eq!(body["broadcast"], "lavfi-spike");
        assert_eq!(body["certHash"], CERT_HASH);
        assert_eq!(body["alpn"], "moq-lite-04");
        assert_eq!(body["videoTrack"], "video");
        assert_eq!(body["audioTrack"], "audio");
        let session_id = body["sessionId"].as_str().unwrap().to_string();
        let ticket = body["ticket"].as_str().unwrap().to_string();
        let exp = body["ticketExpiresAt"].as_i64().unwrap();
        let claims = ticket::verify(&ticket, &state.jwt_secret).unwrap();
        assert_eq!(claims.uid, user_id);
        assert_eq!(claims.sid, session_id);
        assert_eq!(claims.broadcast, "lavfi-spike");
        assert_eq!(claims.exp, exp);
        assert!(jwt::verify_access(&ticket, &state.jwt_secret).is_err());

        let (status, refreshed) = call(
            state.clone(),
            Method::POST,
            "/api/watch/tickets",
            Some(&token),
            Some(json!({"sessionId": session_id})),
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{refreshed}");
        assert_eq!(refreshed["sessionId"], session_id);
        // Claims are (user, session, broadcast, iat). A refresh in the same
        // second repeats those claims, so the JWT bytes can match. Either
        // result is a live ticket for this session.
        let second = refreshed["ticket"].as_str().unwrap();
        assert_eq!(
            ticket::verify(second, &state.jwt_secret).unwrap().sid,
            session_id
        );

        let (status, redeemed) = call(
            state.clone(),
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&ticket),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{redeemed}");
        assert_eq!(redeemed["broadcast"], "lavfi-spike");
        assert_eq!(redeemed["relayUrl"], RELAY);
        assert_eq!(redeemed["certHash"], CERT_HASH);
        assert_eq!(redeemed["sessionId"], session_id);
    }

    #[tokio::test]
    async fn expired_ticket_is_rejected() {
        let state = fresh_state().await;
        let (user_id, token) = seed_token(&state, "watch-exp").await;
        let (status, body) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{body}");
        let session_id = body["sessionId"].as_str().unwrap();
        let live = body["ticket"].as_str().unwrap().to_string();
        let now = watch::unix_now();
        let expired = ticket::encode_claims(
            &Claims {
                purpose: ticket::PURPOSE.into(),
                uid: user_id,
                sid: session_id.into(),
                broadcast: "lavfi-spike".into(),
                iat: now - 30,
                exp: now - 1,
            },
            &state.jwt_secret,
        )
        .unwrap();

        let (status, rejected) = call(
            state.clone(),
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&expired),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::UNAUTHORIZED);
        assert_eq!(rejected["error"], msg::INVALID_TOKEN);

        let (status, _) = call(
            state,
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&live),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::OK);
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_401() {
        let state = fresh_state().await;
        let cases = [
            (
                Method::POST,
                "/api/watch/sessions",
                Some(json!({"broadcast": "lavfi-spike"})),
            ),
            (
                Method::POST,
                "/api/watch/tickets",
                Some(json!({"sessionId": "abc"})),
            ),
            (Method::DELETE, "/api/watch/sessions/abc", None),
            (Method::GET, "/api/watch/playhead/lavfi-spike", None),
            (
                Method::PUT,
                "/api/watch/playhead/lavfi-spike",
                Some(json!({"positionMs": 0, "paused": true})),
            ),
            (Method::POST, "/api/watch/tickets/redeem", None),
        ];
        for (method, uri, body) in cases {
            let (status, value) = call(state.clone(), method.clone(), uri, None, body).await;
            assert_eq!(status, AxStatus::UNAUTHORIZED, "{method} {uri} -> {value}");
            assert_eq!(value["error"], msg::NO_TOKEN);
        }

        let (status, value) = call(
            state,
            Method::POST,
            "/api/watch/sessions",
            Some("not-a-jwt"),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::UNAUTHORIZED);
        assert_eq!(value["error"], msg::INVALID_TOKEN);
    }

    #[tokio::test]
    async fn second_session_conflicts_until_replace_or_release() {
        let state = fresh_state().await;
        let (_, token) = seed_token(&state, "watch-one").await;

        let (status, first) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{first}");
        let first_id = first["sessionId"].as_str().unwrap().to_string();
        let first_ticket = first["ticket"].as_str().unwrap().to_string();
        assert!(first["certHash"].is_null());

        let (status, conflict) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "other-room"})),
        )
        .await;
        assert_eq!(status, AxStatus::CONFLICT, "{conflict}");
        assert_eq!(conflict["error"], "watch_session_active");
        assert_eq!(conflict["sessionId"], first_id);
        assert_eq!(conflict["broadcast"], "lavfi-spike");

        let (status, _) = call(
            state.clone(),
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&first_ticket),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::OK);

        let (status, replaced) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "other-room", "replace": true})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{replaced}");
        let second_id = replaced["sessionId"].as_str().unwrap();
        assert_ne!(second_id, first_id);
        assert_eq!(replaced["broadcast"], "other-room");

        let (status, revoked) = call(
            state.clone(),
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&first_ticket),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::UNAUTHORIZED, "{revoked}");

        let (status, _) = call(
            state.clone(),
            Method::DELETE,
            &format!("/api/watch/sessions/{second_id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::NO_CONTENT);

        let (status, again) = call(
            state,
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{again}");
    }

    #[tokio::test]
    async fn two_viewers_share_a_playhead() {
        let state = fresh_state().await;
        let (_, token_a) = seed_token(&state, "watch-a").await;
        let (_, token_b) = seed_token(&state, "watch-b").await;
        for token in [&token_a, &token_b] {
            let (status, body) = call(
                state.clone(),
                Method::POST,
                "/api/watch/sessions",
                Some(token),
                Some(json!({"broadcast": "room/demo"})),
            )
            .await;
            assert_eq!(status, AxStatus::CREATED, "{body}");
        }

        let (status, empty) = call(
            state.clone(),
            Method::GET,
            "/api/watch/playhead/room/demo",
            Some(&token_a),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{empty}");
        assert_eq!(empty["positionMs"], 0);
        assert_eq!(empty["paused"], true);
        assert!(empty["updatedAt"].is_null());

        let (status, written) = call(
            state.clone(),
            Method::PUT,
            "/api/watch/playhead/room/demo",
            Some(&token_a),
            Some(json!({"positionMs": 15000, "paused": false})),
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{written}");

        let (status, seen) = call(
            state.clone(),
            Method::GET,
            "/api/watch/playhead/room/demo",
            Some(&token_b),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{seen}");
        assert_eq!(seen["broadcast"], "room/demo");
        assert_eq!(seen["positionMs"], 15000);
        assert_eq!(seen["paused"], false);
        assert!(seen["updatedBy"].is_i64());

        let (status, paused) = call(
            state,
            Method::PUT,
            "/api/watch/playhead/room/demo",
            Some(&token_b),
            Some(json!({"positionMs": 15100, "paused": true})),
        )
        .await;
        assert_eq!(status, AxStatus::OK, "{paused}");
        assert_eq!(paused["positionMs"], 15100);
        assert_eq!(paused["paused"], true);
    }

    #[tokio::test]
    async fn playhead_requires_a_session_on_that_broadcast() {
        let state = fresh_state().await;
        let (_, token) = seed_token(&state, "watch-head").await;

        let (status, missing) = call(
            state.clone(),
            Method::PUT,
            "/api/watch/playhead/lavfi-spike",
            Some(&token),
            Some(json!({"positionMs": 1, "paused": false})),
        )
        .await;
        assert_eq!(status, AxStatus::CONFLICT, "{missing}");
        assert_eq!(missing["error"], "watch_session_required");

        let (status, _) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED);

        let (status, mismatch) = call(
            state.clone(),
            Method::GET,
            "/api/watch/playhead/other-room",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::CONFLICT, "{mismatch}");
        assert_eq!(mismatch["error"], "watch_session_mismatch");

        let (status, huge) = call(
            state,
            Method::PUT,
            "/api/watch/playhead/lavfi-spike",
            Some(&token),
            Some(json!({"positionMs": 999_999_999_999u64, "paused": false})),
        )
        .await;
        assert_eq!(status, AxStatus::BAD_REQUEST, "{huge}");
        assert_eq!(huge["error"], "invalid_playhead");
    }

    #[tokio::test]
    async fn relay_unconfigured_is_503_and_access_token_does_not_redeem() {
        let mut state = fresh_state().await;
        state.moq_public_url = None;
        let (_, token) = seed_token(&state, "watch-norelay").await;
        let (status, body) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"], "moq_relay_unconfigured");

        state.moq_public_url = Some(RELAY.into());
        let (status, minted) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&token),
            Some(json!({"broadcast": "lavfi-spike"})),
        )
        .await;
        assert_eq!(status, AxStatus::CREATED, "{minted}");
        let watch_ticket = minted["ticket"].as_str().unwrap().to_string();

        let (status, _) = call(
            state.clone(),
            Method::POST,
            "/api/watch/sessions",
            Some(&watch_ticket),
            Some(json!({"broadcast": "lavfi-spike", "replace": true})),
        )
        .await;
        assert_eq!(status, AxStatus::UNAUTHORIZED);

        let (status, redeemed) = call(
            state,
            Method::POST,
            "/api/watch/tickets/redeem",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, AxStatus::UNAUTHORIZED, "{redeemed}");
        assert_eq!(redeemed["error"], msg::INVALID_TOKEN);
    }
}
