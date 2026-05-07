//! Spec §6.9 / §6.10 — HTTP-layer hardening: CORS, security headers.
//!
//! Phase 1 SECURITY slice 1.5: this module ships [`cors`] and [`headers`]
//! middleware. Slice 2 will add [`rate_limit`] (login/refresh/setup/webhook
//! windows per §6.8) and [`proxy`] (single-hop X-Forwarded-For parser per
//! §6.8 last paragraph) once the auth REST surface lands and we have real
//! handlers to gate.

#![allow(dead_code)] // consumed by main.rs; re-exports for future workstreams

pub mod cors;
pub mod headers;

pub use cors::cors_layer;
#[allow(unused_imports)] // SecurityHeadersStack re-exported for future composers
pub use headers::{SecurityHeadersStack, security_headers_stack};
