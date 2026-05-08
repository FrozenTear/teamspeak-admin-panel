//! Public widgets — workstream WIDGETS (§3.11) — PURA-72.
//!
//! Slice A (this module) ships the JSON pipeline:
//!
//! - [`themes`]   — six built-in palettes (`WIDGET_THEMES`).
//! - [`snapshot`] — `serverinfo`/`channellist`/`clientlist` →
//!   [`ts6_manager_shared::widgets::WidgetData`] per spec §27.1, with
//!   spacer detection, depth prune, hide-empty, and §7.29 redaction.
//! - [`cache`]    — token-keyed 45 s TTL cache (spec §7.29).
//! - [`routes`]   — `GET /api/widget/{token}/data`. The shared
//!   `resolve_widget_data` driver is reused by Slices B/C for the SVG /
//!   PNG endpoints so all three formats render off the same snapshot
//!   under a single TTL window.
//!
//! Out of scope for this slice (tracked as child issues):
//!
//! - SVG renderer per §27.3 — [PURA-72-B].
//! - PNG via `resvg` per §27.4 — [PURA-72-C].
//! - Operator widget CRUD (§7.27) — [PURA-72-D].
//! - Public `/widget/:token` HTML page (§3.9) — [PURA-72-E].
//! - Cross-cutting CORS/CSP/rate-limit on widget routes — [PURA-72-F].
//! - Operator Widget Manager UI (Chapter 34) — [PURA-72-G].

#![allow(dead_code)] // Slice B/C/D consumers land in follow-up tickets.

pub mod cache;
pub mod routes;
pub mod snapshot;
pub mod themes;

pub use cache::WidgetCache;
