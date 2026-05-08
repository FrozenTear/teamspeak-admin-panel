//! PURA-72 Slice F — widget-route security middleware (spec §7.28 / §27).
//!
//! Three concerns, all scoped to the public widget surface only:
//!
//! 1. **CORS relax** — `Access-Control-Allow-Origin: *` on `/api/widget/*`
//!    and `/widget/*`. No credentials. The rest of the SPA stays on the
//!    single-origin allowlist owned by [`super::cors`].
//! 2. **Frame embedding** — clear `X-Frame-Options` and emit
//!    `Content-Security-Policy: frame-ancestors *` on the same paths so
//!    third-party sites can `<iframe src="/widget/{token}">`. The host
//!    SPA's strict CSP / `X-Frame-Options: DENY` stay untouched everywhere
//!    else.
//! 3. **Rate limit** — independent per-token and per-IP `governor`
//!    buckets on `/api/widget/*`. Per-token shields upstream WebQuery
//!    from a single token spammer; per-IP shields the box from a single
//!    client iterating tokens. Both default to 30 req/min, env-tunable.
//!
//! ## Middleware shape
//!
//! - [`widget_rate_limit`] is a request-side middleware. Mount it via
//!   `route_layer` on the `/api/widget/*` router; it intentionally does
//!   NOT touch the SPA `/widget/*` HTML page (which is served by the
//!   dx fallback and never hits upstream).
//! - [`widget_response_headers`] is a response-side middleware. Layer it
//!   globally so it can reach both `/api/widget/*` and `/widget/*`. Place
//!   it OUTSIDE the nonce-CSP middleware so its CSP override wins on the
//!   way out.
//!
//! ## Logging
//!
//! Spec §26.1 forbids logging full widget tokens above DEBUG. Every
//! tracing call below renders tokens through [`short_token`] (first 4
//! chars + `…`), the same helper Slice A uses on the JSON path.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};

use super::proxy;

/// Per-token rate limiter. Token strings are widget-token / `player:botId`
/// keys extracted from the URL path.
pub type WidgetTokenRateLimiter =
    RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;
/// Per-source-IP rate limiter, source resolution per the `trusted_hops`
/// policy in [`super::proxy`].
pub type WidgetIpRateLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Middleware state for [`widget_rate_limit`]. Cheap to clone — just two
/// `Arc`s and a byte.
#[derive(Clone)]
pub struct WidgetRateLimitState {
    pub by_token: Arc<WidgetTokenRateLimiter>,
    pub by_ip: Arc<WidgetIpRateLimiter>,
    pub trusted_hops: u8,
}

/// Build a [`Quota`] of `rpm` cells per minute with bursts capped at the
/// same value (no over-the-window burst tolerance — the spec's "30 req/min"
/// is interpreted as a strict cap, not a hourly average).
pub fn quota_from_rpm(rpm: u32) -> Quota {
    let burst = NonZeroU32::new(rpm.max(1)).expect("rpm clamped to ≥1");
    Quota::with_period(Duration::from_secs(60))
        .expect("60s != 0 — quota construction is infallible")
        .allow_burst(burst)
}

/// Construct a [`WidgetRateLimitState`] from explicit limits. Callers
/// typically pull `per_token_rpm` / `per_ip_rpm` from
/// [`crate::config::Config`].
pub fn make_widget_rate_limit_state(
    per_token_rpm: u32,
    per_ip_rpm: u32,
    trusted_hops: u8,
) -> WidgetRateLimitState {
    WidgetRateLimitState {
        by_token: Arc::new(RateLimiter::keyed(quota_from_rpm(per_token_rpm))),
        by_ip: Arc::new(RateLimiter::keyed(quota_from_rpm(per_ip_rpm))),
        trusted_hops,
    }
}

/// True iff the request path belongs to the public widget surface.
///
/// Both `/api/widget/...` (data endpoints) and `/widget/...` (SPA page)
/// receive the relaxed CORS + frame headers. Trailing-slash-bare match is
/// included so a request to `/api/widget` itself doesn't slip past the
/// gate (axum may resolve such a request to a handler depending on routing).
fn is_widget_path(path: &str) -> bool {
    path == "/api/widget"
        || path == "/widget"
        || path.starts_with("/api/widget/")
        || path.starts_with("/widget/")
}

