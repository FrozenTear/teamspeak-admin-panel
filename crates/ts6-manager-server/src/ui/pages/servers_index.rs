//! `/servers` — list configured TS6 connections + add-server CTA.
//!
//! Replaces [PURA-213](/PURA/issues/PURA-213)'s catch-all destination for
//! the dashboard empty-state CTA ("Add a server" on `dashboard_placeholder.rs`)
//! and the header [`ServerSelector`](crate::ui::components::ServerSelector)
//! footer ("Manage servers…"). Both link targets stayed `/servers` after
//! PURA-213 shipped, so adding an explicit `#[route("/servers")]` is the
//! whole wire-up — no callers need to change.
//!
//! Data comes from the shared [`ServersContext`](crate::ui::layout::ServersContext)
//! mounted by [`AppShell`](crate::ui::layout::AppShell), the same source the
//! header selector reads. Re-using that hoisted resource means visiting
//! `/servers` does not fire a second `GET /api/servers` when the chrome
//! already has the list in hand.
//!
//! ## Scope (PURA-218)
//!
//! Phase 1 ships the read-side surface only:
//!
//! - List of `ServerSummary` rows (name, host, transport, status).
//! - Empty / loading / error states matching the dashboard pattern.
//! - "Add a server" CTA → `/setup`. The setup wizard guards itself with
//!   `GET /api/setup/status.needsSetup`; once any user exists it redirects
//!   to `/login`, so this link only does the right thing for first-run
//!   panels. A follow-up ticket rewrites the wizard into a slim add-server
//!   form usable post-bootstrap.
//!
//! ## Scope (PURA-222)
//!
//! Phase 2 lights up the row-level CRUD now that `PATCH /api/servers/:id`
//! (PURA-221) and `DELETE /api/servers/:id` (this ticket) are wired:
//!
//! - **Edit** — slide-over drawer per row that `PATCH`-es name, host,
//!   ports, transport (`useHttps`), and credentials. Empty `apiKey` /
//!   `sshPassword` fields preserve the existing sealed values; explicit
//!   non-empty values re-seal at the server. Only admins see the button —
//!   the route enforces `RequireAdmin`, this is FE-side suppression so a
//!   non-admin doesn't see affordances that would 403.
//! - **Delete** — confirm dialog mirroring the Widget Manager pattern;
//!   the operator must retype the server name to arm the destructive
//!   action.
//! - **Last seen** — `lastSeenAt` from the wire is rendered as a relative
//!   timestamp (`5 min ago`); rows that have never been probed render as
//!   `Never` rather than the 1970 epoch.
//!
//! After a successful PATCH the in-memory [`ServersContext`] is updated
//! in place; after DELETE the row is removed from the same signal so the
//! header selector and any other consumer of the shared context refresh
//! without a redundant `GET /api/servers`.

use dioxus::prelude::*;
use std::sync::Arc;
use ts6_manager_shared::servers::{PatchServerRequest, ServerSummary};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::{self as client_root};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, Field, PasswordInput,
    TextInput,
};
use crate::ui::layout::{ServersData, use_servers_context};
use crate::ui::routes::Route;

#[component]
pub fn ServersIndexPage() -> Element {
    // AppShell already redirects anonymous sessions to /login; render
    // nothing for the frame between auth state change and effect firing.
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }

    let ctx = use_servers_context();
    let snap = ctx.data.read().clone();
    // PURA-222 — only admins can edit/delete. The route layer enforces
    // `RequireAdmin`; this is the FE-side suppression so a non-admin
    // doesn't see buttons that would 403.
    let is_admin = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin"))
        .unwrap_or(false);

    rsx! {
        div { class: "crumb", "Servers" }
        h1 { "Servers" }

        section { class: "stack-md",
            { match snap {
                ServersData::Loading => rsx! { ServersIndexSkeleton {} },
                ServersData::Error(err) => rsx! { ServersIndexError { error: err } },
                ServersData::Loaded(rows) if rows.is_empty() => rsx! { ServersIndexEmpty {} },
                ServersData::Loaded(rows) => rsx! { ServersTable { rows: rows, is_admin: is_admin } },
            } }
        }
    }
}

