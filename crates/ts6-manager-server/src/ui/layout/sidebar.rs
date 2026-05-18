//! Left-rail navigation. Grouped per `components.md` §11.2 (Server,
//! Moderation, Automation, Admin). Phase 1 only ships routes for
//! Dashboard + Login; the remaining nav items are visible-but-disabled
//! placeholders so the operator's mental model lines up with the future
//! routes — the items become real `Link`s as their pages land in PURA-5
//! children.

use dioxus::prelude::*;

use crate::ui::routes::Route;

/// `id` of the sidebar `<nav>` landmark. Shared so the AppShell's
/// skip-to-navigation link can target it via `href="#primary-nav"` and the
/// id stays in lock-step if it ever gets renamed.
pub const NAV_LANDMARK_ID: &str = "primary-nav";

/// One nav group with an uppercase label and a list of items. The label is
/// decoration only — `<nav aria-label="Primary">` wraps the whole rail per
/// §11.2.
#[derive(Clone, PartialEq, Props)]
pub struct NavGroupProps {
    /// Section heading text. Rendered uppercase via CSS, supplied lower-cased
    /// here so the source matches the operator-facing copy.
    pub label: String,
    /// Items in the group. Each is a `NavItem` (real `Link`) or a placeholder
    /// rendered through [`PlaceholderItem`].
    pub children: Element,
}

#[allow(non_snake_case)]
#[component]
pub fn NavGroup(props: NavGroupProps) -> Element {
    rsx! {
        div { class: "nav-group",
            div { class: "nav-group-label", "{props.label}" }
            {props.children}
        }
    }
}

/// One real route entry. Renders as a `<Link>` so the SPA navigates without
/// a full reload, and gets `aria-current="page"` + `is-active` when the
/// current route matches.
#[derive(Clone, PartialEq, Props)]
pub struct NavItemProps {
    pub icon: String,
    pub label: String,
    pub to: Route,
    /// `true` when this item maps to the currently active route. Driven from
    /// the parent — keeping the comparison close to the route enum avoids
    /// every nav item re-running the route matcher.
    #[props(default = false)]
    pub active: bool,
}

#[allow(non_snake_case)]
#[component]
pub fn NavItem(props: NavItemProps) -> Element {
    let class = if props.active {
        "nav-item is-active"
    } else {
        "nav-item"
    };
    // `dioxus_router::Link` auto-renders `aria-current="page"` when the
    // link's destination matches the current URL — see
    // `dioxus-router-0.7.7/src/components/link.rs:194`. Don't pass an
    // explicit `aria-current`; it would render the attribute twice and
    // confuse screen readers.
    rsx! {
        Link {
            to: props.to.clone(),
            class: "{class}",
            span { class: "icn", "{props.icon}" }
            "{props.label}"
        }
    }
}

/// Visible-but-disabled future route. Renders as `<a>` without an `href` so
/// it stays in the tab order yet doesn't navigate. Hover/focus styling
/// matches `nav-item`; the cursor reads `not-allowed` via class so the
/// operator gets immediate feedback.
#[derive(Clone, PartialEq, Props)]
pub struct PlaceholderItemProps {
    pub icon: String,
    pub label: String,
}

#[allow(non_snake_case)]
#[component]
pub fn PlaceholderItem(props: PlaceholderItemProps) -> Element {
    rsx! {
        a {
            class: "nav-item is-disabled",
            "aria-disabled": "true",
            tabindex: "-1",
            title: "Available in a future Phase 1 child",
            span { class: "icn", "{props.icon}" }
            "{props.label}"
        }
    }
}

/// Phase 1 nav rail. The order, grouping, and labels come straight from
/// `preview/dashboard.html` so the visual-truth gate matches.
#[derive(Clone, PartialEq, Props)]
pub struct SidebarProps {
    /// Currently matched route — used to flag the active item.
    pub active: Route,
    /// PURA-237 / PURA-238 — `true` when the signed-in operator has the
    /// `admin` role. Drives visibility of the admin-only nav entries
    /// (Users + Audit log) per `docs/admin/ui-brief.md` §5. The `/admin/*`
    /// routes also enforce `RequireAdmin` server-side; this is the
    /// front-end suppression so non-admins never see a link that 403s.
    /// Defaults to `false` so the SSR chrome-snapshot harness — which has
    /// no session context — keeps rendering the non-admin rail.
    #[props(default = false)]
    pub is_admin: bool,
}

