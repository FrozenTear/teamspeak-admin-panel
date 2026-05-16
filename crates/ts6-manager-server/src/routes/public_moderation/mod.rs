//! Phase 9.2 public moderation surface — `/api/public/moderation/*`
//! (PURA-307, workstream `9.2-public-routes` of
//! [PURA-269](/PURA/issues/PURA-269) §9).
//!
//! This is the **unauthenticated** half of the moderation system: a
//! report intake form and an appeal form, reachable by people who — by
//! definition — have no operator account (brief §1). It is deliberately
//! its own router, its own module, and its own prefix:
//!
//! - **No `RequirePermission`, no JWT.** Every handler here is reachable
//!   without an account. The distinct `/api/public/moderation/*` prefix
//!   means an operator can firewall / reverse-proxy / rate-limit it
//!   independently of the authenticated API, and a code reviewer can see
//!   at a glance which handlers are unauthenticated (brief §6).
//! - **Identity is a single-use token, not a session.** Reports carry a
//!   `report_challenge_token` (UID-bound, delivered over the TS6 poke
//!   channel); appeals carry a case-scoped `appeal_token`. Both are
//!   minted + verified by [`crate::routes::moderation::tokens`].
//!
//! ## Abuse posture (brief §4, outermost first)
//!
//! 1. **Per-server opt-in.** The whole surface is off by default. The
//!    `app_setting` flags `moderation.reports.enabled` /
//!    `moderation.appeals.enabled` are independent kill-switches; a
//!    disabled flag makes the corresponding routes `404` ([`flag_enabled`]).
//! 2. **Body-size cap before parse** ([`MAX_BODY_BYTES`]) — an
//!    unauthenticated route must not buffer an unbounded body.
//! 3. **Rate limiting** ([`rate_limit_and_attribute`]): a per-source-IP
//!    token bucket on every route, plus a per-reporter-UID bucket on
//!    report submission. `X-Forwarded-For` is trusted for IP attribution
//!    **only** when the direct peer is inside a configured proxy CIDR —
//!    default-deny otherwise (brief §6 hook 2, [`resolve_client_ip`]).
//! 4. **Identity gate.** The token check is the primary anti-spam
//!    control; it runs before any case / report row is touched.
//! 5. **Telemetry.** Every submission emits a structured log line and
//!    bumps [`metrics`] `moderation_public_submissions_total{kind,outcome}`
//!    (brief §4.8).

// Handlers and helpers here uniformly thread errors as a pre-built
// `Response`. That makes `Result<_, Response>` the module's idiom; the
// `Response` Err variant is large, so the lint fires on every helper —
// allow it module-wide rather than peppering per-fn attributes.
#![allow(clippy::result_large_err)]

pub mod appeals;
pub mod forms;
pub mod metrics;
pub mod reports;

#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use ipnet::IpNet;
use sha2::{Digest, Sha256};
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::repos::app_settings;

/// Hard request-body cap for the public surface. The largest legitimate
/// field is the free-text `statement` (≤ 4 KB, brief §4.5); 16 KiB leaves
/// ample room for the token, the other JSON fields, and encoding overhead
/// while still rejecting an unbounded body before it is parsed.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Server-enforced max length of any free-text field (brief §4.5 — 4 KB).
pub const MAX_TEXT_LEN: usize = 4096;

/// Per-source-IP token bucket — brief §4.4 default of 20 public requests
/// per hour. GCRA encoding: burst of 20, replenish 1 cell per 180 s
/// (3600 s / 20). Covers token-probing and form-submit floods across
/// every public route.
type IpLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;
/// Per-reporter-UID token bucket — brief §4.4 default of 3 reports per
/// hour. Burst of 3, replenish 1 cell per 1200 s. Keyed by the UID the
/// `report_challenge_token` is bound to, so mass-reporting needs mass real
/// connected TS6 clients.
type UidLimiter = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

/// Process-wide per-IP limiter. One instance — the bucket map is the
/// limiter's state, so a single shared limiter is exactly the intent.
static PER_IP_LIMITER: LazyLock<IpLimiter> = LazyLock::new(|| {
    let quota = Quota::with_period(Duration::from_secs(180))
        .expect("180s != 0")
        .allow_burst(NonZeroU32::new(20).expect("20 != 0"));
    RateLimiter::keyed(quota)
});

/// Process-wide per-reporter-UID limiter (report submission only).
static PER_UID_LIMITER: LazyLock<UidLimiter> = LazyLock::new(|| {
    let quota = Quota::with_period(Duration::from_secs(1200))
        .expect("1200s != 0")
        .allow_burst(NonZeroU32::new(3).expect("3 != 0"));
    RateLimiter::keyed(quota)
});

