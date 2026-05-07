//! `/` route — placeholder dashboard.
//!
//! Real dashboard data depends on REST endpoints that aren't built yet
//! (separate PURA-5 child). For PURA-14 we just need a target for the
//! post-login redirect that signals "session active" so QA can verify the
//! login flow end-to-end.

use dioxus::prelude::*;

use crate::client::dioxus::use_session;
use crate::client::session::SessionHandle;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant, Button};
use crate::ui::routes::Route;

#[component]
pub fn DashboardPlaceholder() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let state = session.state.read().clone();

    // Anonymous users hitting `/` get bounced to /login with a `next` so
    // re-login lands them back on the dashboard. The router does this on
    // first render, before any flicker.
    use_effect({
        let state = state.clone();
        move || {
            if matches!(state, AuthState::Anonymous) {
                nav.replace(Route::LoginPage {
                    next: Some("/".into()),
                });
            }
        }
    });

    let user = match &state {
        AuthState::Authenticated { user, .. } => user.clone(),
        AuthState::Anonymous => return rsx! { "" },
    };

    let mut logging_out = use_signal(|| false);
    let logout_session = session.clone();
    let onclick = move |_| {
        let session = logout_session.clone();
        logging_out.set(true);
        spawn(async move {
            // Best-effort server-side logout — failure is fine because we
            // clear local state regardless. Pull the refresh token before
            // we wipe the session so the call has something to send.
            if let Some(refresh) = session.read().refresh_token().map(str::to_owned) {
                let _ = crate::client::auth::logout(
                    api_base().as_str(),
                    &ts6_manager_shared::auth::LogoutRequest {
                        refreshToken: refresh,
                    },
                )
                .await;
            }
            session.replace(AuthState::Anonymous);
            logging_out.set(false);
        });
    };

    rsx! {
        div { class: "app-root",
            Banner { variant: BannerVariant::Info, title: "Dashboard placeholder",
                "Live dashboard data lands in a follow-up PURA-5 child. This route exists "
                "today so the post-login redirect has a target."
            }
            section { class: "stack-md",
                h1 { "Welcome, {user.displayName}" }
                p { "Signed in as @{user.username} ({user.role})." }
                Button {
                    variant: crate::ui::components::ButtonVariant::Secondary,
                    loading: logging_out(),
                    onclick: onclick,
                    "Log out"
                }
            }
        }
    }
}

/// Resolve the API base URL. On WASM we read it from `window.location` so
/// the SPA targets the same origin it was served from. On native (SSR /
/// tests) we return an unused placeholder — the dashboard code only runs
/// after hydration in practice.
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
