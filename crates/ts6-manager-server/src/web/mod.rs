//! HTTP-layer hardening for the axum listener.
//!
//! Phase 1 SECURITY:
//!
//! - [`cors`] — spec §6.10 `Access-Control-Allow-Origin` allowlist driven
//!   by `FRONTEND_URL`.
//! - [`headers`] — spec §6.9 sensible default security headers.
//! - [`proxy`] — spec §6.8 single-hop `X-Forwarded-For` trust policy.
//! - [`rate_limit`] — spec §6.8 per-IP bucket on `/api/auth/login` and
//!   `/api/auth/refresh`.

#![allow(dead_code)] // consumed by main.rs; re-exports for future workstreams

pub mod cors;
pub mod csp_nonce;
pub mod headers;
pub mod proxy;
pub mod rate_limit;
pub mod widget_security;

pub use cors::cors_layer;
pub use csp_nonce::nonce_csp_middleware;
#[allow(unused_imports)] // SecurityHeadersStack re-exported for future composers
pub use headers::{SecurityHeadersStack, security_headers_stack};
// `WidgetRateLimitState` is built behind the `make_widget_rate_limit_state`
// helper; the type is consumed via inference inside `main.rs`.
pub use widget_security::{
    make_widget_rate_limit_state, widget_rate_limit, widget_response_headers,
};
