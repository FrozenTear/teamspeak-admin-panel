//! `/login` route — operator authentication.
//!
//! Spec §28: a single form (`username`, `password`, submit) with the
//! hardened error copy from `auth_error_strings::*`. The form is built
//! from design-system primitives only — no inline styles, no copy
//! duplicated from the constants module.
//!
//! Flow:
//! 1. User types credentials, hits Submit.
//! 2. `crate::client::auth::login` posts to `/api/auth/login`.
//! 3. On success, the [`DioxusSession`] is replaced with the new state and
//!    the navigator pushes the post-login target — `?next=` if provided
//!    and same-origin, else `/`.
//! 4. On 401 / 429 / network error, the form re-enables and renders an
//!    inline `Banner` with a spec-verbatim error string.

use dioxus::prelude::*;
use ts6_manager_shared::auth::{
    LoginRequest, UserInfo, auth_error_strings as msg,
};

use crate::client::auth::{self, AuthError};
use crate::client::dioxus::use_session;
use crate::client::setup as setup_client;
use crate::client::store::AuthState;
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, Field, PasswordInput, TextInput,
};
use crate::ui::routes::Route;

/// `/login?next=…` — credentials form.
#[component]
pub fn LoginPage(next: Option<String>) -> Element {
    let session = use_session();
    let nav = use_navigator();

    // If the user is already authenticated when they hit /login, send
    // them straight to the post-login target. This makes the route a
    // no-op for already-logged-in users instead of letting them log in a
    // second time over their own session.
    let already = matches!(*session.state.read(), AuthState::Authenticated { .. });
    let redirect_target_for_already = next.clone();
    use_effect(move || {
        if already {
            let target = post_login_target(redirect_target_for_already.as_deref());
            nav.replace(target);
        }
    });

    // First-run gate (PURA-34): if no admin exists yet, the login form is
    // moot — bounce the operator to `/setup` so they can create one. We
    // skip the gate for already-authed users (the previous effect handled
    // them) and on status-fetch errors (the operator sees the login form
    // as a fallback; they'll get a friendly auth error if there's truly no
    // admin to log in as).
    {
        let nav = nav.clone();
        use_future(move || async move {
            if already {
                return;
            }
            let base = api_base();
            if let Ok(status) = setup_client::status(&base).await {
                if status.needs_setup {
                    nav.replace(Route::SetupPage {});
                }
            }
        });
    }

    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut submitting = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    // Disable submit while a request is in flight or both fields aren't filled.
    let can_submit = !submitting() && !username.read().is_empty() && !password.read().is_empty();

    let next_for_submit = next.clone();
    let session_for_submit = session.clone();
    let onsubmit = move |evt: FormEvent| {
        evt.prevent_default();
        if !can_submit {
            return;
        }
        let user = username.read().clone();
        let pass = password.read().clone();
        let next_target = next_for_submit.clone();
        let session = session_for_submit.clone();
        submitting.set(true);
        error.set(None);
        spawn(async move {
            let result = auth::login(
                api_base().as_str(),
                &LoginRequest {
                    username: user,
                    password: pass,
                },
            )
            .await;
            match result {
                Ok(pair) => {
                    // The login response carries the token pair but not
                    // the user profile. Fetch /me with the brand-new
                    // access token so the session is fully populated
                    // before we redirect.
                    let user = auth::me(api_base().as_str(), &pair.access_token)
                        .await
                        .unwrap_or_else(|_| UserInfo {
                            id: 0,
                            username: String::new(),
                            display_name: String::new(),
                            role: String::new(),
                        });
                    session.replace(AuthState::Authenticated {
                        access: pair.access_token,
                        refresh: pair.refresh_token,
                        user,
                    });
                    submitting.set(false);
                    nav.replace(post_login_target(next_target.as_deref()));
                }
                Err(e) => {
                    submitting.set(false);
                    error.set(Some(login_error_message(&e).to_string()));
                }
            }
        });
    };

    let banner_msg = error.read().clone();

    rsx! {
        div { class: "app-root login-page",
            section { class: "stack-md login-card",
                h1 { "Sign in" }
                p { "Enter your operator credentials to manage your TeamSpeak servers." }

                if let Some(text) = banner_msg.as_deref() {
                    Banner { variant: BannerVariant::Danger, title: "Sign-in failed", "{text}" }
                }

                form { onsubmit: onsubmit, novalidate: "true",
                    Field {
                        label: "Username".to_string(),
                        id: "login-username".to_string(),
                        required: true,
                        TextInput {
                            id: "login-username".to_string(),
                            name: "username".to_string(),
                            autocomplete: "username".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: username.read().clone(),
                            error: banner_msg.is_some(),
                            oninput: move |evt: FormEvent| username.set(evt.value()),
                        }
                    }
                    Field {
                        label: "Password".to_string(),
                        id: "login-password".to_string(),
                        required: true,
                        PasswordInput {
                            id: "login-password".to_string(),
                            name: "password".to_string(),
                            autocomplete: "current-password".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: password.read().clone(),
                            error: banner_msg.is_some(),
                            oninput: move |evt: FormEvent| password.set(evt.value()),
                        }
                    }
                    Button {
                        kind: ButtonType::Submit,
                        size: ButtonSize::Large,
                        block: true,
                        loading: submitting(),
                        disabled: !can_submit,
                        onclick: move |_| { /* form `onsubmit` carries the action */ },
                        "Sign in"
                    }
                }
            }
        }
    }
}