/// Extract the rate-limit token key from a `/api/widget/...` path.
///
/// - `/api/widget/{token}/...`         → `Some(token)`
/// - `/api/widget/player/{botId}/...`  → `Some("player:botId")` so the
///   per-bot HMAC token (§7.28.1) gets its own bucket distinct from
///   regular widget tokens.
/// - Any path without a non-empty token segment returns `None`; the
///   per-IP bucket still applies.
fn extract_token_key(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/widget/")?;
    let mut segments = rest.split('/').filter(|s| !s.is_empty());
    let first = segments.next()?;
    if first == "player" {
        let bot_id = segments.next()?;
        return Some(format!("player:{bot_id}"));
    }
    Some(first.to_string())
}

/// Rate-limit middleware for `/api/widget/*`. Denies with HTTP 429 when
/// either the per-IP or the per-token bucket is exhausted.
///
/// The middleware reads `ConnectInfo<SocketAddr>` from request extensions
/// (matching the auth-side limiter in [`super::rate_limit`]) so test
/// harnesses without `into_make_service_with_connect_info` can inject the
/// peer address directly. Missing ConnectInfo collapses to `0.0.0.0` —
/// every anonymous request shares one bucket, which is the safer default
/// than wide-open.
pub async fn widget_rate_limit(
    State(state): State<WidgetRateLimitState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let token_key = extract_token_key(&path);

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let ip = proxy::client_ip(req.headers(), peer, state.trusted_hops);
    let now = DefaultClock::default().now();

    if let Err(not_until) = state.by_ip.check_key(&ip) {
        let wait = not_until.wait_time_from(now);
        tracing::info!(
            client_ip = %ip,
            token_prefix = token_key.as_deref().map(short_token).unwrap_or_default(),
            retry_after_secs = wait.as_secs(),
            "rate-limit denied widget request (per-IP)"
        );
        return rate_limit_response(wait);
    }

    if let Some(t) = &token_key {
        if let Err(not_until) = state.by_token.check_key(t) {
            let wait = not_until.wait_time_from(now);
            tracing::info!(
                client_ip = %ip,
                token_prefix = %short_token(t),
                retry_after_secs = wait.as_secs(),
                "rate-limit denied widget request (per-token)"
            );
            return rate_limit_response(wait);
        }
    }

    next.run(req).await
}

fn rate_limit_response(wait: Duration) -> Response {
    let secs = wait.as_secs().max(1);
    let retry_after = HeaderValue::from_str(&secs.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("60"));
    let body = serde_json::json!({"error": "Widget rate limit exceeded"}).to_string();
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response();
    resp.headers_mut().insert("retry-after", retry_after);
    resp
}

/// Response-side middleware that overrides CORS / frame headers on the
/// widget surface. Layered globally; a no-op on every other path.
pub async fn widget_response_headers(req: Request, next: Next) -> Response {
    let path_is_widget = is_widget_path(req.uri().path());
    let mut resp = next.run(req).await;
    if !path_is_widget {
        return resp;
    }
    let h = resp.headers_mut();
    // CORS: anyone can read this, no credentials.
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.remove(header::ACCESS_CONTROL_ALLOW_CREDENTIALS);
    // Frame embedding: drop the strict XFO and replace the strict CSP with
    // the minimum frame-only policy. We rewrite, not just append, so any
    // upstream `frame-ancestors 'none'` from the nonce-CSP middleware loses.
    h.remove(header::X_FRAME_OPTIONS);
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors *"),
    );
    resp
}

