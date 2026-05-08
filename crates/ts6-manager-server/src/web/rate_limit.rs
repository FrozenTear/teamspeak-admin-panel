//! Spec §6.8 — per-IP rate limit for authentication routes.
//!
//! Quota: 15 requests / 15 minutes, source-IP keyed (after the
//! single-hop X-Forwarded-For trust policy in [`crate::web::proxy`]).
//! Applies to `POST /api/auth/login` and `POST /api/auth/refresh`. The
//! `/api/setup/*` and `/api/bots/webhook/*` buckets land with their
//! respective routes.
//!
//! On rate-limit denial the middleware returns:
//!
//! - HTTP **429 Too Many Requests**
//! - body `{"error":"Too many attempts, please try again later"}`
//!   (verbatim from `auth_error_strings::RATE_LIMIT_AUTH`)
//! - `Retry-After: <seconds>` header derived from the GCRA-computed
//!   next-allowed time
//!
//! Backed by `governor` (token-bucket / GCRA) with a `DashMap`-backed
//! per-key state store. Eviction of dormant keys is governor's
//! responsibility — the limiter retains keys until they replenish to
//! full capacity, then drops them.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use ts6_manager_shared::auth::{ErrorResponse, auth_error_strings as msg};

use crate::web::proxy;

/// Per-IP rate limiter type alias — verbose enough that callers cope
/// with the parameter list more easily.
pub type AuthRateLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Middleware state carrying both the limiter and the proxy-trust
/// configuration. Cheap to clone (single `Arc`).
#[derive(Clone)]
pub struct RateLimitState {
    pub limiter: Arc<AuthRateLimiter>,
    pub trusted_hops: u8,
}

/// Spec §6.8 quota for `POST /api/auth/login` and `POST /api/auth/refresh`:
/// 15 requests / 15 minutes, source-IP keyed.
///
/// GCRA encoding: replenish 1 cell per minute (15 min / 15 reqs), with a
/// max burst of 15. The 16th attempt within 5 seconds (spec §6.13 verify
/// test) hits an empty bucket and is denied.
pub fn auth_quota() -> Quota {
    let burst = NonZeroU32::new(15).expect("15 != 0");
    Quota::with_period(Duration::from_secs(60))
        .expect("60s != 0 — quota construction is infallible")
        .allow_burst(burst)
}

/// Construct the keyed rate limiter for the auth routes. One limiter is
/// shared across login + refresh so the spec's "per source IP across the
/// auth surface" intent holds — a single attacker can't side-step the
/// 15/15min budget by alternating endpoints.
pub fn make_auth_limiter() -> Arc<AuthRateLimiter> {
    Arc::new(RateLimiter::keyed(auth_quota()))
}

/// Axum middleware: per-IP rate limit on the wrapped routes.
///
/// Resolves the source IP via the configured trusted-proxy policy,
/// consults the limiter, and either calls `next.run(req)` or returns
/// the spec-mandated 429 envelope.
///
/// `ConnectInfo<SocketAddr>` is read from request extensions rather than
/// a typed extractor parameter so the middleware survives test harnesses
/// that don't wire `into_make_service_with_connect_info` — those harnesses
/// inject the value manually via `Request::extensions_mut`. When neither
/// the listener nor the test inject ConnectInfo the middleware
/// fails-safe to `0.0.0.0`, which collapses every limiter key onto a
/// single bucket — wide-open is unacceptable, fail-closed-ish (every
/// request shares one quota) is the safer default.
pub async fn rate_limit_auth(
    State(state): State<RateLimitState>,
    req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let ip = proxy::client_ip(req.headers(), peer, state.trusted_hops);

    match state.limiter.check_key(&ip) {
        Ok(_) => next.run(req).await,
        Err(not_until) => {
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            tracing::info!(
                client_ip = %ip,
                retry_after_secs = wait.as_secs(),
                "rate-limit denied auth request"
            );
            rate_limit_response(wait)
        }
    }
}

