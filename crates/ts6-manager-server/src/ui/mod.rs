// Public component + token APIs are consumed by PURA-5 surfaces (login,
// setup, dashboard) in follow-up heartbeats. Dead-code warnings would only
// be true positives once those surfaces land and stop importing a primitive.
#![allow(dead_code)]

//! Dioxus UI for the TS6 Manager admin panel.
//!
//! Surface organisation follows [`study-documents/design-system/components.md`]:
//! - [`tokens`] — compile-time spacing/radius/motion constants for Rust-side math.
//! - [`theme`] — `<ThemeProvider>` + `use_reduced_motion()` per tokens.md §10.
//! - [`components`] — Dioxus primitives, one module per spec section.
//! - [`pages`] — route components, one per `Route` variant.
//! - [`routes`] — the [`Routable`] enum that maps URL → page.
//!
//! [PURA-14](/PURA/issues/PURA-14) replaces the Phase-0 placeholder with the
//! `/login` + dashboard placeholder pair. Subsequent PURA-5 children fill in
//! the rest of the §3.12 route table.

pub mod components;
pub mod layout;
pub mod pages;
pub mod routes;
pub mod theme;
pub mod tokens;

use dioxus::prelude::*;

use crate::client::dioxus::{provide_auth_gate, provide_session};
use crate::client::ws::{provide_ws_hub, use_ws_lifecycle};
use crate::ui::components::{provide_activity_feed, provide_toaster};
use crate::ui::routes::Route;

const TOKENS_CSS: Asset = asset!("/assets/tokens.css");
const COMPONENTS_CSS: Asset = asset!("/assets/components.css");
const LAYOUT_CSS: Asset = asset!("/assets/layout.css");

#[allow(non_snake_case)]
pub fn App() -> Element {
    let session = use_context_provider(provide_session);
    let storage = session.storage.clone();
    // Single shared refresh gate for every non-auth fetch (PURA-31). Mounting
    // it once here means every descendant route can `use_auth_gate()` and
    // inherit the single-flight refresh contract.
    use_context_provider(|| provide_auth_gate(session.clone()));
    // PURA-73 — WS hub, toast region, and activity feed providers live at
    // the App level so the reconnect state survives route changes and
    // every authed page can `use_ws_hub()` / `use_toaster()`.
    let hub = provide_ws_hub();
    let _toaster = provide_toaster();
    let _feed = provide_activity_feed();
    use_ws_lifecycle(hub);
    rsx! {
        document::Stylesheet { href: TOKENS_CSS }
        document::Stylesheet { href: COMPONENTS_CSS }
        document::Stylesheet { href: LAYOUT_CSS }
        theme::ThemeProvider { storage: storage,
            Router::<Route> {}
        }
    }
}
