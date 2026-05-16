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

pub mod admin;
pub mod auth;
pub mod control;
pub mod dashboard;
pub mod flows;
pub mod health;
pub mod moderation;
pub mod music_bots;
pub mod servers;
pub mod setup;
pub mod test_connection;
pub mod video_sources;
pub mod widgets;

pub use dashboard::{BandwidthSnapshot, DashboardData};
pub use health::Health;
