//! Wire-format types shared between the Dioxus client (WASM) and the axum server.
//!
//! This crate must stay WASM-clean: no `tokio`, `sqlx`, `reqwest`, or other native-only
//! dependencies. JSON field names mirror the spec verbatim where they appear in HTTP
//! request/response bodies — those are part of the external contract.
//!
//! Future feature crates land their DTOs here so that `ts6-manager-server` (server) and
//! the Dioxus frontend crate (added in PURA-5 / FE-PAGES) can share a single source of
//! truth via `serde`.

#![deny(missing_debug_implementations)]
// Wire-shape JSON keys (e.g., `accessToken`, `passwordHash`) are camelCase per
// spec; matching them in Rust struct fields keeps `#[derive(Serialize)]` direct
// without per-field rename attributes.
#![allow(non_snake_case)]

pub mod auth;
pub mod health;

pub use health::Health;
