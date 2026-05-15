//! Client-side (Dioxus / WASM) glue: typed REST client, persisted session
//! store, refresh-on-401 single-flight interceptor.
//!
//! Layout (PURA-14 — auth client slice):
//! - [`auth`] — typed REST functions for the five `/api/auth` endpoints.
//! - [`storage`] — `Storage` trait + `LocalStorageStore` (`web-sys`) /
//!   `MemoryStore` (tests) so the session store can be unit-tested off-WASM.
//! - [`store`] — `AuthState` enum + a Dioxus `Signal`-backed session that
//!   hydrates from storage and clears on logout / refresh failure.
//! - [`session`] — single-flight refresh interceptor that wraps any
//!   `(access_token) -> Future<Result>` closure with refresh-on-401.
//! - [`api`] — generic authorized JSON fetch helper (PURA-31). Wraps the
//!   refresh gate around `gloo-net` so any non-auth endpoint inherits the
//!   single-flight refresh contract for free.
//! - [`setup`] — typed unauthenticated `/api/setup/*` calls (PURA-34). The
//!   wizard runs before any session exists so it bypasses the refresh gate
//!   on purpose; the `409 already_initialized` branch surfaces as its own
//!   error variant for branchless wizard logic.
//!
//! Cleanroom-safe: this module is built from the spec and the wire shapes in
//! `ts6_manager_shared::auth`; the reference repo is not consulted.
//!
//! WASM-only at runtime. Server-side renders go through Dioxus SSR but never
//! exercise these code paths — everything that would touch `window` /
//! `localStorage` / `fetch` is gated behind `cfg(target_arch = "wasm32")`.

#![allow(dead_code)] // public APIs are consumed by /login + future routes.

pub mod api;
pub mod auth;
pub mod debug;
pub mod dioxus;
pub mod music_bots;
pub mod servers;
pub mod session;
pub mod settings;
pub mod setup;
pub mod storage;
pub mod store;
pub mod ui_prefs;
pub mod users;
pub mod video_sources;
pub mod ws;
