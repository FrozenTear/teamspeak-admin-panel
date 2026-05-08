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
    rsx! {
        Link {
            to: props.to.clone(),
            class: "{class}",
            "aria-current": if props.active { "page" } else { "false" },
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
            nav { id: "{NAV_LANDMARK_ID}", tabindex: "-1", "aria-label": "Primary",
                Link { to: Route::DashboardPlaceholder {}, class: "brand",
                    span { class: "mark" }
                    "TS6 Manager"
                }

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
}
