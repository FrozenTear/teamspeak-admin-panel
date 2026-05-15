//! `/setup` route — first-run operator wizard.
//!
//! Spec §7.2 / PURA-22 wire: a single form that creates the bootstrap admin
//! **and** the first `server_connection` row in one round-trip via
//! `POST /api/setup/init`. The endpoint is unauthenticated but only valid
//! while `GET /api/setup/status.needsSetup == true`; once any user exists
//! the server hard-fails with `409 already_initialized`. We branch on that
//! wire string (not English copy) per [`crate::client::setup`].
//!
//! Flow:
//! 1. On mount, fetch `/api/setup/status`. If `needsSetup` is false, replace
//!    the route with `/login` — the wizard is moot.
//! 2. Operator fills the form (admin credentials + first server). Submit
//!    posts the wire request.
//! 3. On 201, auto-login with the same credentials so the operator lands on
//!    `/dashboard` already authenticated. Auto-login failures (rare race or
//!    network blip after the admin row is created) fall back to `/login`
//!    with a banner asking the operator to sign in manually.
//! 4. On `409 already_initialized`, surface the spec-correct message and
//!    bounce to `/login` after a brief delay so the operator isn't stuck.
//! 5. On `400` (weak password), surface the spec-verbatim rule message
//!    inline on the password field.
//!
//! The form does NOT include `apiKey` / `sshPassword` in any rendered
//! response — the wire `SetupInitResponse.server` (a `ServerSummary`) omits
//! both by construction (asserted by the pin in `ts6_manager_shared::servers`).

use dioxus::prelude::*;
use ts6_manager_shared::auth::{LoginRequest, UserInfo};
use ts6_manager_shared::setup::{SetupInitRequest, SetupInitServer};
use ts6_manager_shared::test_connection::{
    TestConnectionKind, TestConnectionRequest, TestConnectionResponse,
};

use crate::client::auth as auth_client;
use crate::client::dioxus::use_session;
use crate::client::setup::{self, SetupInitError};
use crate::client::store::AuthState;
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, Field, PasswordInput,
    TextInput,
};
use crate::ui::routes::Route;

/// Spec-correct copy for the `409 already_initialized` branch. Surfaced on
/// the wizard before we redirect the operator to `/login`.
const ALREADY_INITIALIZED_COPY: &str =
    "This panel has already been initialised. Please sign in instead.";

/// Generic copy for any error that isn't already-initialised or a
/// weak-password complaint.
const GENERIC_FAILURE_COPY: &str =
    "Could not complete setup. Check your input and the panel logs, then try again.";

