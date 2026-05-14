//! Layout chrome for authenticated routes.
//!
//! Implements the spec §28.2 / `components.md` §11 contract: a fixed left
//! sidebar with grouped navigation and a top header that hosts the server
//! selector, websocket indicator, theme toggle, user menu, and logout.
//!
//! [`AppShell`] is wired into the `Routable` enum via `#[layout(AppShell)]`,
//! so every route nested under it renders inside
//! `.app > .sidebar + .header + .main` automatically. The login route stays
//! outside the layout because the spec puts it on its own surface.

mod header;
mod servers_context;
mod sidebar;

pub use header::Header;
pub use servers_context::{
    ServersContext, ServersData, mount_servers_context, use_servers_context,
};
pub use sidebar::{NAV_LANDMARK_ID, Sidebar};

use dioxus::prelude::*;

use crate::client::dioxus::use_session;
use crate::client::store::AuthState;
use crate::ui::components::{
    ActivityFeedSubscription, ServerSelector, ServerSelectorVariant, ToasterRegion,
    WsReconnectBanner,
};
use crate::ui::routes::Route;

/// Pull the current location path from the SPA URL. On the server (SSR)
/// the navigator's serialised representation is used directly. The
/// returned string is always a leading-slash path; `LoginPage::is_safe_internal_path`
/// gate-keeps it before honouring `?next=`.
fn current_path() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            let mut out = loc.pathname().unwrap_or_else(|_| "/".into());
            if let Ok(search) = loc.search() {
                if !search.is_empty() {
                    out.push_str(&search);
                }
            }
            return out;
        }
        "/".into()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "/".into()
    }
}

/// Authenticated layout. Dioxus mounts an `Outlet<Route>` for the matched
/// child route inside `<main class="main">`.
///
/// PURA-61 → PURA-63: `<main>` carries `tabindex="0"` so axe's
/// `scrollable-region-focusable` rule passes when route content overflows
/// the viewport (mobile especially). The rule requires the scrollable
/// region to be **keyboard-reachable**, not merely programmatically
/// focusable — `tabindex="-1"` (PURA-61's first pass) only let scripted
/// focus land here and still flagged. The trade-off is one extra Tab stop
/// in the global tab order, which is the WCAG-intended behavior so a
/// keyboard-only operator can land on the region and arrow-scroll. Also
/// serves as the target for the eventual "skip to main content" link.
///
/// Anonymous sessions are bounced to `/login?next=<path>`; the inline
/// rendered chrome is empty so there's no flash of authenticated UI before
/// the redirect lands.
#[allow(non_snake_case)]
#[component]
pub fn AppShell() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let route: Route = use_route();

    let is_authed = matches!(*session.state.read(), AuthState::Authenticated { .. });
    use_effect(move || {
        if !is_authed {
            nav.replace(Route::LoginPage {
                next: Some(current_path()),
            });
        }
    });

    // Single shared `/api/servers` resource for both selector variants and
    // (eventually) any chrome surface that wants the same list. Mounted
    // before the anon-session early return so the hook order is stable
    // across renders — for anon sessions the fetch errors out harmlessly
    // before the chrome bounces to /login.
    let _servers = mount_servers_context();

    if !is_authed {
        return rsx! { "" };
    }

    rsx! {
        div { class: "app",
            // Skip-to-nav link is the first focusable element. Visually
            // hidden until focused (via `.skip-link` in `layout.css`); on
            // activation, browser focus jumps to the sidebar `<nav>` so a
            // keyboard or screen-reader user doesn't have to tab through
            // the header chrome to reach the route table.
            a {
                class: "skip-link",
                href: "#{NAV_LANDMARK_ID}",
                "Skip to navigation"
            }
            Sidebar { active: route.clone() }
            Header {}
            div { class: "mobile-selector-bar",
                ServerSelector { variant: ServerSelectorVariant::Mobile }
            }
            main { class: "main", tabindex: "0",
                WsReconnectBanner {}
                Outlet::<Route> {}
            }
            // Subscribes to the selected server's `clients` topic and
            // pushes activity entries + toasts. Renders nothing visible.
            ActivityFeedSubscription {}
            // Top-right transient feedback (kicks, bans, dropped events).
            ToasterRegion {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dioxus::prelude::*;

    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use crate::ui::theme::{Theme, ThemeContext};
    use ts6_manager_shared::auth::UserInfo;

    use super::Route;

    /// Wraps the production `Route::DashboardPlaceholder` inside an
    /// authenticated session + a Dark theme so `AppShell` renders the full
    /// chrome instead of bouncing to login. Pure SSR — no async, no async
    /// hooks, no browser-only fallbacks.
    #[component]
    fn AppShellHarness() -> Element {
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "rsoot".into(),
                    display_name: "Robert Soot".into(),
                    role: "admin".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
        });
        use_context_provider(|| ThemeContext {
            theme: Signal::new(Theme::Dark),
        });
        // PURA-31 — every authed page descends from the gate provider in
        // production. SSR rendering never fires a fetch through it, but the
        // dashboard route reads it from context, so the harness must mount
        // one to avoid a "missing context" panic during chrome-snapshot tests.
        use_context_provider(|| provide_auth_gate(session));
        rsx! { Router::<Route> {} }
    }

    fn render_app_shell() -> String {
        let mut dom = VirtualDom::new(AppShellHarness);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// `components.md` §11.4 acceptance: the skip-to-navigation link is the
    /// chrome's first focusable anchor and points at the sidebar's `<nav>`.
    /// We assert ordering (skip-link emitted before the nav landmark) so a
    /// future refactor that moves the link below the chrome is a flagged
    /// regression.
    #[test]
    fn skip_link_precedes_primary_nav_landmark_in_app_shell() {
        let html = render_app_shell();
        let skip = html
            .find(r#"class="skip-link""#)
            .expect("skip-link not rendered in AppShell");
        let nav_target = html
            .find(r#"id="primary-nav""#)
            .expect("primary-nav target not rendered in Sidebar");
        assert!(
            skip < nav_target,
            "skip-link must appear before its target nav (skip @ {skip}, nav @ {nav_target}): {html}"
        );
        assert!(
            html.contains(r##"href="#primary-nav""##),
            "skip-link href must point at #primary-nav: {html}"
        );
    }

    #[test]
    fn app_shell_includes_both_chrome_landmarks() {
        let html = render_app_shell();
        // Belt-and-braces against an AppShell refactor that hides the
        // sidebar or header — the chrome contract is "always both" for the
        // authenticated layout.
        assert!(
            html.contains(r#"<aside class="sidebar""#),
            "missing sidebar landmark: {html}"
        );
        assert!(
            html.contains(r#"role="banner""#),
            "missing header banner role: {html}"
        );
    }

    /// PURA-61 → PURA-63: `<main>` must carry `tabindex="0"` so axe's
    /// `scrollable-region-focusable` rule passes when route content
    /// overflows the viewport. PURA-61 used `tabindex="-1"` which the
    /// rule still flagged because it is only programmatically focusable;
    /// the rule wants keyboard reachability. A future refactor that drops
    /// or weakens this attribute would silently regress the dashboard
    /// a11y contract; this test pins it.
    #[test]
    fn app_shell_main_landmark_is_keyboard_focusable() {
        let html = render_app_shell();
        assert!(
            html.contains(r#"<main class="main" tabindex="0""#)
                || html.contains(r#"<main tabindex="0" class="main""#),
            "missing tabindex='0' on <main>: {html}"
        );
    }
}
