//! Route enum for the operator SPA.
//!
//! - `/login` is its own surface (matches spec §28.2 — login has no chrome).
//! - `/setup` is the first-run wizard (PURA-34). Lives outside `AppShell`
//!   for the same reason as `/login` — there's no operator account yet, so
//!   the auth gate would bounce us in a loop.
//! - `/` is a typed `Home` route so the sidebar brand can use the SPA
//!   `Link` primitive (`components.md` §11.2). Its component immediately
//!   `nav.replace`s to `/dashboard`; a real landing surface is Phase 2.
//! - `/dashboard` is the dashboard surface. Keeping it on a distinct path
//!   from the brand-target `/` is what lets `dioxus_router::Link` auto-emit
//!   `aria-current="page"` on exactly one element (PURA-37).
//! - Every authenticated route renders inside [`AppShell`] (sidebar +
//!   header + main outlet, per `components.md` §11). PURA-5's remaining
//!   children slot more pages into the same layout block.

use dioxus::prelude::*;

use crate::ui::layout::AppShell;
use crate::ui::pages::{
    BansPage, ChannelsPage, ClientsPage, DashboardPlaceholder, Home, LoginPage, LogsPage,
    PublicWidgetPage, ServerInfoPage, SetupPage,
};

#[rustfmt::skip]
#[derive(Clone, Debug, PartialEq, Routable)]
pub enum Route {
    #[route("/login?:next")]
    LoginPage { next: Option<String> },

    #[route("/setup")]
    SetupPage {},

    // PURA-72 Slice E — public widget page lives outside `AppShell` so it
    // renders without the operator chrome (sidebar / header / auth gate).
    // The token in the URL is the only credential.
    #[route("/widget/:token")]
    PublicWidgetPage { token: String },

    #[layout(AppShell)]
    #[route("/")]
    Home {},

    #[route("/dashboard")]
    DashboardPlaceholder {},

    // PURA-73 — Phase 2 control surfaces.
    #[route("/clients")]
    ClientsPage {},

    #[route("/channels")]
    ChannelsPage {},

    #[route("/bans")]
    BansPage {},

    #[route("/server-info")]
    ServerInfoPage {},

    #[route("/logs")]
    LogsPage {},
}