/// The trusted client IP resolved by [`rate_limit_and_attribute`] and
/// stashed in request extensions for the handlers (they hash it into a
/// `moderation_report` / `moderation_appeal` `sourceIpHash`). A newtype so
/// it cannot be confused with any other `IpAddr` extension.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// Build the public moderation router.
///
/// `trusted_proxy_cidrs` is the operator's reverse-proxy allow-list
/// (`MODERATION_TRUSTED_PROXY_CIDRS`); an empty list is default-deny — XFF
/// is ignored and the rate limiter keys on the direct peer.
///
/// Absolute paths — the caller `merge`s this into the top-level router.
pub fn router(trusted_proxy_cidrs: Vec<IpNet>) -> Router<AppState> {
    let proxy = Arc::new(trusted_proxy_cidrs);
    Router::new()
        .route(
            "/api/public/moderation/request-report-link",
            post(reports::request_report_link),
        )
        .route("/api/public/moderation/reports", post(reports::submit))
        .route(
            "/api/public/moderation/case",
            get(appeals::view_redacted_case),
        )
        .route("/api/public/moderation/appeals", post(appeals::submit))
        // PURA-309 — the server-rendered public report / appeal web forms.
        // These are HTML pages, not the `/api/*` JSON surface; the poke /
        // ban-reason links the token layer mints point exactly here. They
        // share this router's rate-limit + body-size middleware (page GETs
        // count against the per-IP bucket — token-probing the appeal page
        // is itself the threat, brief §4.4).
        .route(
            "/moderation/report",
            get(forms::report_form).post(forms::report_submit),
        )
        .route(
            "/moderation/appeal",
            get(forms::appeal_form).post(forms::appeal_submit),
        )
        // Inner: per-IP rate limit + client-IP attribution. Outer: body
        // cap, so an oversized body is rejected before anything else.
        .layer(axum::middleware::from_fn_with_state(
            proxy,
            rate_limit_and_attribute,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

// ---------------------------------------------------------------------
// Client-IP attribution + per-IP rate limit middleware
// ---------------------------------------------------------------------

/// Resolve the IP a public request should be attributed to.
///
/// `X-Forwarded-For` is honoured **only** when the direct peer is inside
/// one of `trusted` — the operator's reverse-proxy allow-list (brief §6
/// hook 2). If the allow-list is empty, or the peer is not in it, XFF is
/// ignored entirely and the direct connection IP is used. This is
/// default-deny: an attacker who is not behind the trusted proxy cannot
/// rotate a spoofed XFF header to escape the per-IP limiter.
///
/// When the peer *is* a trusted proxy, the rightmost parseable XFF entry
/// is taken — the convention `crate::web::proxy` documents: a trusted
/// proxy appends the client IP it observed, so the rightmost entry is the
/// one our proxy added and the only one not client-controlled.
pub fn resolve_client_ip(headers: &HeaderMap, peer: SocketAddr, trusted: &[IpNet]) -> IpAddr {
    let peer_ip = peer.ip();
    let peer_is_trusted_proxy = trusted.iter().any(|net| net.contains(&peer_ip));
    if !peer_is_trusted_proxy {
        return peer_ip;
    }
    match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .rev()
            .find_map(|entry| entry.parse::<IpAddr>().ok())
            .unwrap_or(peer_ip),
        None => peer_ip,
    }
}

/// Middleware: attribute the request to a client IP, enforce the per-IP
/// token bucket, and stash the resolved [`ClientIp`] for the handlers.
///
/// `ConnectInfo<SocketAddr>` is read from request extensions rather than a
/// typed parameter so test harnesses that inject it manually
/// (`Request::extensions_mut`) work without `into_make_service_with_connect_info`.
/// A missing `ConnectInfo` fails safe to `0.0.0.0`, collapsing every key
/// onto one bucket — wide-open is unacceptable, one-shared-quota is the
/// safer default.
async fn rate_limit_and_attribute(
    axum::extract::State(trusted): axum::extract::State<Arc<Vec<IpNet>>>,
    mut req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let client_ip = resolve_client_ip(req.headers(), peer, &trusted);

    if let Err(not_until) = PER_IP_LIMITER.check_key(&client_ip) {
        let wait = not_until.wait_time_from(DefaultClock::default().now());
        tracing::info!(
            client_ip = %client_ip,
            retry_after_secs = wait.as_secs(),
            "public moderation: per-IP rate limit denied a request",
        );
        return too_many_requests(wait);
    }

    req.extensions_mut().insert(ClientIp(client_ip));
    next.run(req).await
}

/// Consult the per-reporter-UID token bucket. `Ok(())` when the request
/// may proceed; `Err` is the ready-to-return 429 response.
pub(super) fn check_uid_rate_limit(uid: &str) -> Result<(), Response> {
    match PER_UID_LIMITER.check_key(&uid.to_string()) {
        Ok(_) => Ok(()),
        Err(not_until) => {
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            tracing::info!(
                retry_after_secs = wait.as_secs(),
                "public moderation: per-UID report rate limit denied a submission",
            );
            Err(too_many_requests(wait))
        }
    }
}

fn too_many_requests(wait: Duration) -> Response {
    let secs = wait.as_secs().max(1);
    let mut resp = err(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many requests, please try again later",
        "rate_limited",
    );
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        resp.headers_mut().insert("retry-after", v);
    }
    resp
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// Build an [`wire::ErrorBody`] response. The public surface keeps its
/// error bodies terse — never echo server internals to an unauthenticated
/// caller.
pub(super) fn err(status: StatusCode, message: &str, code: &str) -> Response {
    (status, Json(wire::ErrorBody::new(message).with_code(code))).into_response()
}

/// `404` for a disabled per-server kill-switch. A disabled flag must be
/// indistinguishable from a route that does not exist (brief §4.1) — so
/// this is a plain not-found, no hint that the surface merely toggled off.
pub(super) fn disabled() -> Response {
    err(StatusCode::NOT_FOUND, "Not found", "not_found")
}

/// Generic invalid-token response. Every token failure — malformed,
/// unknown, wrong secret, expired, already used, wrong kind — collapses
/// here so the caller is not an enumeration oracle (token spec §4).
pub(super) fn invalid_token() -> Response {
    err(
        StatusCode::FORBIDDEN,
        "Invalid or expired token",
        "invalid_token",
    )
}

pub(super) fn validation(message: &str) -> Response {
    err(StatusCode::BAD_REQUEST, message, "validation")
}

pub(super) fn conflict(message: &str) -> Response {
    err(StatusCode::CONFLICT, message, "conflict")
}

pub(super) fn internal() -> Response {
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal server error",
        "internal",
    )
}

/// Read a per-server opt-in flag from `app_settings`. The flag is enabled
/// only when the value is exactly `"true"`; a missing key, any other
/// value, or a lookup error all read as **disabled** — the surface fails
/// closed (brief §4.1).
pub(super) async fn flag_enabled(state: &AppState, key: &str) -> bool {
    match app_settings::get(&state.db, key).await {
        Ok(Some(setting)) => setting.value.trim() == "true",
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(err = %e, flag = key, "public moderation: flag lookup failed; treating as disabled");
            false
        }
    }
}

