//! `GET /api/audit` — v1.1 admin audit-log read surface.
//!
//! Spec deviation documented in `docs/admin/architecture.md` §8 (§7 does
//! not enumerate an audit route; v1.1 adds it). Admin-only via the
//! [`RequireAdmin`] extractor. Query semantics + pagination per
//! `docs/admin/http-api.md` §3.4.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use ts6_manager_shared::admin::{AuditEvent, Page};
use ts6_manager_shared::auth::ErrorResponse;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAdmin;
use crate::repos::admin_audit_log::{self, AdminAuditLogRow, ListFilter, MAX_LIMIT};

const DEFAULT_LIMIT: i64 = 50;

/// Build the `/api/audit` sub-router. Absolute path — `merge` at top-level.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/audit", axum::routing::get(list))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AuditQuery {
    actor_user_id: Option<i64>,
    target_kind: Option<String>,
    target_id: Option<i64>,
    kind: Option<String>,
    outcome: Option<String>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Page<AuditEvent>>, Response> {
    // `targetId` without `targetKind` is rejected so the composite index
    // stays usable (http-api.md §3.4).
    if q.target_id.is_some() && q.target_kind.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "targetId requires targetKind"));
    }

    // `outcome` must be one of the legal discriminants when present.
    if let Some(ref o) = q.outcome
        && o != "success"
        && o != "failure"
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "outcome must be 'success' or 'failure'",
        ));
    }

    let from = parse_time(q.from.as_deref(), "from")?;
    let to = parse_time(q.to.as_deref(), "to")?;
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return Err(err(StatusCode::BAD_REQUEST, "from must be <= to"));
    }

    // limit clamps silently into 1..=MAX_LIMIT; offset floors at 0.
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0).max(0);

    let filter = ListFilter {
        actorUserId: q.actor_user_id,
        kind: q.kind,
        targetKind: q.target_kind,
        targetId: q.target_id,
        outcome: q.outcome,
        from,
        to,
    };

    let (rows, total) = admin_audit_log::list(&state.db, &filter, limit, offset)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "list_audit: query failed");
            internal()
        })?;

    Ok(Json(Page {
        items: rows.into_iter().map(to_wire).collect(),
        total,
        limit,
        offset,
    }))
}

// Ok path is a small `Option<DateTime>`, error path the already-built large
// `Response`. Only called from `list`, which returns the same error type —
// boxing would just relocate the allocation. Matches the `video_sources`
// precedent.
#[allow(clippy::result_large_err)]
fn parse_time(raw: Option<&str>, field: &str) -> Result<Option<DateTime<Utc>>, Response> {
    match raw {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|_| {
                err(
                    StatusCode::BAD_REQUEST,
                    &format!("{field} must be an ISO-8601 UTC timestamp"),
                )
            }),
    }
}

fn to_wire(row: AdminAuditLogRow) -> AuditEvent {
    AuditEvent {
        id: row.id,
        occurred_at: row.occurredAt,
        inserted_at: row.insertedAt,
        actor_user_id: row.actorUserId,
        actor_username: row.actorUsername,
        kind: row.kind,
        target_kind: row.targetKind,
        target_id: row.targetId,
        target_label: row.targetLabel,
        payload: row.payload,
        outcome: row.outcome,
        error_msg: row.errorMsg,
        request_ip: row.requestIp,
        request_user_agent: row.requestUserAgent,
    }
}

fn err(status: StatusCode, body: &str) -> Response {
    (status, Json(ErrorResponse::new(body))).into_response()
}

fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwt, password};
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{admin_audit_log::NewAdminAuditLog, users};
    use axum::body::Body;
    use axum::http::{HeaderValue, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crate::crypto::init("test-seed-pura-235-audit");
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
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
        }
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    async fn seed_admin(state: &AppState) -> String {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        let id = users::insert(
            &state.db,
            users::NewUser {
                username: "admin".into(),
                passwordHash: hash,
                displayName: "Admin".into(),
                role: "admin".into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id;
        jwt::mint_access(
            id,
            "admin",
            "admin",
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    fn auth(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
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

    async fn seed_event(state: &AppState, kind: &str) {
        admin_audit_log::insert(
            &state.db,
            NewAdminAuditLog {
                actorUserId: Some(1),
                actorUsername: "admin".into(),
                kind: kind.into(),
                targetKind: Some("user".into()),
                targetId: Some(2),
                targetLabel: Some("mod1".into()),
                payload: None,
                outcome: "success".into(),
                errorMsg: None,
                requestIp: None,
                requestUserAgent: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn audit_requires_admin() {
        let state = fresh_state().await;
        // Viewer token.
        let vid = users::insert(
            &state.db,
            users::NewUser {
                username: "v".into(),
                passwordHash: "$argon2id$v=19$x".into(),
                displayName: "V".into(),
                role: "viewer".into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id;
        let token = jwt::mint_access(
            vid,
            "v",
            "viewer",
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn audit_lists_paginated_and_newest_first() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        for n in 0..3 {
            seed_event(&state, &format!("kind{n}")).await;
        }
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?limit=2")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let page: Page<AuditEvent> = read_json(resp).await;
        assert_eq!(page.total, 3);
        assert_eq!(page.limit, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].kind, "kind2");
    }

    #[tokio::test]
    async fn audit_filters_by_kind() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        seed_event(&state, "userCreated").await;
        seed_event(&state, "userDeleted").await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?kind=userDeleted")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let page: Page<AuditEvent> = read_json(resp).await;
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].kind, "userDeleted");
    }

    #[tokio::test]
    async fn audit_limit_clamps_to_max() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?limit=9999")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let page: Page<AuditEvent> = read_json(resp).await;
        assert_eq!(
            page.limit, MAX_LIMIT,
            "limit must clamp to the documented cap"
        );
    }

    #[tokio::test]
    async fn audit_target_id_without_target_kind_returns_400() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?targetId=5")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn audit_from_after_to_returns_400() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?from=2026-05-20T00:00:00Z&to=2026-05-19T00:00:00Z")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn audit_bad_timestamp_returns_400() {
        let state = fresh_state().await;
        let token = seed_admin(&state).await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/audit?from=not-a-date")
                    .header("authorization", auth(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
