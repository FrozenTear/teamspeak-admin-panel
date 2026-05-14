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
#[cfg(debug_assertions)]
use crate::ui::pages::DevVideoPlayerPage;
use crate::ui::pages::{
    BansPage, BotDetailPage, BotsIndexPage, ChannelsPage, ClientsPage, DashboardPlaceholder, Home,
    LoginPage, LogsPage, MusicLibraryPage, MusicPlaylistsPage, PublicWidgetPage, RadioStationsPage,
    ServerInfoPage, SetupPage, VideoSourcesPage, WidgetsPage,
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

    // PURA-143 WS-5 — dev-only mount for the moq-lite video player. Lives
    // outside `AppShell` so the operator can two-tab the smoke without an
    // auth bounce. Gated by `cfg(debug_assertions)` so `dx serve --release`
    // bundles do not expose it.
    #[cfg(debug_assertions)]
    #[route("/dev/video-player?:relay&:ns")]
    DevVideoPlayerPage { relay: Option<String>, ns: Option<String> },

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

    // PURA-92 — Slice G operator-facing Widget Manager (Chapter 34).
    #[route("/widgets")]
    WidgetsPage {},

    // PURA-145 WS-7 — operator video-source surface (MoQ sidecar pipeline
    // management). Lives at the bare `/video-sources` prefix; per-server
    // scoping comes from the global server selector, same as Clients/
    // Channels/Bans.
    #[route("/video-sources")]
    VideoSourcesPage {},

    // PURA-124 WS-6 — music-bots product. Per-bot resources nest under
    // the bot id so the URLs stay shareable; the index lives at the
    // bare /music-bots prefix.
    #[route("/music-bots")]
    BotsIndexPage {},

    #[route("/music-bots/:bot_id")]
    BotDetailPage { bot_id: u64 },

    #[route("/music-bots/:bot_id/library")]
    MusicLibraryPage { bot_id: u64 },

    #[route("/music-bots/:bot_id/playlists")]
    MusicPlaylistsPage { bot_id: u64 },

    #[route("/music-bots/:bot_id/radio")]
    RadioStationsPage { bot_id: u64 },
}