fn rate_limit_response(wait: Duration) -> Response {
    // Retry-After per RFC 7231 §7.1.3 — integer seconds. Round up so a
    // sub-second wait still surfaces as `Retry-After: 1` (the client
    // should not retry immediately).
    let secs = wait.as_secs().max(1);
    let retry_after = HeaderValue::from_str(&secs.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("60"));
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ErrorResponse::new(msg::RATE_LIMIT_AUTH)),
    )
        .into_response();
    resp.headers_mut().insert("retry-after", retry_after);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Method, Request};
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn noop_handler() -> &'static str {
        "ok"
    }

    fn app(state: RateLimitState) -> Router {
        Router::new()
            .route("/login", post(noop_handler))
            .layer(from_fn_with_state(state, rate_limit_auth))
    }

    fn req_from(addr: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .body(Body::empty())
            .unwrap();
        let socket: SocketAddr = format!("{addr}:50000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(socket));
        req
    }

    fn req_from_with_xff(addr: &str, xff: &str) -> Request<Body> {
        let mut req = req_from(addr);
        req.headers_mut().insert(
            "x-forwarded-for",
            HeaderValue::from_str(xff).unwrap(),
        );
        req
    }

    #[tokio::test]
    async fn fifteen_attempts_pass_sixteenth_is_429() {
        let state = RateLimitState {
            limiter: make_auth_limiter(),
            trusted_hops: 0,
        };
        let app = app(state);

        // Spec §6.13 verify: 16 attempts within 5 seconds → 16th = 429.
        // We hammer them as fast as cargo can spin the executor.
        for n in 1..=15 {
            let resp = app.clone().oneshot(req_from("198.51.100.5")).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "attempt {n} should be allowed inside the 15-burst window"
            );
        }
        let resp = app.clone().oneshot(req_from("198.51.100.5")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        // Body matches the spec's exact wire string.
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["error"],
            "Too many attempts, please try again later",
            "spec §6.8 mandates this exact body"
        );
    }

    #[tokio::test]
    async fn rate_limit_response_carries_retry_after_header() {
        let state = RateLimitState {
            limiter: make_auth_limiter(),
            trusted_hops: 0,
        };
        let app = app(state);

        // Burn the bucket.
        for _ in 0..15 {
            let _ = app.clone().oneshot(req_from("203.0.113.99")).await.unwrap();
        }
        let resp = app.oneshot(req_from("203.0.113.99")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = resp
            .headers()
            .get("retry-after")
            .expect("Retry-After must be set per spec §6.8");
        let secs: u64 = header.to_str().unwrap().parse().unwrap();
        assert!(
            (1..=60).contains(&secs),
            "Retry-After should be a positive int seconds value (got {secs}); the bucket replenishes at 1/min so the wait is at most ~60 s"
        );
    }

    #[tokio::test]
    async fn distinct_ips_track_independent_buckets() {
        let state = RateLimitState {
            limiter: make_auth_limiter(),
            trusted_hops: 0,
        };
        let app = app(state);

        // Burn IP A's bucket entirely.
        for _ in 0..15 {
            let _ = app.clone().oneshot(req_from("198.51.100.5")).await.unwrap();
        }
        let a_burned = app.clone().oneshot(req_from("198.51.100.5")).await.unwrap();
        assert_eq!(a_burned.status(), StatusCode::TOO_MANY_REQUESTS);

        // IP B starts fresh.
        let b_first = app.oneshot(req_from("203.0.113.10")).await.unwrap();
        assert_eq!(b_first.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn xff_keying_is_used_when_trusted_hops_set() {
        // Two requests from the SAME peer IP but with different XFF
        // values — when trusted_hops=1, the limiter must key by the XFF
        // entry, so each "client" has its own 15-burst budget.
        let state = RateLimitState {
            limiter: make_auth_limiter(),
            trusted_hops: 1,
        };
        let app = app(state);

        // Burn client X's bucket via XFF.
        for _ in 0..15 {
            let r = app
                .clone()
                .oneshot(req_from_with_xff("203.0.113.7", "198.51.100.5"))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
        let x_burned = app
            .clone()
            .oneshot(req_from_with_xff("203.0.113.7", "198.51.100.5"))
            .await
            .unwrap();
        assert_eq!(x_burned.status(), StatusCode::TOO_MANY_REQUESTS);

        // Same peer IP but a different XFF → different bucket → allowed.
        let y_first = app
            .oneshot(req_from_with_xff("203.0.113.7", "198.51.100.6"))
            .await
            .unwrap();
        assert_eq!(y_first.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn trusted_hops_zero_uses_peer_only_even_with_xff() {
        // When trusted_hops=0, attacker-supplied XFF must not let them
        // escape the limiter by rotating the header value — bucket keying
        // sticks to ConnectInfo.
        let state = RateLimitState {
            limiter: make_auth_limiter(),
            trusted_hops: 0,
        };
        let app = app(state);

        // Burn the peer bucket — caller supplies a different XFF on every
        // request hoping to bypass the limit.
        for n in 0..15 {
            let xff = format!("10.{n}.0.1");
            let r = app
                .clone()
                .oneshot(req_from_with_xff("203.0.113.99", &xff))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
        let burned = app
            .oneshot(req_from_with_xff("203.0.113.99", "10.99.0.1"))
            .await
            .unwrap();
        assert_eq!(
            burned.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "with trusted_hops=0, XFF rotation must NOT let an attacker bypass per-peer limits"
        );
    }

    #[test]
    fn auth_quota_matches_spec_window() {
        let q = auth_quota();
        // 15 cells max — spec §6.8 line 1094.
        assert_eq!(q.burst_size().get(), 15);
        // 60-second replenishment interval = 1 token/min, derived from
        // spec's 15/15min window. Verifies the encoding choice doesn't
        // drift if the period helper is touched later.
        assert_eq!(q.replenish_interval(), Duration::from_secs(60));
    }

    // Silence unused-helper warning when individual tests are commented
    // out during local debugging — keeps CI quiet.
    #[allow(dead_code)]
    fn _h(h: HeaderMap) -> HeaderMap {
        h
    }
}