#[component]
fn ServersIndexSkeleton() -> Element {
    rsx! {
        div { class: "servers-loading",
            span { class: "sr-only", role: "status", "aria-live": "polite",
                "Loading server list…"
            }
            div { class: "servers-loading-rows", "aria-hidden": "true",
                for _ in 0..3 {
                    div { class: "skeleton skeleton-line wide" }
                }
            }
        }
    }
}

#[component]
fn ServersIndexEmpty() -> Element {
    rsx! {
        div { class: "empty",
            div { class: "icon", "⬢" }
            h3 { "No servers configured" }
            p { "Add a TeamSpeak server to start managing it from this panel." }
            div { class: "actions",
                a { class: "btn btn-primary", href: "/setup", "Add a server" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServersIndexErrorProps {
    error: ApiError,
}

#[component]
fn ServersIndexError(props: ServersIndexErrorProps) -> Element {
    let (title, body) = error_copy(&props.error);
    let show_signin_cta = props.error.is_unauthorized();
    rsx! {
        Banner { variant: BannerVariant::Danger, title: title.to_string(),
            "{body}"
            if show_signin_cta {
                div { class: "banner-actions",
                    SignInAgainButton {}
                }
            }
        }
    }
}

/// PURA-225 — escape-hatch CTA rendered next to any 401 banner on an
/// authenticated surface. Clears the persisted session and navigates to
/// `/login` so an operator whose access token is expired but whose refresh
/// path is wedged (transient 5xx / proxy strip / unknown 401 sub-code) has
/// a one-click path back to a working state instead of being stranded on
/// "Session expired" with no affordance. AppShell's auth-gate effect will
/// also fire on the signal flip, but the explicit `nav.replace` removes
/// the dependency on whether `use_effect` re-runs on the same render.
#[component]
fn SignInAgainButton() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let on_click = move |_| {
        let next = current_authed_path();
        session.replace(AuthState::Anonymous);
        nav.replace(Route::LoginPage { next: Some(next) });
    };
    rsx! {
        button {
            r#type: "button",
            class: "btn btn-primary",
            onclick: on_click,
            "Sign in again"
        }
    }
}

/// Best-effort `?next=` capture for the sign-in-again handler. On WASM we
/// read `window.location.pathname + search`; on SSR (where this never
/// actually fires from a user click) we fall back to `/servers`. The
/// global `current_path()` in `ui::layout` is module-private so this
/// duplicates the minimal slice we need.
fn current_authed_path() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            let mut out = loc.pathname().unwrap_or_else(|_| "/".into());
            if let Ok(search) = loc.search()
                && !search.is_empty()
            {
                out.push_str(&search);
            }
            return out;
        }
        "/".into()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "/servers".into()
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServersTableProps {
    rows: Vec<ServerSummary>,
    /// PURA-222 — gates the per-row Edit / Delete buttons. Non-admins still
    /// see the table (the route grants them server access via
    /// `server_user_grant`); they just can't mutate the rows.
    is_admin: bool,
}

/// Slide-over edit form / delete confirm — only one is open at a time, so
/// a single signal carries both the action and the row it targets.
#[derive(Clone, PartialEq)]
enum RowAction {
    Edit(ServerSummary),
    Delete(ServerSummary),
}

#[component]
fn ServersTable(props: ServersTableProps) -> Element {
    let is_admin = props.is_admin;
    let mut ctx = use_servers_context();
    let mut action: Signal<Option<RowAction>> = use_signal(|| None);

    let on_close = move |_| action.set(None);
    let on_saved = move |updated: ServerSummary| {
        // Replace the row in place; AppShell's selector + every other
        // `ServersContext` consumer picks up the new state immediately
        // without a redundant `GET /api/servers`.
        let mut data = ctx.data.write();
        if let ServersData::Loaded(ref mut rows) = *data
            && let Some(slot) = rows.iter_mut().find(|s| s.id == updated.id)
        {
            *slot = updated;
        }
        action.set(None);
    };
    let on_deleted = move |id: i64| {
        let mut data = ctx.data.write();
        if let ServersData::Loaded(ref mut rows) = *data {
            rows.retain(|s| s.id != id);
        }
        action.set(None);
    };

    rsx! {
        div { class: "servers-toolbar",
            a { class: "btn btn-primary", href: "/setup", "Add a server" }
        }
        table { class: "data-table",
            "aria-label": "Configured TeamSpeak servers",
            thead {
                tr {
                    th { scope: "col", "Name" }
                    th { scope: "col", "Host" }
                    th { scope: "col", "Transport" }
                    th { scope: "col", "SSH" }
                    th { scope: "col", "Last seen" }
                    th { scope: "col", "Status" }
                    if is_admin {
                        th { scope: "col", class: "actions-col",
                            span { class: "sr-only", "Actions" }
                        }
                    }
                }
            }
            tbody {
                for s in props.rows.iter() {
                    {
                        let s = s.clone();
                        let edit_target = s.clone();
                        let delete_target = s.clone();
                        rsx! {
                            tr { key: "{s.id}",
                                td { class: "server-name", "{s.name}" }
                                td { class: "server-host", "{s.host}" }
                                td { "{transport_label(&s)}" }
                                td { "{ssh_label(&s)}" }
                                td { class: "server-last-seen",
                                    {format_last_seen(s.last_seen_at)}
                                }
                                td {
                                    if s.enabled {
                                        span { class: "pill pill-ok", "Enabled" }
                                    } else {
                                        span { class: "pill pill-muted", "Disabled" }
                                    }
                                }
                                if is_admin {
                                    td { class: "row-actions",
                                        button {
                                            class: "btn btn-secondary btn-sm",
                                            r#type: "button",
                                            "aria-label": "Edit {s.name}",
                                            onclick: move |_| action.set(Some(RowAction::Edit(edit_target.clone()))),
                                            "Edit"
                                        }
                                        button {
                                            class: "btn btn-danger btn-sm",
                                            r#type: "button",
                                            "aria-label": "Delete {s.name}",
                                            onclick: move |_| action.set(Some(RowAction::Delete(delete_target.clone()))),
                                            "Delete"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        { match action.read().clone() {
            Some(RowAction::Edit(server)) => rsx! {
                EditServerDrawer {
                    server: server,
                    on_close: on_close,
                    on_saved: on_saved,
                }
            },
            Some(RowAction::Delete(server)) => rsx! {
                DeleteServerConfirm {
                    server: server,
                    on_close: on_close,
                    on_deleted: on_deleted,
                }
            },
            None => rsx! { "" },
        } }
    }
}

/// Render `lastSeenAt` as a relative "x min ago" string. Falls back to
/// `Never` when the row has no observation yet (the `None` branch on
/// `ServerSummary::last_seen_at`). Buckets keep the surface stable as the
/// timestamp ages — operators don't need single-second precision next to
/// every row.
fn format_last_seen(ts: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(ts) = ts else {
        return "Never".to_string();
    };
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 30 {
        // Negative deltas (clock skew between panel host and operator's
        // browser) collapse here too — surfacing "-3 d ago" would be a
        // bug, not a feature.
        return "just now".into();
    }
    if secs < 90 {
        return "1 min ago".into();
    }
    let minutes = delta.num_minutes();
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{hours} h ago");
    }
    let days = delta.num_days();
    if days < 7 {
        return format!("{days} d ago");
    }
    // Past a week, the absolute date communicates more than "9 d ago".
    ts.format("%Y-%m-%d").to_string()
}

#[derive(Props, Clone, PartialEq)]
struct EditServerDrawerProps {
    server: ServerSummary,
    on_close: EventHandler<MouseEvent>,
    on_saved: EventHandler<ServerSummary>,
}

#[component]
fn EditServerDrawer(props: EditServerDrawerProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    let initial = props.server.clone();
    let server_id = initial.id;
    let server_name = initial.name.clone();

    // Pre-fill from the server summary. Secrets (apiKey, sshPassword) are
    // never returned — leave their fields blank; an empty value on submit
    // means "preserve the existing sealed value".
    let mut name = use_signal(|| initial.name.clone());
    let mut host = use_signal(|| initial.host.clone());
    let mut webquery_port = use_signal(|| initial.webquery_port.to_string());
    let mut use_https = use_signal(|| initial.use_https);
    let mut api_key = use_signal(String::new);
    let mut ssh_port = use_signal(|| initial.ssh_port.to_string());
    let mut ssh_username = use_signal(|| initial.ssh_username.clone().unwrap_or_default());
    let mut ssh_password = use_signal(String::new);
    let mut enabled = use_signal(|| initial.enabled);
    let mut submitting = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let on_close = props.on_close;
    let on_saved = props.on_saved;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let name_v = name.read().trim().to_string();
        let host_v = host.read().trim().to_string();
        if name_v.is_empty() || host_v.is_empty() {
            error.set(Some("Name and host are required.".into()));
            return;
        }
        let wq_port_v = match webquery_port.read().trim().parse::<i64>() {
            Ok(n) if (1..=65535).contains(&n) => n,
            _ => {
                error.set(Some(
                    "WebQuery port must be a number between 1 and 65535.".into(),
                ));
                return;
            }
        };
        let ssh_port_v = match ssh_port.read().trim().parse::<i64>() {
            Ok(n) if (1..=65535).contains(&n) => n,
            _ => {
                error.set(Some(
                    "SSH port must be a number between 1 and 65535.".into(),
                ));
                return;
            }
        };

        let api_key_v = api_key.read().clone();
        let ssh_user_v = ssh_username.read().trim().to_string();
        let ssh_password_v = ssh_password.read().clone();

        let req = PatchServerRequest {
            name: Some(name_v),
            host: Some(host_v),
            webquery_port: Some(wq_port_v),
            api_key: if api_key_v.is_empty() {
                None
            } else {
                Some(api_key_v)
            },
            use_https: Some(*use_https.read()),
            ssh_port: Some(ssh_port_v),
            // Empty string flows through the route's `nullable()` mapping
            // as "clear"; a non-empty value writes through.
            ssh_username: Some(ssh_user_v),
            ssh_password: if ssh_password_v.is_empty() {
                None
            } else {
                Some(ssh_password_v)
            },
            ..Default::default()
        };

        // `enabled` stays out of `PatchServerRequest` until the wire type
        // grows it; the toggle is wired so the future expansion (which
        // PURA-221 left at "out of scope") slots in here without a
        // redesign of the form.
        let _ = *enabled.read();

        let gate = gate.clone();
        submitting.set(true);
        error.set(None);

        spawn(async move {
            let res = client_root::servers::patch(&gate, &api::api_base(), server_id, &req).await;
            submitting.set(false);
            match res {
                Ok(updated) => on_saved.call(updated),
                Err(e) => error.set(Some(format_action_error(&e, "save server settings"))),
            }
        });
    };

    rsx! {
        div { class: "drawer-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "drawer",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "edit-server-title",
                div { class: "drawer-header",
                    h2 { id: "edit-server-title", "Edit {server_name}" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "drawer-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not save".to_string(),
                            "{msg}"
                        }
                    }
                    Field {
                        label: "Display name".to_string(),
                        id: "edit-server-name".to_string(),
                        required: true,
                        TextInput {
                            id: "edit-server-name".to_string(),
                            name: "name".to_string(),
                            value: name.read().clone(),
                            required: true,
                            disabled: *submitting.read(),
                            oninput: move |evt: FormEvent| name.set(evt.value()),
                            onchange: move |evt: FormEvent| name.set(evt.value()),
                        }
                    }
                    Field {
                        label: "Host".to_string(),
                        id: "edit-server-host".to_string(),
                        required: true,
                        helper: "DNS name or IP of the TS6 server.".to_string(),
                        TextInput {
                            id: "edit-server-host".to_string(),
                            name: "host".to_string(),
                            value: host.read().clone(),
                            required: true,
                            disabled: *submitting.read(),
                            oninput: move |evt: FormEvent| host.set(evt.value()),
                            onchange: move |evt: FormEvent| host.set(evt.value()),
                        }
                    }
                    Field {
                        label: "WebQuery port".to_string(),
                        id: "edit-server-wq-port".to_string(),
                        required: true,
                        TextInput {
                            id: "edit-server-wq-port".to_string(),
                            name: "webqueryPort".to_string(),
                            value: webquery_port.read().clone(),
                            required: true,
                            disabled: *submitting.read(),
                            oninput: move |evt: FormEvent| webquery_port.set(evt.value()),
                            onchange: move |evt: FormEvent| webquery_port.set(evt.value()),
                        }
                    }
                    label { class: "toggle-row",
                        input {
                            r#type: "checkbox",
                            checked: *use_https.read(),
                            disabled: *submitting.read(),
                            onchange: move |evt: FormEvent| use_https.set(evt.checked()),
                        }
                        span { class: "toggle-label", "Use HTTPS for WebQuery" }
                    }
                    Field {
                        label: "API key".to_string(),
                        id: "edit-server-api-key".to_string(),
                        optional: true,
                        helper: "Leave blank to preserve the existing API key.".to_string(),
                        PasswordInput {
                            id: "edit-server-api-key".to_string(),
                            name: "apiKey".to_string(),
                            autocomplete: "off".to_string(),
                            disabled: *submitting.read(),
                            value: api_key.read().clone(),
                            oninput: move |evt: FormEvent| api_key.set(evt.value()),
                            onchange: move |evt: FormEvent| api_key.set(evt.value()),
                        }
                    }
                    Field {
                        label: "SSH port".to_string(),
                        id: "edit-server-ssh-port".to_string(),
                        required: true,
                        TextInput {
                            id: "edit-server-ssh-port".to_string(),
                            name: "sshPort".to_string(),
                            value: ssh_port.read().clone(),
                            required: true,
                            disabled: *submitting.read(),
                            oninput: move |evt: FormEvent| ssh_port.set(evt.value()),
                            onchange: move |evt: FormEvent| ssh_port.set(evt.value()),
                        }
                    }
                    Field {
                        label: "SSH username".to_string(),
                        id: "edit-server-ssh-username".to_string(),
                        optional: true,
                        helper: "Leave blank to clear SSH credentials.".to_string(),
                        TextInput {
                            id: "edit-server-ssh-username".to_string(),
                            name: "sshUsername".to_string(),
                            autocomplete: "off".to_string(),
                            disabled: *submitting.read(),
                            value: ssh_username.read().clone(),
                            oninput: move |evt: FormEvent| ssh_username.set(evt.value()),
                            onchange: move |evt: FormEvent| ssh_username.set(evt.value()),
                        }
                    }
                    Field {
                        label: "SSH password".to_string(),
                        id: "edit-server-ssh-password".to_string(),
                        optional: true,
                        helper: "Leave blank to preserve the existing SSH password.".to_string(),
                        PasswordInput {
                            id: "edit-server-ssh-password".to_string(),
                            name: "sshPassword".to_string(),
                            autocomplete: "off".to_string(),
                            disabled: *submitting.read(),
                            value: ssh_password.read().clone(),
                            oninput: move |evt: FormEvent| ssh_password.set(evt.value()),
                            onchange: move |evt: FormEvent| ssh_password.set(evt.value()),
                        }
                    }
                    label { class: "toggle-row",
                        input {
                            r#type: "checkbox",
                            checked: *enabled.read(),
                            disabled: *submitting.read(),
                            onchange: move |evt: FormEvent| enabled.set(evt.checked()),
                        }
                        span { class: "toggle-label", "Enabled" }
                    }
                }
                div { class: "drawer-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        size: ButtonSize::Medium,
                        loading: *submitting.read(),
                        disabled: *submitting.read(),
                        "Save changes"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DeleteServerConfirmProps {
    server: ServerSummary,
    on_close: EventHandler<MouseEvent>,
    on_deleted: EventHandler<i64>,
}

#[component]
fn DeleteServerConfirm(props: DeleteServerConfirmProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    let server_id = props.server.id;
    let server_name = props.server.name.clone();
    let confirm_match = server_name.clone();

    let mut typed: Signal<String> = use_signal(String::new);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let on_close = props.on_close;
    let on_deleted = props.on_deleted;

    let typed_matches = *typed.read() == confirm_match;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() || !typed_matches {
            return;
        }
        submitting.set(true);
        error.set(None);
        let gate = gate.clone();
        spawn(async move {
            let path = format!("/api/servers/{server_id}");
            let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
            submitting.set(false);
            match res {
                Ok(()) => on_deleted.call(server_id),
                Err(e) => error.set(Some(format_action_error(&e, "delete this server"))),
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "delete-server-title",
                div { class: "modal-header",
                    h2 { id: "delete-server-title", "Delete server" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not delete".to_string(),
                            "{msg}"
                        }
                    }
                    p {
                        "Deleting "
                        strong { "{server_name}" }
                        " removes the connection from this panel and disconnects every dashboard, widget, and bot bound to it. The TS6 server itself is untouched."
                    }
                    label { class: "field",
                        span { class: "field-label",
                            "Type the server name to confirm: "
                            code { "{server_name}" }
                        }
                        input {
                            class: "input",
                            r#type: "text",
                            value: "{typed.read()}",
                            autofocus: true,
                            oninput: move |e| typed.set(e.value()),
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        kind: ButtonType::Submit,
                        disabled: !typed_matches || *submitting.read(),
                        loading: *submitting.read(),
                        "Delete server"
                    }
                }
            }
        }
    }
}

fn format_action_error(err: &ApiError, action: &str) -> String {
    match err {
        ApiError::Unauthorized(_) => "Session expired — sign in again to retry.".into(),
        ApiError::Client { status: 404, .. } => {
            "This server has already been removed. Refresh the page to update the list.".into()
        }
        ApiError::Client { status: 403, .. } => {
            "You don't have permission to perform this action.".into()
        }
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("Could not {action} ({status}): {message}")
        }
        ApiError::BadGateway { error, details, .. } => {
            let detail = details.as_deref().unwrap_or("");
            if detail.is_empty() {
                format!("Could not {action}: {error}")
            } else {
                format!("Could not {action}: {error} — {detail}")
            }
        }
        ApiError::Transport(m) => format!("Could not {action}: transport error ({m})."),
        ApiError::Deserialise(m) => format!("Could not {action}: unexpected response ({m})."),
        ApiError::UnsupportedTarget => format!("Could not {action} from this build."),
    }
}

fn transport_label(s: &ServerSummary) -> String {
    let scheme = if s.use_https { "https" } else { "http" };
    format!("{scheme}://{}:{}", s.host, s.webquery_port)
}

fn ssh_label(s: &ServerSummary) -> &'static str {
    if s.has_ssh_credentials {
        "Configured"
    } else {
        "—"
    }
}

fn error_copy(err: &ApiError) -> (&'static str, String) {
    match err {
        ApiError::Unauthorized(_) => (
            "Session expired",
            "Sign in again to view configured servers.".into(),
        ),
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut body = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                body.push_str(": ");
                body.push_str(d);
            }
            if let Some(c) = code {
                body.push_str(&format!(" (code {c})"));
            }
            ("Could not load servers", body)
        }
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            ("Could not load servers", format!("{status}: {message}"))
        }
        ApiError::Transport(m) => ("Could not load servers", format!("Transport error: {m}")),
        ApiError::Deserialise(m) => (
            "Unexpected response",
            format!("/api/servers returned an unexpected payload: {m}"),
        ),
        ApiError::UnsupportedTarget => (
            "Server list unavailable",
            "This view does not support the live server list.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use crate::ui::layout::ServersContext;
    use chrono::Utc;
    use std::sync::Arc;
    use ts6_manager_shared::auth::UserInfo;

    fn fixture(id: i64, name: &str, host: &str) -> ServerSummary {
        let now = Utc::now();
        ServerSummary {
            id,
            name: name.into(),
            host: host.into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: Some("admin".into()),
            has_ssh_credentials: true,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
            last_seen_at: None,
        }
    }

    #[test]
    fn transport_label_renders_https_prefix_when_use_https_true() {
        let s = fixture(1, "Primary", "ts.example.com");
        assert_eq!(transport_label(&s), "https://ts.example.com:10080");
    }

    #[test]
    fn transport_label_renders_http_prefix_when_use_https_false() {
        let mut s = fixture(1, "Primary", "ts.example.com");
        s.use_https = false;
        assert_eq!(transport_label(&s), "http://ts.example.com:10080");
    }

    #[test]
    fn ssh_label_reflects_configured_credentials_flag() {
        let mut s = fixture(1, "Primary", "ts.example.com");
        assert_eq!(ssh_label(&s), "Configured");
        s.has_ssh_credentials = false;
        assert_eq!(ssh_label(&s), "—");
    }

    #[test]
    fn error_copy_for_unauthorized_uses_session_expired_phrasing() {
        let (title, body) = error_copy(&ApiError::Unauthorized("expired".into()));
        assert_eq!(title, "Session expired");
        assert!(body.contains("Sign in"));
    }

    #[test]
    fn error_copy_for_bad_gateway_inlines_upstream_envelope() {
        let (_, body) = error_copy(&ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1153),
            details: Some("invalid serverID".into()),
        });
        assert!(body.contains("TeamSpeak API Error"));
        assert!(body.contains("invalid serverID"));
        assert!(body.contains("1153"));
    }

    // ── SSR markup contract ────────────────────────────────────────────────

    /// Synthetic single-route Router so [`ServersIndexPage`] (and the
    /// PURA-225 `SignInAgainButton` inside its 401 banner) can call
    /// `use_navigator()` during SSR without panicking. The production
    /// `Route::ServersIndexPage` lives under `#[layout(AppShell)]` which
    /// would pull in the whole chrome; the test only needs the navigator
    /// context, so we mount a stand-in enum with the same route shape.
    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum ServersIndexTestRoute {
        #[route("/")]
        TestHarness {},
    }

    thread_local! {
        static TEST_DATA: std::cell::RefCell<Option<ServersData>> =
            const { std::cell::RefCell::new(None) };
    }

    #[component]
    #[allow(non_snake_case)]
    fn TestHarness() -> Element {
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
        use_context_provider(|| provide_auth_gate(session));
        let data = TEST_DATA
            .with(|d| d.borrow().clone())
            .unwrap_or(ServersData::Loading);
        use_context_provider(|| ServersContext {
            data: Signal::new(data),
        });
        rsx! { ServersIndexPage {} }
    }

    fn render_with_state(data: ServersData) -> String {
        TEST_DATA.with(|d| *d.borrow_mut() = Some(data));
        let mut dom = VirtualDom::new(|| rsx! { Router::<ServersIndexTestRoute> {} });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        TEST_DATA.with(|d| *d.borrow_mut() = None);
        html
    }

    #[test]
    fn empty_state_renders_add_server_cta_pointing_at_setup() {
        let html = render_with_state(ServersData::Loaded(Vec::new()));
        assert!(html.contains("No servers configured"), "got: {html}");
        assert!(
            html.contains(r#"href="/setup""#),
            "empty-state CTA must link to /setup: {html}"
        );
    }

    #[test]
    fn loaded_state_renders_one_row_per_server_with_host_and_transport() {
        let rows = vec![
            fixture(1, "Primary", "ts1.example.com"),
            fixture(2, "Backup", "ts2.example.com"),
        ];
        let html = render_with_state(ServersData::Loaded(rows));
        assert!(html.contains("Primary"));
        assert!(html.contains("ts1.example.com"));
        assert!(html.contains("Backup"));
        assert!(html.contains("ts2.example.com"));
        assert!(
            html.contains("https://ts1.example.com:10080"),
            "transport column must surface scheme://host:port: {html}"
        );
        // Toolbar Add-server affordance still visible on non-empty list.
        assert!(
            html.contains(r#"href="/setup""#),
            "non-empty list still surfaces /setup CTA in toolbar: {html}"
        );
    }

    #[test]
    fn loading_state_announces_via_sr_only_status_region() {
        let html = render_with_state(ServersData::Loading);
        assert!(
            html.contains(r#"role="status""#),
            "loading state needs a live region for AT: {html}"
        );
        assert!(html.contains("Loading server list"));
    }

    #[test]
    fn error_state_renders_danger_banner_with_upstream_copy() {
        let html = render_with_state(ServersData::Error(ApiError::Transport(
            "connection refused".into(),
        )));
        // Banner danger styling produces the `banner-danger` class.
        assert!(
            html.contains("banner-danger") || html.contains("Could not load"),
            "danger banner expected for error state: {html}"
        );
        assert!(html.contains("connection refused"));
    }

    /// PURA-225 — when the `/api/servers` fetch is a 401, the danger
    /// banner MUST surface a primary "Sign in again" CTA so the operator
    /// has a one-click escape from the stuck-with-banner state. Asserting
    /// the literal label + a primary-button class pins the affordance so
    /// a future banner refactor that drops the CTA flags as a regression.
    #[test]
    fn unauthorized_error_state_renders_sign_in_again_cta() {
        let html = render_with_state(ServersData::Error(ApiError::Unauthorized(
            "Invalid or expired token".into(),
        )));
        assert!(
            html.contains("Sign in again"),
            "401 banner must include a `Sign in again` CTA: {html}"
        );
        assert!(
            html.contains("btn-primary"),
            "CTA must render as a primary button so it's the obvious next \
             action, not an inline link inside the body copy: {html}"
        );
    }

    /// PURA-225 — non-401 error states (transport / 5xx / 502) MUST NOT
    /// render the sign-in CTA. Those are recoverable on retry and showing
    /// "Sign in again" would mislead the operator into logging out for a
    /// transient backend hiccup.
    #[test]
    fn non_unauthorized_error_state_omits_sign_in_again_cta() {
        for err in [
            ApiError::Transport("connection refused".into()),
            ApiError::Server {
                status: 502,
                message: "bad gateway".into(),
            },
            ApiError::Server {
                status: 500,
                message: "boom".into(),
            },
        ] {
            let html = render_with_state(ServersData::Error(err.clone()));
            assert!(
                !html.contains("Sign in again"),
                "non-401 error {err:?} must NOT render the sign-in CTA: {html}"
            );
        }
    }

    // ── PURA-222 — last-seen + row-action affordances ─────────────────────

    #[test]
    fn format_last_seen_renders_never_when_field_is_none() {
        assert_eq!(format_last_seen(None), "Never");
    }

    #[test]
    fn format_last_seen_buckets_recent_into_relative_units() {
        let now = chrono::Utc::now();
        // Under 30s collapses to "just now" so the column doesn't churn
        // every refresh.
        assert_eq!(
            format_last_seen(Some(now - chrono::Duration::seconds(5))),
            "just now"
        );
        // 30s..90s collapses to "1 min ago" — the natural human bucket
        // before the minute count starts incrementing.
        assert_eq!(
            format_last_seen(Some(now - chrono::Duration::seconds(45))),
            "1 min ago"
        );
        assert_eq!(
            format_last_seen(Some(now - chrono::Duration::minutes(5))),
            "5 min ago"
        );
        assert_eq!(
            format_last_seen(Some(now - chrono::Duration::hours(3))),
            "3 h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - chrono::Duration::days(2))),
            "2 d ago"
        );
    }

    #[test]
    fn format_last_seen_falls_back_to_iso_date_past_one_week() {
        let ts = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .and_hms_opt(12, 30, 0)
                .unwrap(),
            chrono::Utc,
        );
        // Past a week, the absolute date carries more information than
        // "9 d ago"; pin the date format so a future locale-aware change
        // doesn't silently re-format the column.
        let rendered = format_last_seen(Some(ts));
        assert_eq!(rendered, "2026-01-15");
    }

    #[test]
    fn format_last_seen_handles_clock_skew_with_just_now() {
        // Operator's browser clock is slightly behind the server's; the
        // delta would be negative. Don't surface "-3 d ago" to the user.
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::seconds(5);
        assert_eq!(format_last_seen(Some(future)), "just now");
    }

    #[test]
    fn loaded_state_renders_last_seen_column_header_and_never_default() {
        let rows = vec![fixture(1, "Primary", "ts1.example.com")];
        let html = render_with_state(ServersData::Loaded(rows));
        assert!(
            html.contains("Last seen"),
            "table must surface a `Last seen` column header: {html}"
        );
        assert!(
            html.contains("Never"),
            "rows with no `lastSeenAt` render as `Never`: {html}"
        );
    }

    #[test]
    fn admin_sees_edit_and_delete_row_buttons() {
        let rows = vec![fixture(1, "Primary", "ts1.example.com")];
        // Default test harness provides the admin role (UserInfo above).
        let html = render_with_state(ServersData::Loaded(rows));
        assert!(
            html.contains(r#"aria-label="Edit Primary""#),
            "admin must see a per-row Edit affordance with an aria-label \
             that names the target server: {html}"
        );
        assert!(
            html.contains(r#"aria-label="Delete Primary""#),
            "admin must see a per-row Delete affordance with an aria-label \
             that names the target server: {html}"
        );
    }
}