/// Spec-verbatim error copy. Every branch maps to one of the constants in
/// [`auth_error_strings`] — never duplicate the string, always reference
/// the constant so a copy revision in the shared crate updates the UI.
fn login_error_message(err: &AuthError) -> &'static str {
    match err {
        // 401 with the "Invalid or expired token" body should never arrive
        // on the login route — but if it does, treat it like a credential
        // mismatch rather than spawning a refresh dance.
        AuthError::Unauthorized(m) if m == msg::USER_DISABLED => msg::USER_DISABLED,
        AuthError::Unauthorized(_) => msg::INVALID_CREDENTIALS,
        AuthError::Client { status: 429, .. } => msg::RATE_LIMIT_AUTH,
        AuthError::Client { .. } => msg::INVALID_CREDENTIALS,
        AuthError::Server { .. } => msg::SIGN_IN_UNAVAILABLE,
        AuthError::Transport(_) => msg::SIGN_IN_UNAVAILABLE,
        AuthError::Deserialise(_) => msg::SIGN_IN_UNAVAILABLE,
        AuthError::UnsupportedTarget => msg::SIGN_IN_UNAVAILABLE,
    }
}

/// Decide where to send the user after successful login.
///
/// `?next=` is honoured only if it parses as a same-origin **path** — i.e.
/// starts with `/` and is not a protocol-relative URL (`//evil.example`).
/// Anything else falls back to `/` so a malicious link can't redirect the
/// session off-site.
fn post_login_target(next: Option<&str>) -> Route {
    if let Some(raw) = next
        && is_safe_internal_path(raw)
        && !raw.starts_with("/login")
    {
        // Future routes may parse arbitrary paths via the macro's
        // `from_str`. For PURA-14 we only ship Login + Dashboard, so
        // any safe non-login path lands on the dashboard placeholder.
        return Route::DashboardPlaceholder {};
    }
    Route::DashboardPlaceholder {}
}

/// `?next=` is acceptable iff:
/// - non-empty
/// - starts with a single `/` (so not `//other-host`)
/// - has no scheme (`http:`, `javascript:`, …)
/// - has no `\` (which some browsers normalise to `/` and bypass the check)
fn is_safe_internal_path(p: &str) -> bool {
    if p.is_empty() || !p.starts_with('/') {
        return false;
    }
    if p.starts_with("//") || p.starts_with("/\\") {
        return false;
    }
    if p.contains('\\') {
        return false;
    }
    if p.contains(':') {
        return false;
    }
    true
}

/// API base URL. On WASM we read it from `window.location.origin` so the
/// SPA targets the same origin it was served from; on native (SSR / tests)
/// the function returns an empty string — the login code path never runs
/// off-WASM in practice.
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

    #[test]
    fn safe_paths_accepted() {
        assert!(is_safe_internal_path("/"));
        assert!(is_safe_internal_path("/dashboard"));
        assert!(is_safe_internal_path("/servers/42/edit"));
    }

    #[test]
    fn unsafe_paths_rejected() {
        assert!(!is_safe_internal_path(""));
        assert!(!is_safe_internal_path("dashboard"));
        assert!(!is_safe_internal_path("//evil.example/path"));
        assert!(!is_safe_internal_path("/\\evil.example"));
        assert!(!is_safe_internal_path("/path\\with-backslash"));
        assert!(!is_safe_internal_path("javascript:alert(1)"));
        assert!(!is_safe_internal_path("https://evil.example"));
        assert!(!is_safe_internal_path("/path?with=colon:in-it"));
    }

    #[test]
    fn post_login_target_falls_back_to_dashboard_for_unsafe_next() {
        // We can compare Route variants because Route derives PartialEq.
        let target = post_login_target(Some("//evil.example/oauth"));
        assert_eq!(target, Route::DashboardPlaceholder {});
    }

    #[test]
    fn post_login_target_avoids_login_loop_when_next_points_back_at_login() {
        let target = post_login_target(Some("/login"));
        assert_eq!(target, Route::DashboardPlaceholder {});
    }

    #[test]
    fn post_login_target_uses_dashboard_for_safe_next() {
        // Only Login + Dashboard exist today; any safe path lands on
        // Dashboard. This test pins the contract so when more routes
        // come online the test will fail and force the implementer to
        // wire arbitrary path resolution properly.
        let target = post_login_target(Some("/servers"));
        assert_eq!(target, Route::DashboardPlaceholder {});
    }

    #[test]
    fn login_error_messages_match_spec_constants() {
        // Wrong-username path → `Invalid username or password` (spec §28).
        let e = AuthError::Unauthorized("Invalid credentials".into());
        assert_eq!(login_error_message(&e), msg::INVALID_CREDENTIALS);

        // Disabled-account path → distinct copy.
        let e = AuthError::Unauthorized(msg::USER_DISABLED.into());
        assert_eq!(login_error_message(&e), msg::USER_DISABLED);

        // Rate-limited → byte-for-byte the spec's 429 string.
        let e = AuthError::Client {
            status: 429,
            message: msg::RATE_LIMIT_AUTH.into(),
        };
        assert_eq!(login_error_message(&e), msg::RATE_LIMIT_AUTH);

        // 5xx / network errors share a single copy that doesn't blame the
        // user for the server being down.
        let e = AuthError::Server {
            status: 503,
            message: "boom".into(),
        };
        assert_eq!(login_error_message(&e), msg::SIGN_IN_UNAVAILABLE);
    }
}
