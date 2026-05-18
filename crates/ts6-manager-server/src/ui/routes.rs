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
use crate::ui::pages::DevFlowCanvasPage;
#[cfg(debug_assertions)]
use crate::ui::pages::DevVideoPlayerPage;
use crate::ui::pages::{
    AdminUsersPage, AuditPage, AutomodMetricsPage, BansPage, BotDetailPage, BotsIndexPage,
    ChannelsPage, ClientsPage, DashboardPlaceholder, FlowDetailPage, FlowEditPage, FlowFormPage,
    FlowsListPage, Home, LoginPage, LogsPage, ModerationCasePage, ModerationQueuePage,
    MusicLibraryPage, MusicPlaylistsPage, NotFoundPage, PermissionGrantsPage, PublicWidgetPage,
    RadioStationsPage, ServerEditPage, ServerInfoPage, ServersIndexPage, SettingsPage, SetupPage,
    SubjectHistoryPage, TokensPage, VideoSourcesPage, WidgetsPage,
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

    // PURA-267 — dev-only mount for the v2 visual flow-canvas builder.
    // Lives outside `AppShell` and `cfg(debug_assertions)`-gated, exactly
    // like the canvas-tech spike route: release bundles (`dx serve
    // --release`) do not expose it. The production swap of `/flows/new`,
    // `/flows/{id}/edit`, and the Definition tab lands once the v2 HTTP
    // surface (PURA-266) can be wired for save / validate / run-overlay.
    #[cfg(debug_assertions)]
    #[route("/dev/flow-canvas")]
    DevFlowCanvasPage {},

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

    // PURA-243 — flow engine pages. The /flows/new route must precede
    // /flows/:flow_id so `new` resolves to the create form instead of
    // tripping the dynamic-segment parser.
    #[route("/flows")]
    FlowsListPage {},

    #[route("/flows/new")]
    FlowFormPage {},

    #[route("/flows/:flow_id/edit")]
    FlowEditPage { flow_id: i64 },

    #[route("/flows/:flow_id")]
    FlowDetailPage { flow_id: i64 },

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

    // PURA-238 — v1.1 admin audit-log viewer. Admin-gated: the sidebar
    // entry is hidden for non-admins and `AuditPage` renders an in-page
    // 403 surface if a non-admin reaches the route directly. The route
    // itself stays mountable for everyone so the gate is a clean 403
    // page rather than a NotFound fall-through.
    #[route("/admin/audit")]
    AuditPage {},

    // PURA-287 Phase 9.0-ui — moderation surfaces. `/moderation` is the
    // operator queue (cases + complaints); the case-detail and per-subject
    // history routes nest under it so their URLs stay shareable. All three
    // are role-gated to admin + moderator in-page; `/api/moderation/*`
    // re-checks the `moderation.*` catalog server-side.
    #[route("/moderation")]
    ModerationQueuePage {},

    #[route("/moderation/cases/:case_id")]
    ModerationCasePage { case_id: i64 },

    #[route("/moderation/subjects/:uid")]
    SubjectHistoryPage { uid: String },

    // PURA-303 Phase 9.1.4 — per-rule automod metrics. Static segment, so
    // it never collides with the `/moderation/cases|subjects/*` dynamic
    // routes. Same admin + moderator in-page gate.
    #[route("/moderation/automod")]
    AutomodMetricsPage {},

    // PURA-376 — privilege keys (tokens). Static segment, so it never
    // collides with the `/moderation/cases|subjects/*` dynamic routes.
    // Read = any operator with server access; write (mint / delete) =
    // admin, enforced server-side by the `/tokens` route's `check_admin`.
    #[route("/moderation/tokens")]
    TokensPage {},

    // PURA-287 — per-user moderation grant editor. Admin-gated like the
    // other `/admin/*` surfaces; the sidebar entry is hidden for non-admins
    // and `PUT /api/users/{id}/permissions` enforces `RequireAdmin`.
    #[route("/admin/permissions")]
    PermissionGrantsPage {},

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

    /// PURA-243 — the static `/flows/new` route must win over the dynamic
    /// `/flows/:flow_id` route. Without the explicit ordering in the enum,
    /// `/flows/new` would parse `new` as a `flow_id` and fail (or, if the
    /// id were a string, silently land on the wrong page).
    #[test]
    fn flows_static_routes_beat_dynamic_segment() {
        assert!(matches!(
            Route::from_str("/flows").expect("/flows parse"),
            Route::FlowsListPage {}
        ));
        assert!(matches!(
            Route::from_str("/flows/new").expect("/flows/new parse"),
            Route::FlowFormPage {}
        ));
        assert!(matches!(
            Route::from_str("/flows/42").expect("/flows/42 parse"),
            Route::FlowDetailPage { flow_id: 42 }
        ));
        assert!(matches!(
            Route::from_str("/flows/42/edit").expect("/flows/42/edit parse"),
            Route::FlowEditPage { flow_id: 42 }
        ));
    }

    /// PURA-287 — the moderation routes. `/moderation` is static and must
    /// win over the dynamic case/subject sub-routes; the dynamic segments
    /// must parse their typed params (`case_id: i64`, `uid: String`).
    #[test]
    fn moderation_routes_resolve_with_typed_params() {
        assert!(matches!(
            Route::from_str("/moderation").expect("/moderation parse"),
            Route::ModerationQueuePage {}
        ));
        assert!(matches!(
            Route::from_str("/moderation/cases/7").expect("case parse"),
            Route::ModerationCasePage { case_id: 7 }
        ));
        assert!(matches!(
            Route::from_str("/moderation/subjects/subject-uid-1").expect("subject parse"),
            Route::SubjectHistoryPage { uid } if uid == "subject-uid-1"
        ));
        // PURA-303 — the two-segment automod metrics route.
        assert!(matches!(
            Route::from_str("/moderation/automod").expect("automod parse"),
            Route::AutomodMetricsPage {}
        ));
        // PURA-376 — the static tokens route wins over the dynamic
        // `/moderation/cases|subjects/*` matchers.
        assert!(matches!(
            Route::from_str("/moderation/tokens").expect("tokens parse"),
            Route::TokensPage {}
        ));
        assert!(matches!(
            Route::from_str("/admin/permissions").expect("permissions parse"),
            Route::PermissionGrantsPage {}
        ));
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
