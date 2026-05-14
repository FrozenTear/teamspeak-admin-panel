//! Phase 2 WebSocket hub — PURA-70.
//!
//! See `study-documents/ts6-manager-impl-deviations.md` (`D-WS`) for the
//! board-authored spec deviations introduced by this module: explicit
//! topic subscriptions and `lastEventId` reconnect replay on top of the
//! Chapter 8 envelope.
//!
//! Module layout:
//! - [`topic`]    — `Topic` enum + parse + per-kind auth requirement.
//! - [`envelope`] — server→client wire envelope and the per-server
//!   bounded reconnect ring buffer.
//! - [`auth`]     — `Principal` (JWT user / widget token) resolver.
//! - [`hub`]      — `Hub` shared state: per-server broadcast channels,
//!   ring buffer, ACL, metrics.
//! - [`session`]  — per-connection task: subscribe state machine,
//!   ping/pong heartbeat, bounded send queue with drop-on-overflow.
//!
//! Out-of-scope follow-ups owned by sibling tickets:
//! - PURA-70c — `/metrics` endpoint exposing the hub counters.
//!
//! The PURA-70b periodic dashboard tick republisher landed in
//! [`dashboard_tick`] (PURA-81); the PURA-70a TS server-notify event
//! source landed in [`server_notify`] (PURA-80).

#![allow(dead_code)] // consumed by PURA-70 follow-ups and the ws::session loop

pub mod auth;
pub mod dashboard_tick;
pub mod envelope;
pub mod hub;
pub mod server_notify;
pub mod session;
pub mod topic;
pub mod video_source_tick;

// Re-export the hub itself because [`crate::app_state::AppState`] holds
// it directly. Other types are reached via the fully-qualified paths
// (e.g. `ws::topic::Topic`) so callers in PURA-70 follow-ups can import
// only what they need.
pub use hub::Hub;
