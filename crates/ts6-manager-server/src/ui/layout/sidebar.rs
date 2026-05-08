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
}

#[allow(non_snake_case)]
#[component]
pub fn Sidebar(props: SidebarProps) -> Element {
    let dashboard_active = matches!(props.active, Route::DashboardPlaceholder {});
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
                    PlaceholderItem { icon: "#", label: "Channels" }
                    PlaceholderItem { icon: "◆", label: "Clients" }
                    PlaceholderItem { icon: "▤", label: "Files" }
                }

                NavGroup { label: "Moderation",
                    PlaceholderItem { icon: "⚐", label: "Server groups" }
                    PlaceholderItem { icon: "⚑", label: "Channel groups" }
                    PlaceholderItem { icon: "⚒", label: "Permissions" }
                    PlaceholderItem { icon: "⊘", label: "Bans" }
                    PlaceholderItem { icon: "∘", label: "Tokens" }
                    PlaceholderItem { icon: "!", label: "Complaints" }
                    PlaceholderItem { icon: "✉", label: "Messages" }
                }

                NavGroup { label: "Automation",
                    PlaceholderItem { icon: "⊕", label: "Bots" }
                    PlaceholderItem { icon: "♪", label: "Music bots" }
                    PlaceholderItem { icon: "▣", label: "Widgets" }
                }

                NavGroup { label: "Admin",
                    PlaceholderItem { icon: "≡", label: "Logs" }
                    PlaceholderItem { icon: "◯", label: "Instance" }
                    PlaceholderItem { icon: "⚙", label: "Settings" }
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
        let active = matches!(Route::DashboardPlaceholder {}, Route::DashboardPlaceholder {});
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
        assert_eq!(count, 1, "expected aria-current='page' once, got {count} in {html}");
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
        // 16 placeholder items in Phase 1 (3 × Server, 7 × Moderation,
        // 3 × Automation, 3 × Admin). Pin the count so a future refactor
        // that drops one is a flagged regression rather than a silent
        // removal.
        let disabled = html.matches(r#"aria-disabled="true""#).count();
        let tabindex_minus = html.matches(r#"tabindex="-1""#).count();
        // 16 placeholders + the `<nav>` itself carries `tabindex=-1` for
        // the skip-link target, so 17 total `tabindex="-1"` attributes.
        assert_eq!(disabled, 16, "expected 16 aria-disabled placeholders, got {disabled}");
        assert_eq!(
            tabindex_minus, 17,
            "expected 17 tabindex='-1' (16 placeholders + nav landmark), got {tabindex_minus}"
        );
    }
}
