//! `/metrics` — Prometheus text exposition of WS hub counters (PURA-82).
//!
//! Phase 2 follow-up to [PURA-70]. The hub already collects five counters
//! in [`crate::ws::hub::Metrics`]; this route exposes them in the standard
//! Prometheus text format (`text/plain; version=0.0.4`).
//!
//! ## Auth gate
//!
//! `GET /metrics` is gated by [`RequireAdmin`] — the same JWT/role chain
//! as the rest of the admin surface. Operators wire Prometheus to the
//! endpoint with a `bearer_token_file:` (or `authorization:` block) using
//! a token minted for an admin user.
//!
//! Loopback-only gating was considered and rejected: in a 2-process
//! Quadlet/podman-compose deployment Prometheus runs in a sibling
//! container, not on `127.0.0.1`, and the loopback check would have to
//! interact with `TRUSTED_PROXY_HOPS` / XFF parsing — added complexity
//! for no operator benefit. JWT scrape is the standard Prometheus
//! pattern.
//!
//! ## What is and isn't exposed
//!
//! The five aggregate counters from `Metrics::snapshot()`. **No labels.**
//! `MetricsSnapshot` has no per-server, per-topic, or per-user fields, so
//! there is nothing in the snapshot that could leak PII (usernames,
//! server names, IPs, tokens). Per-topic gauges are explicitly out of
//! scope for this issue (impl plan: "operators ask first").
//!
//! `connections` is exposed as a **gauge** because
//! [`Hub::record_connection_close`] decrements it — it represents
//! currently-open WS sessions, not lifetime opens. The other four are
//! monotonic counters with the `_total` suffix per Prometheus naming
//! conventions.

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAdmin;
use crate::ws::hub::MetricsSnapshot;

/// Prometheus text format v0.0.4 — the de-facto exposition format every
/// scraper understands. OpenMetrics (`application/openmetrics-text`) is
/// strictly richer; we stick with the simpler dialect because the surface
/// here is five plain counters.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics))
}

