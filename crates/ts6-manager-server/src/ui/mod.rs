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
//!
//! The Phase 0 placeholder lives behind the `App` component; PURA-5 replaces
//! the body with the real router + layout chrome once the auth REST surface
//! lands. This crate exposes only the scaffold needed to start writing real
//! surfaces against — no `<AppShell>`, `/login`, or routing in this slice.

pub mod components;
pub mod theme;
pub mod tokens;

use dioxus::prelude::*;

const TOKENS_CSS: Asset = asset!("/assets/tokens.css");
const COMPONENTS_CSS: Asset = asset!("/assets/components.css");

#[allow(non_snake_case)]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: TOKENS_CSS }
        document::Stylesheet { href: COMPONENTS_CSS }
        theme::ThemeProvider {
            div { class: "app-root",
                h1 { "TS6 Manager" }
                p { "Phase 0 placeholder — Dioxus fullstack server is up." }
            }
        }
    }
}
