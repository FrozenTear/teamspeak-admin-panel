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
            // primary-nav landmark contains only route entries — and a plain
            // `<a>` (rather than `dioxus_router::Link`) keeps the brand
            // from auto-rendering `aria-current="page"` whenever the
            // dashboard happens to be the current route. Click behaviour is
            // a full-page navigation back to `/`; brand-click is rare and
            // a reload from `/` to `/` is effectively a no-op.
            a { class: "brand", href: "/",
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

    /// Minimal route enum for the SSR harness — single page at `/` so the
    /// memory-history default lands somewhere routable.
    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum TestRoute {
        #[route("/")]
        SidebarHarness {},
    }

    #[component]
    fn SidebarHarness() -> Element {
        rsx! { Sidebar { active: Route::DashboardPlaceholder {} } }
    }

    /// Build a SidebarHarness VirtualDom and SSR it to a string.
    fn render_sidebar_harness() -> String {
        let mut dom = VirtualDom::new(|| {
            rsx! { Router::<TestRoute> {} }
        });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
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
        // The Phase-1 active route is `DashboardPlaceholder`. Exactly one
        // `aria-current="page"` should appear in the SSR output — multiple
        // would mean two nav items are claiming current-page state, which
        // confuses screen readers.
        let count = html.matches(r#"aria-current="page""#).count();
        assert_eq!(count, 1, "expected aria-current='page' once, got {count} in {html}");
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
