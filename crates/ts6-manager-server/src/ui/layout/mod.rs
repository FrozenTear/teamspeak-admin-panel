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
pub use sidebar::Sidebar;

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