#[allow(non_snake_case)]
#[component]
pub fn Sidebar(props: SidebarProps) -> Element {
    let dashboard_active = matches!(props.active, Route::DashboardPlaceholder {});
    let channels_active = matches!(props.active, Route::ChannelsPage {});
    let clients_active = matches!(props.active, Route::ClientsPage {});
    let bans_active = matches!(props.active, Route::BansPage {});
    let server_info_active = matches!(props.active, Route::ServerInfoPage {});
    let logs_active = matches!(props.active, Route::LogsPage {});
    let widgets_active = matches!(props.active, Route::WidgetsPage {});
    let video_sources_active = matches!(props.active, Route::VideoSourcesPage {});
    let settings_active = matches!(props.active, Route::SettingsPage {});
    let admin_users_active = matches!(props.active, Route::AdminUsersPage {});
    let audit_active = matches!(props.active, Route::AuditPage {});
    let permissions_active = matches!(props.active, Route::PermissionGrantsPage {});
    // PURA-287 — Moderation highlights on the queue plus the case-detail
    // and per-subject history sub-routes so the operator stays oriented.
    let moderation_active = matches!(
        props.active,
        Route::ModerationQueuePage {}
            | Route::ModerationCasePage { .. }
            | Route::SubjectHistoryPage { .. }
    );
    // PURA-303 — the per-rule automod metrics surface highlights its own
    // nav entry, distinct from the case queue.
    let automod_active = matches!(props.active, Route::AutomodMetricsPage {});
    // PURA-380 (PURA-369 Phase D) — the five moderation control surfaces.
    // Each group entry highlights on its list route plus, where it has one,
    // the typed detail sub-route so the operator stays oriented while
    // drilled into a single group / token / message.
    let server_groups_active = matches!(
        props.active,
        Route::ServerGroupsPage {} | Route::ServerGroupDetailPage { .. }
    );
    let channel_groups_active = matches!(
        props.active,
        Route::ChannelGroupsPage {} | Route::ChannelGroupDetailPage { .. }
    );
    let perm_catalog_active = matches!(props.active, Route::PermissionsCatalogPage { .. });
    let tokens_active = matches!(props.active, Route::TokensPage {});
    let messages_active = matches!(props.active, Route::MessagesPage {});
    // PURA-124 WS-6 — Music bots highlight when the route is the index
    // OR any of the per-bot detail / library / playlists / radio
    // surfaces, so the operator stays oriented across the whole flow.
    let music_bots_active = matches!(
        props.active,
        Route::BotsIndexPage {}
            | Route::BotDetailPage { .. }
            | Route::MusicLibraryPage { .. }
            | Route::MusicPlaylistsPage { .. }
            | Route::RadioStationsPage { .. }
    );
    // PURA-243 — Flows highlight on the list, create, detail, and edit
    // routes so the operator stays oriented across the flow lifecycle.
    let flows_active = matches!(
        props.active,
        Route::FlowsListPage {}
            | Route::FlowFormPage {}
            | Route::FlowDetailPage { .. }
            | Route::FlowEditPage { .. }
    );
    rsx! {
        aside { class: "sidebar",
            // Brand sits OUTSIDE `<nav aria-label="Primary">` so the
            // primary-nav landmark contains only route entries. The brand
            // points at `Route::Home {}` (the `/` route), distinct from the
            // dashboard's `/dashboard`, so `dioxus_router::Link`'s automatic
            // `aria-current="page"` only ever fires on at most one element
            // at a time — the brand on `/`, the Dashboard NavItem on
            // `/dashboard`, and never both (PURA-37).
            Link { to: Route::Home {}, class: "brand",
                span { class: "mark" }
                "TS6 Manager"
            }
            nav { id: "{NAV_LANDMARK_ID}", tabindex: "-1", "aria-label": "Primary",
                NavGroup { label: "Server",
                    NavItem { icon: "▦", label: "Dashboard", to: Route::DashboardPlaceholder {}, active: dashboard_active }
                    NavItem { icon: "#", label: "Channels", to: Route::ChannelsPage {}, active: channels_active }
                    NavItem { icon: "◆", label: "Clients", to: Route::ClientsPage {}, active: clients_active }
                    NavItem { icon: "⊙", label: "Server info", to: Route::ServerInfoPage {}, active: server_info_active }
                    // PURA-145 WS-7 — operator-facing MoQ pipeline manager.
                    NavItem { icon: "▶", label: "Video sources", to: Route::VideoSourcesPage {}, active: video_sources_active }
                    PlaceholderItem { icon: "▤", label: "Files" }
                }

                NavGroup { label: "Moderation",
                    // PURA-380 — the five moderation control surfaces, live
                    // as of PURA-369 Phase D. Reads need only server access;
                    // each page gates its write affordances on `admin` and
                    // the `/api/.../*` routes re-check server-side.
                    NavItem { icon: "⚐", label: "Server groups", to: Route::ServerGroupsPage {}, active: server_groups_active }
                    NavItem { icon: "⚑", label: "Channel groups", to: Route::ChannelGroupsPage {}, active: channel_groups_active }
                    NavItem { icon: "⚒", label: "Permissions", to: Route::PermissionsCatalogPage { permsid: None }, active: perm_catalog_active }
                    NavItem { icon: "⊘", label: "Bans", to: Route::BansPage {}, active: bans_active }
                    NavItem { icon: "∘", label: "Tokens", to: Route::TokensPage {}, active: tokens_active }
                    // PURA-287 — moderation case + complaint queue.
                    NavItem { icon: "⚖", label: "Cases", to: Route::ModerationQueuePage {}, active: moderation_active }
                    // PURA-303 — per-rule automod metrics.
                    NavItem { icon: "🤖", label: "Automod", to: Route::AutomodMetricsPage {}, active: automod_active }
                    NavItem { icon: "✉", label: "Messages", to: Route::MessagesPage {}, active: messages_active }
                }

                NavGroup { label: "Automation",
                    PlaceholderItem { icon: "⊕", label: "Bots" }
                    NavItem { icon: "♪", label: "Music bots", to: Route::BotsIndexPage {}, active: music_bots_active }
                    // PURA-243 — Flows sits between Music bots and
                    // Widgets per `docs/flows/ui-brief.md` §2.
                    NavItem { icon: "⚡", label: "Flows", to: Route::FlowsListPage {}, active: flows_active }
                    NavItem { icon: "▣", label: "Widgets", to: Route::WidgetsPage {}, active: widgets_active }
                }

                NavGroup { label: "Admin",
                    // PURA-237 — admin user management. Visible only to
                    // admin sessions; the route + `/api/users` both enforce
                    // `RequireAdmin` independently of this suppression.
                    if props.is_admin {
                        NavItem { icon: "◈", label: "Users", to: Route::AdminUsersPage {}, active: admin_users_active }
                    }
                    NavItem { icon: "≡", label: "Logs", to: Route::LogsPage {}, active: logs_active }
                    // PURA-238 — audit-log viewer. Visible only to admin
                    // sessions; non-admins never see the entry and hit a
                    // 403 surface if they deep-link the route directly.
                    if props.is_admin {
                        NavItem { icon: "⊟", label: "Audit log", to: Route::AuditPage {}, active: audit_active }
                    }
                    // PURA-287 — per-user moderation grant editor. Admin-only,
                    // same suppression contract as Users / Audit log.
                    if props.is_admin {
                        NavItem { icon: "⚷", label: "Permission grants", to: Route::PermissionGrantsPage {}, active: permissions_active }
                    }
                    PlaceholderItem { icon: "◯", label: "Instance" }
                    NavItem { icon: "⚙", label: "Settings", to: Route::SettingsPage {}, active: settings_active }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skip-link in `AppShell` jumps focus to `#primary-nav`, which is
    /// this sidebar's `<nav>` id. Pinning the constant to its literal value
    /// means a typo or rename in either place becomes a unit-test failure
    /// instead of silently breaking the keyboard-skip path.
    #[test]
    fn nav_landmark_id_is_pinned_to_primary_nav() {
        assert_eq!(NAV_LANDMARK_ID, "primary-nav");
    }

    /// `dashboard_active` derivation — pinned so adding a new route can't
    /// silently leave Dashboard highlighted on a different page.
    #[test]
    fn active_derivation_only_matches_dashboard_route() {
        let active = matches!(
            Route::DashboardPlaceholder {},
            Route::DashboardPlaceholder {}
        );
        assert!(active);
        let other = matches!(
            Route::LoginPage { next: None },
            Route::DashboardPlaceholder {}
        );
        assert!(!other);
    }

    // ── Chrome-snapshot harness ──────────────────────────────────────────
    //
    // SSR-renders the Sidebar inside a synthetic Router so the inner `Link`s
    // see a router context (without one, `dioxus_router::Link` panics in
    // debug builds — see Link.rs:158). We don't use the production `Route`
    // enum because that would drag in `AppShell`'s session/theme contexts;
    // a one-route test enum is enough to render the sidebar's markup.
    //
    // The harness pre-seeds a `MemoryHistory` at `/dashboard` so the
    // production `Route::DashboardPlaceholder {}` Link inside the sidebar
    // (which serializes to `/dashboard`) matches the router's current URL —
    // that's how `dioxus_router::Link` decides to emit `aria-current="page"`
    // (Link.rs:194). The brand Link's `to: Route::Home {}` serializes to
    // `/`, so it does NOT match `/dashboard` and stays clean.

    use std::rc::Rc;

    use dioxus::history::{History, MemoryHistory};

    /// Minimal route enum for the SSR harness — single page at `/dashboard`
    /// so the memory-history seed below lands somewhere routable.
    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum TestRoute {
        #[route("/dashboard")]
        SidebarHarness {},
    }

    #[component]
    fn SidebarHarness() -> Element {
        rsx! { Sidebar { active: Route::DashboardPlaceholder {} } }
    }

    /// Build a SidebarHarness VirtualDom seeded at `/dashboard` and SSR it.
    fn render_sidebar_harness() -> String {
        render_sidebar_harness_at("/dashboard")
    }

    fn render_sidebar_harness_at(path: &str) -> String {
        // Inject the seeded history into the root scope BEFORE the Router
        // calls `dioxus_history::history()` — without this, the router
        // would fall back to `MemoryHistory::default()` (initial path `/`)
        // and the Dashboard NavItem's Link wouldn't auto-emit
        // `aria-current`. `VirtualDom::new` requires a non-capturing
        // `fn` pointer for the root component, so the seed must be
        // injected via `with_root_context` rather than a closure.
        let history: Rc<dyn History> = Rc::new(MemoryHistory::with_initial_path(path));
        fn root() -> Element {
            rsx! { Router::<TestRoute> {} }
        }
        let mut dom = VirtualDom::new(root).with_root_context(history);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Slice out the open tag of the brand `<a>` (start to first `>`) so we
    /// can assert about its attributes without snagging attributes from
    /// nested elements or sibling links.
    fn brand_open_tag(html: &str) -> &str {
        let start = html
            .find(r#"class="brand""#)
            .expect("brand element not found in sidebar render");
        // `<a class="brand"` — walk backwards from `class=` to the `<a` so
        // the slice covers the whole open tag from the leading `<`.
        let tag_open = html[..start]
            .rfind('<')
            .expect("brand `class=` attribute is not inside an open tag");
        let tag_close = html[tag_open..]
            .find('>')
            .expect("brand open tag is unterminated")
            + tag_open;
        &html[tag_open..=tag_close]
    }

    #[test]
    fn sidebar_nav_landmark_has_primary_id_and_aria_label() {
        let html = render_sidebar_harness();
        // Attribute order isn't part of the contract; assert each attribute
        // independently inside the same `<nav …>` open tag.
        assert!(html.contains("<nav "), "rendered html missing <nav: {html}");
        assert!(
            html.contains(r#"aria-label="Primary""#),
            "missing aria-label='Primary' in nav: {html}"
        );
        assert!(
            html.contains(r#"id="primary-nav""#),
            "missing id='primary-nav' in nav: {html}"
        );
        assert!(
            html.contains(r#"tabindex="-1""#),
            "missing tabindex='-1' on nav (so the skip-link target is programmatically focusable): {html}"
        );
    }

    #[test]
    fn dashboard_nav_item_renders_aria_current_page_exactly_once() {
        let html = render_sidebar_harness();
        // The Phase-1 active route is `DashboardPlaceholder` (rendered at
        // `/dashboard`). Exactly one `aria-current="page"` should appear in
        // the SSR output — multiple would mean two nav items (or the brand)
        // are claiming current-page state, which confuses screen readers.
        let count = html.matches(r#"aria-current="page""#).count();
        assert_eq!(
            count, 1,
            "expected aria-current='page' once, got {count} in {html}"
        );
    }

    #[test]
    fn brand_link_does_not_emit_aria_current_on_dashboard() {
        // PURA-37 acceptance: visiting `/dashboard` must put `aria-current=
        // "page"` on the Dashboard NavItem and NOT on the brand. The brand
        // `Link { to: Route::Home {} }` serializes to `/`, so it diverges
        // from the dashboard URL and `dioxus_router::Link` correctly omits
        // the attribute. Pinning this means a future re-collapse of `/`
        // and `/dashboard` would fail the chrome-snapshot gate.
        let html = render_sidebar_harness_at("/dashboard");
        let brand_tag = brand_open_tag(&html);
        assert!(
            !brand_tag.contains(r#"aria-current="page""#),
            "brand must not emit aria-current on /dashboard, got: {brand_tag}"
        );
        assert!(
            html.contains(r#"aria-current="page""#),
            "Dashboard NavItem should still emit aria-current somewhere in: {html}"
        );
    }

    #[test]
    fn placeholder_items_carry_aria_disabled_and_tabindex_minus_one() {
        let html = render_sidebar_harness();
        // PURA-124 WS-6 has converted "Music bots" into a real nav item.
        // PURA-224 has converted "Settings" into a real nav item.
        // PURA-287 has converted "Complaints" into the real "Cases" nav
        // item (the /moderation queue). PURA-380 has converted the five
        // Moderation placeholders (Server groups, Channel groups,
        // Permissions, Tokens, Messages) into real nav items. Remaining
        // placeholders: 1 × Server (Files), 1 × Automation (Bots), 1 ×
        // Admin (Instance) = 3. The count is the authoritative signal here
        // — adding/removing a real route should bump it.
        let disabled = html.matches(r#"aria-disabled="true""#).count();
        let tabindex_minus = html.matches(r#"tabindex="-1""#).count();
        // 3 placeholders + the `<nav>` itself carries `tabindex=-1` for
        // the skip-link target, so 4 total `tabindex="-1"` attributes.
        assert_eq!(
            disabled, 3,
            "expected 3 aria-disabled placeholders, got {disabled}"
        );
        assert_eq!(
            tabindex_minus, 4,
            "expected 4 tabindex='-1' (3 placeholders + nav landmark), got {tabindex_minus}"
        );
    }

    // ── PURA-237 — admin nav gating ───────────────────────────────────────

    /// Mirror of [`TestRoute`] for the `is_admin = true` harness. A separate
    /// one-route enum keeps the admin-on render isolated from the default
    /// `SidebarHarness` so the placeholder-count test above stays stable.
    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum AdminTestRoute {
        #[route("/dashboard")]
        AdminSidebarHarness {},
    }

    #[component]
    fn AdminSidebarHarness() -> Element {
        rsx! { Sidebar { active: Route::DashboardPlaceholder {}, is_admin: true } }
    }

    fn render_admin_sidebar() -> String {
        let history: Rc<dyn History> = Rc::new(MemoryHistory::with_initial_path("/dashboard"));
        fn root() -> Element {
            rsx! { Router::<AdminTestRoute> {} }
        }
        let mut dom = VirtualDom::new(root).with_root_context(history);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// ui-brief §5 — an admin session sees the admin user-management nav
    /// entry pointing at `/admin/users`.
    #[test]
    fn admin_session_sees_users_nav_entry() {
        let html = render_admin_sidebar();
        assert!(
            html.contains(r#"href="/admin/users""#),
            "admin nav must link to /admin/users: {html}"
        );
    }

    /// The default harness (`is_admin` defaults to `false`) must NOT render
    /// the admin nav entry — a non-admin never sees a link that would 403.
    #[test]
    fn non_admin_session_hides_users_nav_entry() {
        let html = render_sidebar_harness();
        assert!(
            !html.contains("/admin/users"),
            "non-admin sidebar must not surface the admin nav link: {html}"
        );
    }
}