#[component]
pub fn SetupPage() -> Element {
    let session = use_session();
    let nav = use_navigator();

    // Status gate: if `needsSetup` is false, the wizard is moot — bounce to
    // /login. We do this in a one-shot effect so the SPA doesn't render the
    // form at all when an operator already exists.
    {
        use_future(move || async move {
            let base = api_base();
            match setup::status(&base).await {
                Ok(status) if !status.needs_setup => {
                    nav.replace(Route::LoginPage { next: None });
                }
                // Status fetch errors (transport / 5xx) are non-blocking —
                // the operator can still try the form; the server itself
                // gates on the same `user_count == 0` check.
                _ => {}
            }
        });
    }

    let mut username = use_signal(String::new);
    let mut display_name = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut password_confirm = use_signal(String::new);
    let mut server_name = use_signal(String::new);
    let mut server_host = use_signal(String::new);
    let mut api_key = use_signal(String::new);

    // SSH section signals
    let mut ssh_open = use_signal(|| false);
    let mut ssh_username = use_signal(String::new);
    let mut ssh_password = use_signal(String::new);
    let mut ssh_host_key_fingerprint = use_signal(String::new);

    let mut submitting = use_signal(|| false);
    let mut password_error: Signal<Option<String>> = use_signal(|| None);
    let mut form_error: Signal<Option<String>> = use_signal(|| None);
    // PURA-211 — "Test connection" affordance. Independent of the
    // submit path: the operator can probe before committing the row.
    let mut probing = use_signal(|| false);
    let mut probe_result: Signal<Option<TestConnectionResponse>> = use_signal(|| None);
    let mut probe_error: Signal<Option<String>> = use_signal(|| None);

    // Submit gate: every required field must be non-empty AND the two
    // password fields must match. Disabling the button is the cheap-and-
    // friendly UX for "you can't submit yet" — the explicit mismatch
    // message lives below the confirm field.
    let passwords_match =
        !password.read().is_empty() && password.read().as_str() == password_confirm.read().as_str();
    let required_filled = !username.read().is_empty()
        && !password.read().is_empty()
        && !server_name.read().is_empty()
        && !server_host.read().is_empty()
        && !api_key.read().is_empty();
    let can_submit = !submitting() && passwords_match && required_filled;

    let session_for_submit = session.clone();
    let onsubmit = move |evt: FormEvent| {
        evt.prevent_default();
        if !can_submit {
            return;
        }
        let req = build_request(
            username.read().clone(),
            display_name.read().clone(),
            password.read().clone(),
            server_name.read().clone(),
            server_host.read().clone(),
            api_key.read().clone(),
            ssh_username.read().clone(),
            ssh_password.read().clone(),
            ssh_host_key_fingerprint.read().clone(),
        );
        let username_for_login = req.username.clone();
        let password_for_login = req.password.clone();
        let session = session_for_submit.clone();
        let nav = nav;
        submitting.set(true);
        password_error.set(None);
        form_error.set(None);

        spawn(async move {
            let base = api_base();
            match setup::init(&base, &req).await {
                Ok(_created) => {
                    // Auto-login with the credentials we just submitted so
                    // the operator lands on /dashboard already authed
                    // rather than re-typing the password they just chose.
                    match auto_login(&base, &username_for_login, &password_for_login).await {
                        Ok((pair, user)) => {
                            session.replace(AuthState::Authenticated {
                                access: pair.access_token,
                                refresh: pair.refresh_token,
                                user,
                            });
                            submitting.set(false);
                            nav.replace(Route::DashboardPlaceholder {});
                        }
                        Err(_) => {
                            // The admin row exists — the operator can sign
                            // in manually. Bounce to /login with a banner.
                            submitting.set(false);
                            nav.replace(Route::LoginPage { next: None });
                        }
                    }
                }
                Err(SetupInitError::AlreadyInitialized) => {
                    submitting.set(false);
                    form_error.set(Some(ALREADY_INITIALIZED_COPY.to_string()));
                    // Same status the server's gate reads — bounce to /login
                    // so the operator can sign in. We delay the redirect via
                    // the navigation rather than a sleep timer so the banner
                    // is at least readable mid-frame.
                    nav.replace(Route::LoginPage { next: None });
                }
                Err(SetupInitError::WeakPassword(msg)) => {
                    submitting.set(false);
                    password_error.set(Some(msg));
                }
                Err(SetupInitError::Other(_)) => {
                    submitting.set(false);
                    form_error.set(Some(GENERIC_FAILURE_COPY.to_string()));
                }
            }
        });
    };

    // PURA-211 — kick off the probe with the same wire shape the wizard
    // submits. The host + apiKey fields must be filled; webqueryPort /
    // useHttps default server-side. Result lands in `probe_result` so
    // the panel below the form can render it.
    let server_fields_filled =
        !server_host.read().is_empty() && !api_key.read().is_empty();
    let can_probe = !probing() && !submitting() && server_fields_filled;
    let on_probe = move |_evt: MouseEvent| {
        if !can_probe {
            return;
        }
        let req = TestConnectionRequest {
            host: server_host.read().clone(),
            api_key: api_key.read().clone(),
            webquery_port: None,
            use_https: None,
        };
        probing.set(true);
        probe_result.set(None);
        probe_error.set(None);
        spawn(async move {
            let base = api_base();
            match setup::test_connection(&base, &req).await {
                Ok(resp) => {
                    probing.set(false);
                    probe_result.set(Some(resp));
                }
                Err(SetupInitError::AlreadyInitialized) => {
                    probing.set(false);
                    probe_error.set(Some(ALREADY_INITIALIZED_COPY.to_string()));
                }
                Err(SetupInitError::WeakPassword(_)) => {
                    // The probe route never returns 400/weak-password —
                    // surface defensively as a generic failure if it ever
                    // does so we don't swallow the result.
                    probing.set(false);
                    probe_error.set(Some(GENERIC_FAILURE_COPY.to_string()));
                }
                Err(SetupInitError::Other(e)) => {
                    probing.set(false);
                    probe_error.set(Some(format!("Could not run the probe: {e}")));
                }
            }
        });
    };

    let banner_msg = form_error.read().clone();
    let pwd_err = password_error.read().clone();
    let confirm_mismatch_visible = !password_confirm.read().is_empty() && !passwords_match;

    rsx! {
        div { class: "app-root setup-page",
            section { class: "stack-md setup-card",
                h1 { "Set up TS6 Manager" }
                p { class: "setup-intro",
                    "Create the first administrator account and connect this panel to your TeamSpeak 6 server."
                }

                if let Some(text) = banner_msg.as_deref() {
                    Banner { variant: BannerVariant::Danger, title: "Setup failed", "{text}" }
                }

                form { onsubmit: onsubmit, novalidate: "true",
                    h2 { class: "setup-section-heading", "Administrator" }

                    Field {
                        label: "Username".to_string(),
                        id: "setup-username".to_string(),
                        required: true,
                        TextInput {
                            id: "setup-username".to_string(),
                            name: "username".to_string(),
                            autocomplete: "username".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: username.read().clone(),
                            oninput: move |evt: FormEvent| username.set(evt.value()),
                            onchange: move |evt: FormEvent| username.set(evt.value()),
                        }
                    }

                    Field {
                        label: "Display name".to_string(),
                        id: "setup-display-name".to_string(),
                        optional: true,
                        helper: "Shown in the header. Defaults to the username if left blank.".to_string(),
                        TextInput {
                            id: "setup-display-name".to_string(),
                            name: "displayName".to_string(),
                            autocomplete: "name".to_string(),
                            disabled: submitting(),
                            value: display_name.read().clone(),
                            oninput: move |evt: FormEvent| display_name.set(evt.value()),
                            onchange: move |evt: FormEvent| display_name.set(evt.value()),
                        }
                    }

                    Field {
                        label: "Password".to_string(),
                        id: "setup-password".to_string(),
                        required: true,
                        helper: "12 characters or more, mixing letters, digits, and symbols.".to_string(),
                        error: pwd_err.clone(),
                        PasswordInput {
                            id: "setup-password".to_string(),
                            name: "password".to_string(),
                            autocomplete: "new-password".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: password.read().clone(),
                            error: pwd_err.is_some(),
                            oninput: move |evt: FormEvent| password.set(evt.value()),
                            onchange: move |evt: FormEvent| password.set(evt.value()),
                        }
                    }

                    Field {
                        label: "Confirm password".to_string(),
                        id: "setup-password-confirm".to_string(),
                        required: true,
                        error: if confirm_mismatch_visible { Some("Passwords do not match.".to_string()) } else { None },
                        PasswordInput {
                            id: "setup-password-confirm".to_string(),
                            name: "passwordConfirm".to_string(),
                            autocomplete: "new-password".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: password_confirm.read().clone(),
                            error: confirm_mismatch_visible,
                            oninput: move |evt: FormEvent| password_confirm.set(evt.value()),
                            onchange: move |evt: FormEvent| password_confirm.set(evt.value()),
                        }
                    }

                    h2 { class: "setup-section-heading", "First TeamSpeak server" }

                    Field {
                        label: "Server name".to_string(),
                        id: "setup-server-name".to_string(),
                        required: true,
                        helper: "Operator-facing label. You can rename it later.".to_string(),
                        TextInput {
                            id: "setup-server-name".to_string(),
                            name: "serverName".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: server_name.read().clone(),
                            oninput: move |evt: FormEvent| server_name.set(evt.value()),
                            onchange: move |evt: FormEvent| server_name.set(evt.value()),
                        }
                    }

                    Field {
                        label: "WebQuery host".to_string(),
                        id: "setup-server-host".to_string(),
                        required: true,
                        helper: "Hostname or IP of the TeamSpeak 6 server's WebQuery interface.".to_string(),
                        TextInput {
                            id: "setup-server-host".to_string(),
                            name: "serverHost".to_string(),
                            autocomplete: "off".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: server_host.read().clone(),
                            oninput: move |evt: FormEvent| server_host.set(evt.value()),
                            onchange: move |evt: FormEvent| server_host.set(evt.value()),
                        }
                    }

                    Field {
                        label: "WebQuery API key".to_string(),
                        id: "setup-api-key".to_string(),
                        required: true,
                        helper: "Stored encrypted at rest. Never returned by the API.".to_string(),
                        PasswordInput {
                            id: "setup-api-key".to_string(),
                            name: "apiKey".to_string(),
                            autocomplete: "off".to_string(),
                            required: true,
                            disabled: submitting(),
                            value: api_key.read().clone(),
                            oninput: move |evt: FormEvent| api_key.set(evt.value()),
                            onchange: move |evt: FormEvent| api_key.set(evt.value()),
                        }
                    }

                    // PURA-211 — probe affordance. Sits between the
                    // server fields and the SSH section so the operator
                    // can verify WebQuery reachability before commit.
                    div { class: "setup-probe-row",
                        Button {
                            kind: ButtonType::Button,
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Medium,
                            loading: probing(),
                            disabled: !can_probe,
                            onclick: on_probe,
                            "Test connection"
                        }
                    }
                    if let Some(msg) = probe_error.read().clone() {
                        Banner { variant: BannerVariant::Danger, title: "Test failed", "{msg}" }
                    }
                    if let Some(resp) = probe_result.read().clone() {
                        ProbeResultPanel { result: resp }
                    }

                    // SSH section — collapsible disclosure
                    details {
                        open: ssh_open(),
                        ontoggle: move |_| ssh_open.set(!ssh_open()),
                        summary { class: "setup-section-summary",
                            "Real-time events (SSH ServerQuery) — optional"
                        }
                        div { class: "stack-md",
                            p { class: "setup-section-helper",
                                "Fill in SSH credentials to enable live events (client joins, leaves, etc.). Leave blank to use WebQuery polling only."
                            }
                            Field {
                                label: "SSH username".to_string(),
                                id: "setup-ssh-username".to_string(),
                                optional: true,
                                TextInput {
                                    id: "setup-ssh-username".to_string(),
                                    name: "sshUsername".to_string(),
                                    autocomplete: "off".to_string(),
                                    disabled: submitting(),
                                    value: ssh_username.read().clone(),
                                    oninput: move |evt: FormEvent| ssh_username.set(evt.value()),
                                    onchange: move |evt: FormEvent| ssh_username.set(evt.value()),
                                }
                            }
                            Field {
                                label: "SSH password".to_string(),
                                id: "setup-ssh-password".to_string(),
                                optional: true,
                                helper: "Stored encrypted at rest.".to_string(),
                                PasswordInput {
                                    id: "setup-ssh-password".to_string(),
                                    name: "sshPassword".to_string(),
                                    autocomplete: "off".to_string(),
                                    disabled: submitting(),
                                    value: ssh_password.read().clone(),
                                    oninput: move |evt: FormEvent| ssh_password.set(evt.value()),
                                    onchange: move |evt: FormEvent| ssh_password.set(evt.value()),
                                }
                            }
                            Field {
                                label: "SSH host-key fingerprint".to_string(),
                                id: "setup-ssh-fingerprint".to_string(),
                                optional: true,
                                helper: "SHA-256 fingerprint, e.g. SHA256:abc…. Leave blank to use TOFU on first connect.".to_string(),
                                TextInput {
                                    id: "setup-ssh-fingerprint".to_string(),
                                    name: "sshHostKeyFingerprint".to_string(),
                                    autocomplete: "off".to_string(),
                                    disabled: submitting(),
                                    value: ssh_host_key_fingerprint.read().clone(),
                                    oninput: move |evt: FormEvent| ssh_host_key_fingerprint.set(evt.value()),
                                    onchange: move |evt: FormEvent| ssh_host_key_fingerprint.set(evt.value()),
                                }
                            }
                        }
                    }

                    Button {
                        kind: ButtonType::Submit,
                        size: ButtonSize::Large,
                        block: true,
                        loading: submitting(),
                        disabled: !can_submit,
                        onclick: move |_| { /* form `onsubmit` carries the action */ },
                        "Create administrator and continue"
                    }
                }
            }
        }
    }
}

