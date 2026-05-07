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
pub mod pages;
pub mod routes;
pub mod theme;
pub mod tokens;

use dioxus::prelude::*;

use crate::client::dioxus::provide_session;
use crate::ui::routes::Route;

const TOKENS_CSS: Asset = asset!("/assets/tokens.css");
const COMPONENTS_CSS: Asset = asset!("/assets/components.css");
const LAYOUT_CSS: Asset = asset!("/assets/layout.css");

#[allow(non_snake_case)]
pub fn App() -> Element {
    use_context_provider(provide_session);
    rsx! {
        document::Stylesheet { href: TOKENS_CSS }
        document::Stylesheet { href: COMPONENTS_CSS }
        document::Stylesheet { href: LAYOUT_CSS }
        theme::ThemeProvider {
            Router::<Route> {}
        }
    }
}
