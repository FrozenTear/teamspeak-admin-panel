//! `/server-edit` — edit SSH credentials on an existing server connection.
//! PURA-221.
//!
//! Loads the selected server's current (non-secret) state via
//! `GET /api/servers`, pre-fills the form, and submits a
//! `PATCH /api/servers/:id` to update whichever fields the operator changed.

use dioxus::prelude::*;
use ts6_manager_shared::servers::PatchServerRequest;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::client::{self};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, Field, PasswordInput, TextInput};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

const GENERIC_FAILURE: &str = "Could not save server settings. Check your input and the panel logs, then try again.";

#[component]
pub fn ServerEditPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Edit server" }
            h1 { "Edit server" }
            div { class: "empty",
                div { class: "icon", "⊙" }
                h3 { "No server selected" }
                p { "Select a server from the sidebar before editing its settings." }
            }
        };
    };

    let server_id = server.id;
    let server_name = server.name.clone();

    // Pre-fill from the server summary. Secrets (apiKey, sshPassword) are
    // never returned — the operator must retype them to update.
    let mut ssh_username = use_signal(|| server.ssh_username.clone().unwrap_or_default());
    let mut ssh_password = use_signal(String::new);
    let mut ssh_host_key_fingerprint = use_signal(String::new);
    // controlPath toggle: if the server has ssh credentials, default to ssh
    let mut enable_ssh = use_signal(|| server.has_ssh_credentials);

    let mut submitting = use_signal(|| false);
    let mut success_msg: Signal<Option<String>> = use_signal(|| None);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);

    let gate_for_submit = gate.clone();
    let onsubmit = move |evt: FormEvent| {
        evt.prevent_default();
        if submitting() { return; }

        let ssh_u = ssh_username.read().clone();
        let ssh_p = ssh_password.read().clone();
        let ssh_fp = ssh_host_key_fingerprint.read().clone();
        let use_ssh = enable_ssh();

        let req = PatchServerRequest {
            ssh_username: Some(ssh_u.clone()),
            ssh_password: if ssh_p.is_empty() { None } else { Some(ssh_p) },
            control_path: Some(if use_ssh && !ssh_u.is_empty() { "ssh".into() } else { "webquery".into() }),
            ssh_auth_method: if use_ssh && !ssh_u.is_empty() { Some("password".into()) } else { None },
            ssh_host_key_fingerprint: if ssh_fp.is_empty() { None } else { Some(ssh_fp) },
            ..Default::default()
        };

        let gate = gate_for_submit.clone();
        submitting.set(true);
        success_msg.set(None);
        error_msg.set(None);

        spawn(async move {
            let base = api_base();
            match client::servers::patch(&gate, &base, server_id, &req).await {
                Ok(_) => {
                    submitting.set(false);
                    success_msg.set(Some("Server settings saved.".into()));
                }
                Err(ApiError::Client { status: 400, message }) => {
                    submitting.set(false);
                    error_msg.set(Some(message));
                }
                Err(_) => {
                    submitting.set(false);
                    error_msg.set(Some(GENERIC_FAILURE.into()));
                }
            }
        });
    };

    rsx! {
        div { class: "crumb", "Edit server · {server_name}" }
        h1 { "Edit server" }

        if let Some(msg) = success_msg.read().as_deref() {
            Banner { variant: BannerVariant::Success, title: "Saved", "{msg}" }
        }
        if let Some(msg) = error_msg.read().as_deref() {
            Banner { variant: BannerVariant::Danger, title: "Error", "{msg}" }
        }

        form { onsubmit: onsubmit, novalidate: "true", class: "stack-md",
            h2 { class: "setup-section-heading", "Real-time events (SSH ServerQuery)" }
            p { class: "setup-section-helper",
                "Fill in SSH credentials to enable live events. Leave the username blank to revert to WebQuery polling."
            }

            Field {
                label: "SSH username".to_string(),
                id: "edit-ssh-username".to_string(),
                optional: true,
                TextInput {
                    id: "edit-ssh-username".to_string(),
                    name: "sshUsername".to_string(),
                    autocomplete: "off".to_string(),
                    disabled: submitting(),
                    value: ssh_username.read().clone(),
                    oninput: move |evt: FormEvent| {
                        let v = evt.value();
                        enable_ssh.set(!v.is_empty());
                        ssh_username.set(v);
                    },
                    onchange: move |evt: FormEvent| {
                        let v = evt.value();
                        enable_ssh.set(!v.is_empty());
                        ssh_username.set(v);
                    },
                }
            }

            Field {
                label: "SSH password".to_string(),
                id: "edit-ssh-password".to_string(),
                optional: true,
                helper: "Leave blank to preserve the existing password.".to_string(),
                PasswordInput {
                    id: "edit-ssh-password".to_string(),
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
                id: "edit-ssh-fingerprint".to_string(),
                optional: true,
                helper: "SHA-256 fingerprint, e.g. SHA256:abc…. Leave blank to use TOFU.".to_string(),
                TextInput {
                    id: "edit-ssh-fingerprint".to_string(),
                    name: "sshHostKeyFingerprint".to_string(),
                    autocomplete: "off".to_string(),
                    disabled: submitting(),
                    value: ssh_host_key_fingerprint.read().clone(),
                    oninput: move |evt: FormEvent| ssh_host_key_fingerprint.set(evt.value()),
                    onchange: move |evt: FormEvent| ssh_host_key_fingerprint.set(evt.value()),
                }
            }

            Button {
                kind: ButtonType::Submit,
                size: ButtonSize::Large,
                loading: submitting(),
                disabled: submitting(),
                onclick: move |_| {},
                "Save settings"
            }
        }
    }
}

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