/// Spec §26.1 — render a token as `first 4 chars + …` so tracing fields
/// stay searchable without leaking the credential. Used by both the
/// rate-limit logs above and Slice A's JSON handler.
pub fn short_token(token: &str) -> String {
    let mut chars: Vec<char> = token.chars().take(4).collect();
    if token.chars().count() > 4 {
        chars.push('…');
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request as HttpRequest, StatusCode, header};
    use axum::middleware::{from_fn, from_fn_with_state};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn req_to(path: &str, peer: &str) -> HttpRequest<Body> {
        let mut req = HttpRequest::builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let socket: SocketAddr = format!("{peer}:54321").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(socket));
        req
    }

    // -------- path classification + token extraction ---------------------

    #[test]
    fn is_widget_path_matches_both_prefixes() {
        assert!(is_widget_path("/api/widget/abc/data"));
        assert!(is_widget_path("/api/widget/abc/image.svg"));
        assert!(is_widget_path("/widget/abc"));
        assert!(is_widget_path("/widget/abc-with-dashes"));
        assert!(is_widget_path("/api/widget"));
        assert!(is_widget_path("/widget"));
    }

    #[test]
    fn is_widget_path_rejects_other_paths() {
        assert!(!is_widget_path("/api/auth/login"));
        assert!(!is_widget_path("/api/widgets/123"));
        assert!(!is_widget_path("/widgetz/abc"));
        assert!(!is_widget_path("/"));
        assert!(!is_widget_path("/api/widge"));
    }

    #[test]
    fn extract_token_key_grabs_first_segment() {
        assert_eq!(
            extract_token_key("/api/widget/abc/data"),
            Some("abc".to_string())
        );
        assert_eq!(
            extract_token_key("/api/widget/abc/image.svg"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_key_namespaces_player_widgets() {
        // §7.28.1 player-widget tokens get their own bucket so a busy
        // bot doesn't drain regular widget budget.
        assert_eq!(
            extract_token_key("/api/widget/player/42/data"),
            Some("player:42".to_string())
        );
        assert_eq!(
            extract_token_key("/api/widget/player/abc/bbcode"),
            Some("player:abc".to_string())
        );
    }

    #[test]
    fn extract_token_key_returns_none_when_missing() {
        assert_eq!(extract_token_key("/api/widget"), None);
        assert_eq!(extract_token_key("/api/widget/"), None);
        assert_eq!(extract_token_key("/api/widget/player"), None);
        assert_eq!(extract_token_key("/api/widget/player/"), None);
        assert_eq!(extract_token_key("/widget/abc"), None); // SPA path, not API
    }

    // -------- rate limit -------------------------------------------------

    fn rate_limit_app(state: WidgetRateLimitState) -> Router {
        Router::new()
            .route(
                "/api/widget/{token}/data",
                get(|| async { axum::response::Response::new(Body::from("ok")) }),
            )
            .layer(from_fn_with_state(state, widget_rate_limit))
    }

    /// Spec §6.13-style verify for the widget surface: 30 req/min cap is
    /// honoured per IP, the 31st in the same window is 429 with a sane
    /// `Retry-After`.
    #[tokio::test]
    async fn thirtieth_request_passes_thirty_first_is_429() {
        let state = make_widget_rate_limit_state(30, 30, 0);
        let app = rate_limit_app(state);
        let path = "/api/widget/sometoken/data";
        for n in 1..=30 {
            let resp = app.clone().oneshot(req_to(path, "203.0.113.5")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "request {n} should pass");
        }
        let resp = app.oneshot(req_to(path, "203.0.113.5")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry: u64 = resp
            .headers()
            .get("retry-after")
            .expect("Retry-After header")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (1..=120).contains(&retry),
            "Retry-After should be a positive int seconds value (got {retry})"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "Widget rate limit exceeded");
    }

    /// Per-IP bucket trips even when the second token is fresh — same IP,
    /// different token, must still be denied.
    #[tokio::test]
    async fn per_ip_bucket_protects_against_token_rotation() {
        let state = make_widget_rate_limit_state(30, 30, 0);
        let app = rate_limit_app(state);
        for _ in 0..30 {
            let _ = app
                .clone()
                .oneshot(req_to("/api/widget/tokenA/data", "203.0.113.6"))
                .await
                .unwrap();
        }
        let resp = app
            .oneshot(req_to("/api/widget/tokenB/data", "203.0.113.6"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "per-IP bucket must trip even on a fresh token"
        );
    }

    /// Per-token bucket trips even when the second IP is fresh — different
    /// IPs hammering the same token must still hit 429 once the token
    /// budget is gone (protects upstream WebQuery from a botnet).
    #[tokio::test]
    async fn per_token_bucket_protects_against_ip_rotation() {
        let state = make_widget_rate_limit_state(30, 1_000_000, 0);
        // Per-IP allowance is large so we can exhaust the per-token bucket
        // first by walking the IP space.
        let app = rate_limit_app(state);
        for n in 0..30 {
            let ip = format!("198.51.100.{n}");
            let resp = app
                .clone()
                .oneshot(req_to("/api/widget/sharedtoken/data", &ip))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "ip {ip} should pass");
        }
        let resp = app
            .oneshot(req_to("/api/widget/sharedtoken/data", "198.51.100.250"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "per-token bucket must trip even on a fresh IP"
        );
    }

    /// Independent buckets for distinct (ip, token) pairs — sanity-check
    /// the limiter isn't collapsing keys.
    #[tokio::test]
    async fn distinct_token_and_ip_use_independent_buckets() {
        let state = make_widget_rate_limit_state(30, 30, 0);
        let app = rate_limit_app(state);
        // Burn IP1 / tokenA bucket entirely.
        for _ in 0..30 {
            let _ = app
                .clone()
                .oneshot(req_to("/api/widget/tokenA/data", "203.0.113.7"))
                .await
                .unwrap();
        }
        let burned = app
            .clone()
            .oneshot(req_to("/api/widget/tokenA/data", "203.0.113.7"))
            .await
            .unwrap();
        assert_eq!(burned.status(), StatusCode::TOO_MANY_REQUESTS);
        // Distinct IP2 / tokenB still works.
        let fresh = app
            .oneshot(req_to("/api/widget/tokenB/data", "203.0.113.8"))
            .await
            .unwrap();
        assert_eq!(fresh.status(), StatusCode::OK);
    }

    // -------- response-header override -----------------------------------

    fn header_app() -> Router {
        // Mimic the production stack: a static handler emits a strict
        // X-Frame-Options + a strict CSP, and the widget middleware (layered
        // outside) overrides on widget paths.
        Router::new()
            .route(
                "/api/widget/{token}/data",
                get(|| async {
                    axum::response::Response::builder()
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "https://app.example")
                        .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
                        .header(header::X_FRAME_OPTIONS, "DENY")
                        .header(
                            header::CONTENT_SECURITY_POLICY,
                            "default-src 'self'; frame-ancestors 'none'",
                        )
                        .body(Body::from(r#"{"ok":true}"#))
                        .unwrap()
                }),
            )
            .route(
                "/widget/{token}",
                get(|| async {
                    axum::response::Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(header::X_FRAME_OPTIONS, "DENY")
                        .header(
                            header::CONTENT_SECURITY_POLICY,
                            "default-src 'self'; frame-ancestors 'none'",
                        )
                        .body(Body::from("<html></html>"))
                        .unwrap()
                }),
            )
            .route(
                "/api/auth/login",
                get(|| async {
                    axum::response::Response::builder()
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "https://app.example")
                        .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
                        .header(header::X_FRAME_OPTIONS, "DENY")
                        .header(
                            header::CONTENT_SECURITY_POLICY,
                            "default-src 'self'; frame-ancestors 'none'",
                        )
                        .body(Body::from("ok"))
                        .unwrap()
                }),
            )
            .layer(from_fn(widget_response_headers))
    }

    #[tokio::test]
    async fn widget_api_response_gets_relaxed_cors_and_frame_headers() {
        let app = header_app();
        let resp = app
            .oneshot(req_to("/api/widget/abc/data", "203.0.113.9"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("*"),
            "ACAO must be `*` on widget API"
        );
        assert!(
            h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none(),
            "credentials must NOT be allowed on the public widget surface"
        );
        assert!(
            h.get(header::X_FRAME_OPTIONS).is_none(),
            "X-Frame-Options must be cleared on the widget surface"
        );
        assert_eq!(
            h.get(header::CONTENT_SECURITY_POLICY)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("frame-ancestors *"),
            "CSP must be replaced with the iframe-permissive policy"
        );
    }

    #[tokio::test]
    async fn spa_widget_html_gets_relaxed_frame_headers() {
        let app = header_app();
        let resp = app
            .oneshot(req_to("/widget/abc", "203.0.113.10"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert!(h.get(header::X_FRAME_OPTIONS).is_none());
        assert_eq!(
            h.get(header::CONTENT_SECURITY_POLICY)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("frame-ancestors *"),
            "iframe-embedding contract must hold for the SPA widget page"
        );
    }

    #[tokio::test]
    async fn non_widget_response_is_untouched() {
        let app = header_app();
        let resp = app
            .oneshot(req_to("/api/auth/login", "203.0.113.11"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("https://app.example"),
            "non-widget paths must keep the strict CORS allowlist"
        );
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("true")
        );
        assert_eq!(
            h.get(header::X_FRAME_OPTIONS)
                .map(HeaderValue::to_str)
                .transpose()
                .unwrap(),
            Some("DENY"),
            "non-widget paths keep the strict X-Frame-Options"
        );
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "non-widget paths keep the strict CSP. Got: {csp}"
        );
    }

    // -------- short_token reuse contract ---------------------------------

    #[test]
    fn short_token_truncates_long_inputs() {
        assert_eq!(short_token("abcdefgh"), "abcd…");
    }

    #[test]
    fn short_token_passes_short_inputs_through() {
        assert_eq!(short_token("abc"), "abc");
        assert_eq!(short_token("abcd"), "abcd");
    }
}
