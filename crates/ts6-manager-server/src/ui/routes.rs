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
    AdminUsersPage, BansPage, BotDetailPage, BotsIndexPage, ChannelsPage, ClientsPage,
    DashboardPlaceholder, Home, LoginPage, LogsPage, MusicLibraryPage, MusicPlaylistsPage,
    NotFoundPage, PublicWidgetPage, RadioStationsPage, ServerEditPage, ServerInfoPage,
    ServersIndexPage, SettingsPage, SetupPage, VideoSourcesPage, WidgetsPage,
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

    // PURA-221 — SSH credential editor for existing server connections.
    #[route("/server-edit")]
    ServerEditPage {},

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

    // PURA-218 — explicit `/servers` index inside AppShell. Must precede
    // the catch-all variant so `/servers` resolves to a real authed page
    // instead of falling through to `NotFoundPage`.
    #[route("/servers")]
    ServersIndexPage {},

    // PURA-224 — admin settings surface. First section is YouTube cookie
    // upload (PURA-223 backend); future sections live alongside it under
    // the same `/settings` route.
    #[route("/settings")]
    SettingsPage {},

    // PURA-237 — admin user management (list + create/edit modal + sessions
    // pane). The sidebar hides this entry for non-admin sessions and the
    // `/api/users` routes enforce `RequireAdmin` server-side; the page also
    // renders an "Insufficient permissions" guard so a forged URL lands on
    // a 403 surface rather than a doomed fetch loop.
    #[route("/admin/users")]
    AdminUsersPage {},

    // PURA-213 — catch-all NotFound. Lives outside `AppShell` so the page
    // renders for both authed and anon visitors without an auth bounce
    // (e.g. typo'd URLs shouldn't kick anon users to `/login?next=<bad>`).
    // MUST be the last variant — the macro orders matches by specificity
    // (Query → Static → Dynamic → CatchAll) so any explicit route added
    // below would still take precedence on its own path, but keeping the
    // catch-all at the bottom matches the convention used by the dioxus
    // router macro docs.
    #[end_layout]
    #[route("/:..segments")]
    NotFoundPage { segments: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::Route;
    use std::str::FromStr;

    /// PURA-213 — without a catch-all variant, visiting an unknown path
    /// threw a `Routable` parse error that the default dioxus-core error
    /// path rendered as raw text on the production image. This test pins
    /// the contract: every unmapped path must resolve to
    /// `Route::NotFoundPage` and never fall through to a parse error.
    #[test]
    fn unknown_paths_resolve_to_not_found() {
        for path in ["/totally-unknown", "/x/y/z", "/clients/bogus"] {
            let route =
                Route::from_str(path).unwrap_or_else(|err| panic!("failed to parse {path}: {err}"));
            assert!(
                matches!(route, Route::NotFoundPage { .. }),
                "expected NotFoundPage for {path}, got {route:?}",
            );
        }
    }

    /// Explicit routes must still win over the catch-all.
    #[test]
    fn known_paths_still_match_their_explicit_route() {
        let route = Route::from_str("/dashboard").expect("dashboard parse");
        assert!(matches!(route, Route::DashboardPlaceholder {}));

        let route = Route::from_str("/music-bots").expect("bots index parse");
        assert!(matches!(route, Route::BotsIndexPage {}));
    }

    /// PURA-218 — `/servers` resolves to the new authed index page, not
    /// the catch-all NotFound. Pins the regression introduced by PURA-213
    /// (where the dashboard CTA landed on the friendly 404).
    #[test]
    fn servers_path_resolves_to_servers_index_not_catch_all() {
        let route = Route::from_str("/servers").expect("/servers parse");
        assert!(
            matches!(route, Route::ServersIndexPage {}),
            "expected ServersIndexPage for /servers, got {route:?}",
        );
    }

    /// The captured segments reconstruct the attempted path so the
    /// `NotFoundPage` component can show the operator what they typed.
    #[test]
    fn not_found_captures_path_segments() {
        let route = Route::from_str("/x/y/z").expect("x/y/z parse");
        let Route::NotFoundPage { segments } = route else {
            panic!("expected NotFoundPage variant");
        };
        assert_eq!(
            segments,
            vec!["x".to_string(), "y".to_string(), "z".to_string()]
        );
    }
}