async fn metrics(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let snapshot = state.ws_hub.metrics().snapshot();
    let body = render(&snapshot);
    (StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response()
}

/// Render a [`MetricsSnapshot`] as Prometheus text format.
///
/// Pulled out as a free function so the unit tests can assert the wire
/// shape without spinning up an axum router.
fn render(s: &MetricsSnapshot) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(768);
    let _ = writeln!(
        out,
        "# HELP ts6_ws_connections Currently-open WebSocket sessions on the hub."
    );
    let _ = writeln!(out, "# TYPE ts6_ws_connections gauge");
    let _ = writeln!(out, "ts6_ws_connections {}", s.connections);
    let _ = writeln!(
        out,
        "# HELP ts6_ws_subscribe_ok_total Total successful WS topic subscriptions since process start."
    );
    let _ = writeln!(out, "# TYPE ts6_ws_subscribe_ok_total counter");
    let _ = writeln!(out, "ts6_ws_subscribe_ok_total {}", s.subscribe_ok);
    let _ = writeln!(
        out,
        "# HELP ts6_ws_subscribe_denied_total Total denied WS topic subscriptions since process start."
    );
    let _ = writeln!(out, "# TYPE ts6_ws_subscribe_denied_total counter");
    let _ = writeln!(out, "ts6_ws_subscribe_denied_total {}", s.subscribe_denied);
    let _ = writeln!(
        out,
        "# HELP ts6_ws_events_published_total Total events published through the WS hub since process start."
    );
    let _ = writeln!(out, "# TYPE ts6_ws_events_published_total counter");
    let _ = writeln!(out, "ts6_ws_events_published_total {}", s.events_published);
    let _ = writeln!(
        out,
        "# HELP ts6_ws_events_dropped_total Total events dropped to slow consumers since process start."
    );
    let _ = writeln!(out, "# TYPE ts6_ws_events_dropped_total counter");
    let _ = writeln!(out, "ts6_ws_events_dropped_total {}", s.events_dropped);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(c: u64, ok: u64, denied: u64, pub_: u64, drop: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            connections: c,
            subscribe_ok: ok,
            subscribe_denied: denied,
            events_published: pub_,
            events_dropped: drop,
        }
    }

    #[test]
    fn render_emits_all_five_metrics_with_values() {
        let body = render(&snap(3, 17, 2, 100, 1));

        // Each metric appears exactly once with its value, line-anchored.
        assert!(body.contains("\nts6_ws_connections 3\n"));
        assert!(body.contains("\nts6_ws_subscribe_ok_total 17\n"));
        assert!(body.contains("\nts6_ws_subscribe_denied_total 2\n"));
        assert!(body.contains("\nts6_ws_events_published_total 100\n"));
        assert!(body.contains("\nts6_ws_events_dropped_total 1\n"));
    }

    #[test]
    fn render_emits_help_and_type_lines_per_metric() {
        let body = render(&snap(0, 0, 0, 0, 0));

        // Every metric must have both a HELP and a TYPE comment per the
        // Prometheus exposition format. Counters get TYPE counter and the
        // _total suffix; the live-connections metric is a gauge.
        for (name, kind) in [
            ("ts6_ws_connections", "gauge"),
            ("ts6_ws_subscribe_ok_total", "counter"),
            ("ts6_ws_subscribe_denied_total", "counter"),
            ("ts6_ws_events_published_total", "counter"),
            ("ts6_ws_events_dropped_total", "counter"),
        ] {
            assert!(
                body.contains(&format!("# HELP {name} ")),
                "missing HELP for {name}: {body}"
            );
            assert!(
                body.contains(&format!("# TYPE {name} {kind}\n")),
                "missing or wrong TYPE for {name}: {body}"
            );
        }
    }

    #[test]
    fn render_zero_snapshot_emits_zero_values() {
        let body = render(&snap(0, 0, 0, 0, 0));
        assert!(body.contains("\nts6_ws_connections 0\n"));
        assert!(body.contains("\nts6_ws_subscribe_ok_total 0\n"));
        assert!(body.contains("\nts6_ws_subscribe_denied_total 0\n"));
        assert!(body.contains("\nts6_ws_events_published_total 0\n"));
        assert!(body.contains("\nts6_ws_events_dropped_total 0\n"));
    }

    #[test]
    fn render_emits_no_labels() {
        // `MetricsSnapshot` has no per-server / per-topic / per-user
        // fields, so the formatter must not invent any labels — those
        // would imply a dimension the data doesn't carry, and would be
        // the natural place for PII to leak in a future expansion.
        let body = render(&snap(1, 1, 1, 1, 1));
        assert!(
            !body.contains('{'),
            "metrics body unexpectedly contains label syntax: {body}"
        );
        assert!(
            !body.contains('}'),
            "metrics body unexpectedly contains label syntax: {body}"
        );
    }

    #[test]
    fn render_does_not_leak_pii_strings() {
        // Belt-and-braces: even if someone later wires `MetricsSnapshot`
        // through a path that smuggles in a username/IP/token, the wire
        // shape this function emits is purely numeric. Sanity-check that
        // the body is ASCII and contains no characters outside the
        // Prometheus text-format alphabet (alnum, underscore, space,
        // pound, newline, hyphen for the help text).
        let body = render(&snap(u64::MAX, 0, 0, 0, 0));
        for ch in body.chars() {
            assert!(
                ch.is_ascii(),
                "non-ASCII character {ch:?} in /metrics output: {body}"
            );
        }
    }

    #[test]
    fn render_uses_lf_line_endings_only() {
        // Prometheus's text format is LF-terminated; CRLF is non-conformant
        // and trips strict parsers. Guard against an editor or future
        // refactor sneaking a `\r\n` in.
        let body = render(&snap(1, 2, 3, 4, 5));
        assert!(
            !body.contains('\r'),
            "metrics body must not contain CR: {body:?}"
        );
        assert!(body.ends_with('\n'), "metrics body must end with LF");
    }

    #[test]
    fn content_type_matches_prometheus_text_format() {
        // Cheap regression guard: if someone changes the const and
        // forgets to update Prometheus scrape config docs, this fails.
        assert_eq!(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8");
    }
}

#[cfg(test)]
mod route_tests {
    //! End-to-end coverage of the auth gate and content-type — exercised
    //! through the real axum router so the [`RequireAdmin`] extractor
    //! chain is in the loop.

    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use crate::app_state::AppState;
    use crate::auth::{jwt, password};
    use crate::control::ControlBackendPool;
    use crate::crypto;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::users;
    use crate::webquery::WebQueryPool;
    use crate::ws::Hub;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-pura-82-metrics");
        let control = ControlBackendPool::new(false, db.clone());
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
        }
    }

    async fn seed_token(state: &AppState, role: &str) -> String {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        let row = users::insert(
            &state.db,
            users::NewUser {
                username: format!("metrics-{role}"),
                passwordHash: hash,
                displayName: role.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        jwt::mint_access(
            row.id,
            &row.username,
            &row.role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(super::router()).with_state(state)
    }

    #[tokio::test]
    async fn admin_token_returns_prometheus_body() {
        let state = fresh_state().await;
        // Touch a counter so the body has a non-zero value to assert on.
        state.ws_hub.record_connection_open();
        let token = seed_token(&state, "admin").await;

        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/metrics")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ct.starts_with("text/plain"),
            "expected Prometheus text content-type, got {ct}"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("ts6_ws_connections 1\n"), "body: {body}");
        assert!(body.contains("# TYPE ts6_ws_subscribe_ok_total counter"));
    }

    #[tokio::test]
    async fn non_admin_token_is_forbidden() {
        let state = fresh_state().await;
        let token = seed_token(&state, "viewer").await;

        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/metrics")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_token_is_unauthorized() {
        let state = fresh_state().await;

        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