/// `app_setting` key — report intake kill-switch.
pub(super) const FLAG_REPORTS_ENABLED: &str = "moderation.reports.enabled";
/// `app_setting` key — appeals kill-switch.
pub(super) const FLAG_APPEALS_ENABLED: &str = "moderation.appeals.enabled";
/// `app_setting` key — per-server CAPTCHA / proof-of-work toggle for the
/// public forms (PURA-309, brief §4.6). Default off; when on, the forms
/// render a verification placeholder. The challenge itself is **not**
/// implemented — integration is deferred per plan §7, so this is the
/// stubbed config seam, not an enforced gate.
pub(super) const FLAG_CAPTCHA_ENABLED: &str = "moderation.captcha.enabled";

/// Hash a client IP for `sourceIpHash` storage (brief §5 / §6 hook 6 —
/// abuse correlation without persisting raw PII).
///
/// The IPv4 space is small enough that a bare SHA-256 of an address is
/// brute-forceable, so the hash is **keyed** with the server's JWT secret:
/// `SHA-256(ip || 0x00 || secret)`. Two submissions from one IP collide
/// (the point — correlation), but the raw address cannot be recovered
/// without the server secret.
pub(super) fn hash_source_ip(ip: IpAddr, secret: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(ip.to_string().as_bytes());
    h.update([0u8]);
    h.update(secret);
    hex::encode(h.finalize())
}

/// Validate a free-text field: trimmed, non-empty, within [`MAX_TEXT_LEN`].
/// Returns the trimmed value or a ready-to-return 400.
pub(super) fn validate_text(field: &str, raw: &str) -> Result<String, Response> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(validation(&format!("{field} is required")));
    }
    if trimmed.chars().count() > MAX_TEXT_LEN {
        return Err(validation(&format!(
            "{field} exceeds the {MAX_TEXT_LEN}-character limit"
        )));
    }
    Ok(trimmed.to_string())
}
