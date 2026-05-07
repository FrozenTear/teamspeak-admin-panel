//! Route enum for the operator SPA.
//!
//! PURA-14 lights up the bare minimum needed to unblock the auth slice:
//! `/login` and a placeholder `/` dashboard. Subsequent PURA-5 children
//! flesh out the remaining 23 routes from spec §3.12.

use dioxus::prelude::*;

use crate::ui::pages::{DashboardPlaceholder, LoginPage};

/// Top-level route table. Each variant maps to a component above; missing
/// routes (e.g. setup wizard, server selector) land in follow-up children.
#[rustfmt::skip]
#[derive(Clone, Debug, PartialEq, Routable)]
pub enum Route {
    #[route("/login?:next")]
    LoginPage { next: Option<String> },

    #[route("/")]
    DashboardPlaceholder {},
}