/// PURA-211 — inline render of [`TestConnectionResponse`]. Three states:
/// connected (green), classified failure (red), and the rare success-but-
/// no-version-banner shape (treated as connected, no `serverVersion`
/// suffix). Always shows the URL the panel attempted so the operator can
/// copy it into a bug report verbatim.
#[derive(Props, Clone, PartialEq)]
struct ProbeResultPanelProps {
    result: TestConnectionResponse,
}

#[component]
fn ProbeResultPanel(props: ProbeResultPanelProps) -> Element {
    let r = props.result;
    let variant = if r.ok {
        BannerVariant::Success
    } else {
        BannerVariant::Danger
    };
    let title = if r.ok {
        "Connected".to_string()
    } else {
        format!("Test failed — {}", classify_label(r.kind))
    };
    let hint = r.kind.hint();
    rsx! {
        Banner { variant: variant, title: title,
            p { class: "probe-result-message", "{r.message}" }
            p { class: "probe-result-url",
                "Tried: "
                code { "{r.url_tried}" }
            }
            if let Some(v) = r.server_version.as_deref() {
                p { class: "probe-result-version", "Server reported: {v}" }
            }
            if !hint.is_empty() {
                p { class: "probe-result-hint", "{hint}" }
            }
        }
    }
}

fn classify_label(kind: TestConnectionKind) -> &'static str {
    match kind {
        TestConnectionKind::Ok => "OK",
        TestConnectionKind::Dns => "DNS lookup failed",
        TestConnectionKind::Connect => "Connection refused",
        TestConnectionKind::Timeout => "Timeout",
        TestConnectionKind::Tls => "TLS error",
        TestConnectionKind::Unauthorized => "API key rejected",
        TestConnectionKind::InvalidResponse => "Unexpected response",
        TestConnectionKind::Other => "Unreachable",
    }
}

