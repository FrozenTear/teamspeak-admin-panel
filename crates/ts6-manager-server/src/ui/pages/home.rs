//! `/` route — placeholder home surface.
//!
//! Spec is silent on what `/` should render in Phase 1 (PURA-37 acceptance:
//! "redirects to /dashboard server-side OR renders a tiny home/landing
//! surface"). We pick the redirect path: `/` exists so the brand wordmark
//! has a typed `Route::Home {}` to point at, but visiting it bounces the
//! operator to the dashboard via `nav.replace` so the URL doesn't get
//! stuck on a content-less landing page. A real marketing/landing surface
//! is a Phase 2 polish item.

use dioxus::prelude::*;

use crate::ui::routes::Route;

#[component]
pub fn Home() -> Element {
    let nav = use_navigator();
    // `replace` (not `push`) so the operator's back button doesn't ping-pong
    // between `/` and `/dashboard`.
    use_effect(move || {
        nav.replace(Route::DashboardPlaceholder {});
    });
    rsx! { "" }
}
