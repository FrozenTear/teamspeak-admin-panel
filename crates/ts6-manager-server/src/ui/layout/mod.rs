//! Layout chrome for authenticated routes.
//!
//! Implements the spec §28.2 / `components.md` §11 contract: a fixed left
//! sidebar with grouped navigation and a top header that hosts the server
//! selector, websocket indicator, theme toggle, user menu, and logout.
//!
//! [`AppShell`] is wired into the `Routable` enum via `#[layout(AppShell)]`,
//! so every route nested under it renders inside `.app > .sidebar + .header
//! + .main` automatically. The login route stays outside the layout because
//! the spec puts it on its own surface.

mod header;
mod sidebar;

pub use header::Header;
pub use sidebar::{NAV_LANDMARK_ID, Sidebar};

use dioxus::prelude::*;

use crate::client::dioxus::use_session;
use crate::client::store::AuthState;
use crate::ui::routes::Route;

/// Authenticated layout. Dioxus mounts an `Outlet<Route>` for the matched
/// child route inside `<main class="main">`.
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
                button {
                    class: "selector",
                    r#type: "button",
                    "aria-haspopup": "menu",
                    "aria-disabled": "true",
                    "title": "Server selector — interactive in a follow-up child",
                    span { class: "mark", "⬢" }
                    span { class: "name", "No server selected" }
                    span { class: "chev", "▾" }
                }
            }
            main { class: "main",
                Outlet::<Route> {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dioxus::prelude::*;

    use crate::client::dioxus::DioxusSession;
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
        use_context_provider(|| DioxusSession {
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
        assert!(html.contains(r#"<aside class="sidebar""#), "missing sidebar landmark: {html}");
        assert!(html.contains(r#"role="banner""#), "missing header banner role: {html}");
    }
}

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
