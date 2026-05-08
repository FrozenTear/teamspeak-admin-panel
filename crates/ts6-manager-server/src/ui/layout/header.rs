//! Top header. Slot order matches `components.md` §11.3 and the preview
//! HTML at `study-documents/design-system/preview/dashboard.html` line 139.
//!
//! Phase 1 ships:
//!  - hamburger button (mobile only, currently inert — drawer animation is Phase 2)
//!  - server-selector pill — functional Dropdown (PURA-27) backed by stub data
//!    until the live `GET /api/servers` route lands
//!  - websocket dot (stub status; wires to the real WS hub when it lands)
//!  - theme toggle (live — flips `data-theme="dark"`/`"light"`)
//!  - user menu (stub — initials avatar + display name; dropdown follows)
//!  - logout button (live)

use dioxus::prelude::*;

use crate::client::auth as auth_client;
use crate::client::dioxus::use_session;
use crate::client::session::SessionHandle;
use crate::client::store::AuthState;
use crate::ui::components::{Button, ButtonSize, ButtonVariant, ServerSelector, ServerSelectorVariant};
use crate::ui::routes::Route;
use crate::ui::theme::{Theme, use_theme};
use ts6_manager_shared::auth::LogoutRequest;

/// Header bar. Pulls user + theme from context — no props needed.
#[allow(non_snake_case)]
#[component]
pub fn Header() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let theme_ctx = use_theme();

    let mut logging_out = use_signal(|| false);

    let user = match &*session.state.read() {
        AuthState::Authenticated { user, .. } => user.clone(),
        // AppShell already redirected; render an empty fragment as
        // belt-and-braces so a stale render doesn't crash on missing user.
        AuthState::Anonymous => return rsx! { "" },
    };

    let logout_session = session.clone();
    let onlogout = move |_| {
        let session = logout_session.clone();
        logging_out.set(true);
        spawn(async move {
            // Best-effort server-side logout (spec §6.5.5: idempotent).
            if let Some(refresh) = session.read().refresh_token().map(str::to_owned) {
                let _ = auth_client::logout(
                    api_base().as_str(),
                    &LogoutRequest {
                        refresh_token: refresh,
                    },
                )
                .await;
            }
            session.replace(AuthState::Anonymous);
            logging_out.set(false);
            nav.replace(Route::LoginPage { next: None });
        });
    };

    let mut theme_signal = theme_ctx.theme;
    let current_theme = *theme_signal.read();
    let toggle_label = match current_theme {
        Theme::Dark => "Switch to light theme",
        Theme::Light => "Switch to dark theme",
    };
    let toggle_icon = match current_theme {
        Theme::Dark => "☾",
        Theme::Light => "☀",
    };
    let on_toggle_theme = move |_| {
        let next = match *theme_signal.read() {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        };
        *theme_signal.write() = next;
    };

    rsx! {
        header { class: "header", role: "banner",
            button {
                class: "btn btn-ghost btn-sm hamburger",
                r#type: "button",
                "aria-label": "Open navigation",
                "aria-disabled": "true",
                title: "Mobile drawer arrives in Phase 2",
                "☰"
            }

            ServerSelector { variant: ServerSelectorVariant::Desktop }

            span {
                class: "ws-dot",
                role: "status",
                "aria-label": "WebSocket status: connected",
                title: "WebSocket connected",
            }

            span { class: "spacer" }

            button {
                class: "btn btn-ghost btn-sm",
                r#type: "button",
                "aria-label": "{toggle_label}",
                title: "{toggle_label}",
                onclick: on_toggle_theme,
                "{toggle_icon}"
            }

            span { class: "user", role: "group", "aria-label": "Account",
                span { class: "avatar", "aria-hidden": "true", "{initials_for(&user.display_name, &user.username)}" }
                span { class: "name", "{user.display_name}" }
                span { class: "chev", "aria-hidden": "true", "▾" }
            }

            div { class: "logout-btn",
                Button {
                    variant: ButtonVariant::Secondary,
                    size: ButtonSize::Small,
                    loading: logging_out(),
                    onclick: onlogout,
                    "Logout"
                }
            }
        }
    }
}

/// Initials for the avatar pill.
///
/// Falls back to username's first letter, then `?` if everything's empty —
/// the Banner-vs-empty rule of "never render an empty styled chip."
fn initials_for(display: &str, username: &str) -> String {
    let from_display = display
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if !from_display.is_empty() {
        return from_display;
    }
    username
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// API base URL. Mirrors the helper in `ui::pages::login` — kept in this
/// module so the header doesn't reach into another route's private file.
fn api_base() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(origin) = window.location().origin() {
                return origin;
            }
        }
        String::new()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::client::dioxus::DioxusSession;
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use crate::ui::theme::ThemeContext;
    use ts6_manager_shared::auth::UserInfo;

    #[test]
    fn initials_use_first_letter_of_two_words() {
        assert_eq!(initials_for("Robert Soot", "rsoot"), "RS");
    }

    #[test]
    fn initials_clip_to_two_letters() {
        assert_eq!(initials_for("Alice Bob Charlie", "abc"), "AB");
    }

    #[test]
    fn initials_uppercase_lowercased_words() {
        assert_eq!(initials_for("alice", "alice"), "A");
    }

    #[test]
    fn initials_fall_back_to_username_when_display_empty() {
        assert_eq!(initials_for("", "rsoot"), "R");
    }

    #[test]
    fn initials_fall_back_to_question_mark_when_both_empty() {
        assert_eq!(initials_for("", ""), "?");
    }

    // ── Chrome-snapshot harness ──────────────────────────────────────────
    //
    // SSR-renders the Header inside a synthetic Router and provides the two
    // contexts the production component reaches for: a Dioxus session in
    // the `Authenticated` branch (so the component renders user-menu chrome
    // instead of bailing out empty) and a theme context (so `use_theme()`
    // doesn't panic). Production wiring lives in `provide_session` /
    // `ThemeProvider`; the harness short-cuts both with in-memory state
    // because we only inspect the rendered HTML.

    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum TestRoute {
        #[route("/")]
        HeaderHarness {},
    }

    #[component]
    fn HeaderHarness() -> Element {
        // Authenticated session — `Header` matches `AuthState::Anonymous`
        // to render an empty fragment, so the test must seed a real user.
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
            theme: Signal::new(crate::ui::theme::Theme::Dark),
        });
        rsx! { Header {} }
    }

    fn render_header_harness() -> String {
        let mut dom = VirtualDom::new(|| {
            rsx! { Router::<TestRoute> {} }
        });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn header_carries_banner_role() {
        let html = render_header_harness();
        assert!(
            html.contains(r#"role="banner""#),
            "header missing role='banner': {html}"
        );
    }

    #[test]
    fn theme_toggle_button_has_descriptive_aria_label() {
        let html = render_header_harness();
        // Default seeded theme is `Dark`, so the toggle invites flipping
        // to light. Pinning the label string keeps it from being silently
        // localised away into something a screen-reader user can't follow.
        assert!(
            html.contains(r#"aria-label="Switch to light theme""#),
            "theme toggle missing 'Switch to light theme' aria-label: {html}"
        );
    }

    #[test]
    fn ws_dot_has_status_role_for_assistive_tech() {
        let html = render_header_harness();
        assert!(
            html.contains(r#"role="status""#),
            "websocket indicator missing role='status': {html}"
        );
        assert!(
            html.contains(r#"aria-label="WebSocket status: connected""#),
            "websocket indicator missing aria-label describing connection state: {html}"
        );
    }
}