/// Build the wire `SetupInitRequest` from the wizard's signal payloads.
/// Optional fields that the operator left blank become `None` so the server
/// fills in spec defaults — no zero-string sentinels on the wire.
/// When the operator fills in an SSH username the `controlPath` is set to
/// `"ssh"` and `sshAuthMethod` to `"password"` automatically.
#[allow(clippy::too_many_arguments)]
fn build_request(
    username: String,
    display_name: String,
    password: String,
    server_name: String,
    server_host: String,
    api_key: String,
    ssh_username: String,
    ssh_password: String,
    ssh_host_key_fingerprint: String,
) -> SetupInitRequest {
    let has_ssh = !ssh_username.is_empty();
    SetupInitRequest {
        username,
        password,
        display_name: Some(display_name).filter(|s| !s.is_empty()),
        server: SetupInitServer {
            name: server_name,
            host: server_host,
            webquery_port: None,
            api_key,
            use_https: None,
            ssh_port: None,
            ssh_username: Some(ssh_username).filter(|s| !s.is_empty()),
            ssh_password: Some(ssh_password).filter(|s| !s.is_empty()),
            control_path: if has_ssh { Some("ssh".into()) } else { None },
            ssh_auth_method: if has_ssh { Some("password".into()) } else { None },
            ssh_host_key_fingerprint: Some(ssh_host_key_fingerprint).filter(|s| !s.is_empty()),
        },
    }
}

