//! Route enum for the operator SPA.
//!
//! - `/login` is its own surface (matches spec §28.2 — login has no chrome).
//! - Every authenticated route renders inside [`AppShell`] (sidebar + header
//!   + main outlet, per `components.md` §11). PURA-5's remaining children
//!   slot more pages into the same layout block.

use dioxus::prelude::*;

use crate::ui::layout::AppShell;
use crate::ui::pages::{DashboardPlaceholder, LoginPage};

#[rustfmt::skip]
#[derive(Clone, Debug, PartialEq, Routable)]
pub enum Route {
    #[route("/login?:next")]
    LoginPage { next: Option<String> },

    #[layout(AppShell)]
    #[route("/")]
    DashboardPlaceholder {},
}