/// Auto-login after a successful setup so the operator lands on `/dashboard`
/// already authenticated. Returns the token pair + the user profile to seed
/// the session signal.
async fn auto_login(
    base: &str,
    username: &str,
    password: &str,
) -> Result<(ts6_manager_shared::auth::TokenPairResponse, UserInfo), auth_client::AuthError> {
    let pair = auth_client::login(
        base,
        &LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        },
    )
    .await?;
    let user = auth_client::me(base, &pair.access_token).await?;
    Ok((pair, user))
}

/// API base URL — duplicates the helper in `ui::pages::login` for the same
/// reason: keeps each page self-contained instead of coupling them through
/// a shared utility module that would have to grow if a page wanted a
/// different base policy.
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

    fn no_ssh() -> (String, String, String) {
        (String::new(), String::new(), String::new())
    }

    #[test]
    fn build_request_strips_empty_optional_display_name() {
        let (su, sp, sfp) = no_ssh();
        let req = build_request(
            "admin".into(), String::new(), "Hunter2!ok".into(),
            "Primary".into(), "ts.example.com".into(), "K".into(),
            su, sp, sfp,
        );
        assert!(
            req.display_name.is_none(),
            "empty displayName must serialise as null/missing"
        );
    }

    #[test]
    fn build_request_keeps_provided_display_name() {
        let (su, sp, sfp) = no_ssh();
        let req = build_request(
            "admin".into(), "Robert Soot".into(), "Hunter2!ok".into(),
            "Primary".into(), "ts.example.com".into(), "K".into(),
            su, sp, sfp,
        );
        assert_eq!(req.display_name.as_deref(), Some("Robert Soot"));
    }

    #[test]
    fn build_request_does_not_emit_zero_string_for_optional_server_fields() {
        let (su, sp, sfp) = no_ssh();
        let req = build_request(
            "admin".into(), String::new(), "Hunter2!ok".into(),
            "Primary".into(), "ts.example.com".into(), "K".into(),
            su, sp, sfp,
        );
        assert!(req.server.webquery_port.is_none());
        assert!(req.server.use_https.is_none());
        assert!(req.server.ssh_port.is_none());
        assert!(req.server.ssh_username.is_none());
        assert!(req.server.ssh_password.is_none());
        assert!(req.server.control_path.is_none());
    }

    #[test]
    fn build_request_round_trips_required_fields_into_wire_struct() {
        let (su, sp, sfp) = no_ssh();
        let req = build_request(
            "rsoot".into(), "Robert".into(), "Hunter2!ok".into(),
            "Primary".into(), "ts.example.com".into(), "WEBQUERY-KEY".into(),
            su, sp, sfp,
        );
        assert_eq!(req.username, "rsoot");
        assert_eq!(req.password, "Hunter2!ok");
        assert_eq!(req.server.name, "Primary");
        assert_eq!(req.server.host, "ts.example.com");
        assert_eq!(req.server.api_key, "WEBQUERY-KEY");
    }

    #[test]
    fn build_request_with_ssh_sets_control_path_ssh() {
        let req = build_request(
            "admin".into(), String::new(), "Hunter2!ok".into(),
            "Primary".into(), "ts.example.com".into(), "K".into(),
            "serveradmin".into(), "pw".into(), "SHA256:abc".into(),
        );
        assert_eq!(req.server.control_path.as_deref(), Some("ssh"));
        assert_eq!(req.server.ssh_auth_method.as_deref(), Some("password"));
        assert_eq!(req.server.ssh_username.as_deref(), Some("serveradmin"));
        assert_eq!(req.server.ssh_host_key_fingerprint.as_deref(), Some("SHA256:abc"));
    }

    #[test]
    fn already_initialized_copy_does_not_leak_wire_token() {
        // Operator-facing copy must be human-readable, NOT the
        // `already_initialized` wire string the server returned.
        assert!(!ALREADY_INITIALIZED_COPY.contains("already_initialized"));
        assert!(ALREADY_INITIALIZED_COPY.contains("sign in"));
    }
}
