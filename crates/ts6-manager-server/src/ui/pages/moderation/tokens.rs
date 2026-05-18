//! `/moderation/tokens` — privilege keys (tokens). PURA-376.
//!
//! Phase B of the PURA-369 moderation-completion plan. Backed by the
//! PURA-373 token route module (`routes/control/tokens.rs`):
//! `privilegekeylist` / `privilegekeyadd` / `privilegekeydelete`, mounted
//! at `/api/servers/{configId}/vs/{sid}/tokens`.
//!
//! A TS6 token is a one-time privilege key — a string a client redeems to
//! drop into a server group (`token_type == 0`) or a channel group
//! (`token_type == 1`). The key string itself is a credential: anyone who
//! holds it can join the privileged group. So the list never prints a key
//! inline — each row carries a masked field with a per-row Reveal, and the
//! create flow ends on a single copyable artifact (UX brief §7).
//!
//! Gating (PURA-369 plan §4.1): **read** = any operator with server
//! access; **write** (mint / delete) = admin only. The `/tokens` route
//! re-checks `check_admin` server-side regardless — the in-page gate just
//! suppresses dead affordances.

use std::collections::HashSet;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ChannelGroupItem, ChannelTreeNode, ServerGroupItem, TokenCreateRequest, TokenCreated, TokenItem,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

use super::{format_error, relative_from_unix};

/// Server-group / channel-group / channel names, used to decode the
/// GRANTS cell and to populate the create-flow pickers. Best-effort — a
/// failed sub-fetch leaves its vec empty and the GRANTS cell degrades to
/// the raw id rather than failing the whole page.
#[derive(Clone, Default, PartialEq)]
struct LookupData {
    server_groups: Vec<ServerGroupItem>,
    channel_groups: Vec<ChannelGroupItem>,
    channels: Vec<ChannelTreeNode>,
}

#[component]
pub fn TokensPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let role = session
        .state
        .read()
        .user()
        .map(|u| u.role.clone())
        .unwrap_or_default();
    // Write affordances (mint / delete) are admin-only; a read-only
    // operator still sees the list. The API enforces this regardless.
    let is_admin = role.eq_ignore_ascii_case("admin");

    let gate = use_auth_gate();
    let toaster = use_toaster();
    let hub = use_ws_hub();
    let servers_ctx = use_servers_context();
    let storage = session.storage.clone();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Moderation · Privilege keys" }
            h1 { "Privilege keys" }
            div { class: "empty",
                div { class: "icon", "🔑" }
                h3 { "No server selected" }
                p { "Choose a server to manage its privilege keys." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // ── primary resource: the token list (drives the four list states) ──
    let mut tokens_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_tokens(gate, server_id, sid).await }
        }
    });
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut tokens: Signal<Vec<TokenItem>> = use_signal(Vec::new);
    let mut loaded: Signal<bool> = use_signal(|| false);
    {
        use_effect(move || match &*tokens_resource.read_unchecked() {
            Some(Ok(rows)) => {
                tokens.set(rows.clone());
                error.set(None);
                loaded.set(true);
            }
            Some(Err(e)) => {
                error.set(Some(e.clone()));
                loaded.set(true);
            }
            None => {}
        });
    }

    // ── lookup resource: group + channel names (best-effort) ────────────
    let lookups_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_lookups(gate, server_id, sid).await }
        }
    });
    let lookups = lookups_resource
        .read_unchecked()
        .clone()
        .unwrap_or_default();

    // ── live refresh — tokens fan out on the per-server `moderation` topic
    {
        let hub = hub.clone();
        let _ws = use_resource(move || {
            let hub = hub.clone();
            async move {
                let topic = format!("server:{server_id}:moderation");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(env) = rx.next().await {
                    if matches!(env.kind.as_str(), "ts:token:created" | "ts:token:deleted") {
                        tokens_resource.restart();
                    }
                }
            }
        });
    }

    // ── per-row reveal + delete-confirm + create-flow state ─────────────
    let mut revealed: Signal<HashSet<String>> = use_signal(HashSet::new);
    let mut delete_target: Signal<Option<TokenItem>> = use_signal(|| None::<TokenItem>);
    let mut delete_busy: Signal<bool> = use_signal(|| false);

    let mut create_open: Signal<bool> = use_signal(|| false);
    let mut result_key: Signal<Option<String>> = use_signal(|| None::<String>);
    let mut form_type: Signal<i64> = use_signal(|| 0i64);
    let mut form_group: Signal<i64> = use_signal(|| 0i64);
    let mut form_channel: Signal<i64> = use_signal(|| 0i64);
    let mut form_description: Signal<String> = use_signal(String::new);
    let mut form_customset: Signal<String> = use_signal(String::new);
    let mut form_advanced: Signal<bool> = use_signal(|| false);
    let mut form_busy: Signal<bool> = use_signal(|| false);

    // Open the create modal with sane defaults for the current lookup data.
    let open_create = {
        let lookups = lookups.clone();
        move |_| {
            form_type.set(0);
            form_group.set(lookups.server_groups.first().map(|g| g.sgid).unwrap_or(0));
            form_channel.set(lookups.channels.first().map(|c| c.cid).unwrap_or(0));
            form_description.set(String::new());
            form_customset.set(String::new());
            form_advanced.set(false);
            form_busy.set(false);
            result_key.set(None);
            create_open.set(true);
        }
    };

    // Switching key type re-seeds the group picker so it never points at a
    // group of the wrong kind (progressive disclosure, no stale id).
    let select_type = {
        let lookups = lookups.clone();
        move |t: i64| {
            form_type.set(t);
            let first = if t == 0 {
                lookups.server_groups.first().map(|g| g.sgid)
            } else {
                lookups.channel_groups.first().map(|g| g.cgid)
            };
            form_group.set(first.unwrap_or(0));
        }
    };

    let submit_create = {
        let gate = gate.clone();
        move |_| {
            if *form_busy.read() {
                return;
            }
            let token_type = *form_type.read();
            let token_id1 = *form_group.read();
            if token_id1 == 0 {
                toaster.push(
                    ToastVariant::Warning,
                    "Pick a group",
                    Some("Select the group this key grants.".into()),
                );
                return;
            }
            let token_id2 = if token_type == 1 {
                *form_channel.read()
            } else {
                0
            };
            if token_type == 1 && token_id2 == 0 {
                toaster.push(
                    ToastVariant::Warning,
                    "Pick a channel",
                    Some("A channel-group key needs a target channel.".into()),
                );
                return;
            }
            let req = TokenCreateRequest {
                token_type,
                token_id1,
                token_id2,
                description: trim_to_option(&form_description.read()),
                customset: trim_to_option(&form_customset.read()),
            };
            let gate = gate.clone();
            form_busy.set(true);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/tokens");
                let res = api::authorized_post_json::<_, TokenCreated>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                form_busy.set(false);
                match res {
                    Ok(TokenCreated { token }) => {
                        // Peak-End: the flow ends on the copyable artifact,
                        // not a silent list refresh.
                        result_key.set(Some(token));
                        tokens_resource.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not create key",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let confirm_delete = {
        let gate = gate.clone();
        move |_| {
            if *delete_busy.read() {
                return;
            }
            let Some(target) = delete_target.read().clone() else {
                return;
            };
            let gate = gate.clone();
            delete_busy.set(true);
            spawn(async move {
                // Keys may contain `/`, `+`, `=`; percent-encode so the key
                // stays a single path segment (see `tokens.rs` route doc).
                let encoded = urlencoding::encode(&target.token);
                let path = format!("/api/servers/{server_id}/vs/{sid}/tokens/{encoded}");
                let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
                delete_busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Privilege key deleted", None);
                        delete_target.set(None);
                        tokens_resource.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not delete key",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let is_loaded = *loaded.read();
    let rows = tokens.read().clone();
    let load_error = error.read().clone();

    rsx! {
        div { class: "crumb", "Moderation · {server_name}" }
        div { class: "page-header",
            h1 { "Privilege keys" }
            if is_admin {
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: open_create.clone(),
                    "+ Create key"
                }
            }
        }
        p { class: "muted",
            "One-time codes a client redeems to join a server or channel group."
        }

        // ── the four list states ──────────────────────────────────────
        if !is_loaded {
            div { class: "empty",
                span { role: "status", "aria-live": "polite", "Loading privilege keys…" }
            }
        } else if let Some(err) = load_error.as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load privilege keys".to_string(),
                "{format_error(err)}"
            }
        } else if rows.is_empty() {
            div { class: "empty",
                div { class: "icon", "🔑" }
                h3 { "No privilege keys" }
                p { "Create one to let a client join a group by entering a code." }
                if is_admin {
                    div { class: "actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: open_create.clone(),
                            "Create key"
                        }
                    }
                }
            }
        } else {
            table { class: "data-table", "aria-label": "Privilege keys",
                thead {
                    tr {
                        th { scope: "col", "Description" }
                        th { scope: "col", "Grants" }
                        th { scope: "col", "Key" }
                        th { scope: "col", "Created" }
                        if is_admin {
                            th { scope: "col", class: "actions-col", "Actions" }
                        }
                    }
                }
                tbody {
                    for r in rows.iter() {
                        {
                            let r = r.clone();
                            let token = r.token.clone();
                            let is_revealed = revealed.read().contains(&token);
                            let (grant, channel) = grants_label(&r, &lookups);
                            let copy_token = token.clone();
                            let toggle_token = token.clone();
                            let del_row = r.clone();
                            rsx! {
                                tr { key: "{token}",
                                    td {
                                        if r.token_description.trim().is_empty() {
                                            span { class: "muted", "—" }
                                        } else {
                                            "{r.token_description}"
                                        }
                                    }
                                    td {
                                        div { "{grant}" }
                                        if let Some(ch) = channel {
                                            div { class: "muted", "@ {ch}" }
                                        }
                                    }
                                    td {
                                        if is_revealed {
                                            code { class: "mono", "{token}" }
                                        } else {
                                            span { class: "token-key-masked", "key ••••••••" }
                                        }
                                        div { class: "token-key-actions",
                                            button {
                                                r#type: "button",
                                                class: "btn btn-link btn-sm",
                                                onclick: move |_| {
                                                    copy_to_clipboard(&copy_token);
                                                    toaster
                                                        .push(ToastVariant::Success, "Key copied", None);
                                                },
                                                "Copy"
                                            }
                                            button {
                                                r#type: "button",
                                                class: "btn btn-link btn-sm",
                                                "aria-pressed": if is_revealed { "true" } else { "false" },
                                                onclick: move |_| {
                                                    let mut set = revealed.write();
                                                    if set.contains(&toggle_token) {
                                                        set.remove(&toggle_token);
                                                    } else {
                                                        set.insert(toggle_token.clone());
                                                    }
                                                },
                                                if is_revealed { "Hide" } else { "Reveal" }
                                            }
                                        }
                                    }
                                    td { "{relative_from_unix(r.token_created)}" }
                                    if is_admin {
                                        td { class: "actions-col",
                                            Button {
                                                variant: ButtonVariant::Danger,
                                                size: ButtonSize::Small,
                                                onclick: move |_| delete_target.set(Some(del_row.clone())),
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
        }

        // ── create modal ──────────────────────────────────────────────
        if *create_open.read() {
            CreateKeyModal {
                lookups: lookups.clone(),
                form_type,
                form_group,
                form_channel,
                form_description,
                form_customset,
                form_advanced,
                busy: *form_busy.read(),
                result: result_key.read().clone(),
                on_select_type: EventHandler::new(select_type),
                on_submit: EventHandler::new(submit_create),
                on_close: EventHandler::new(move |_| {
                    create_open.set(false);
                    result_key.set(None);
                }),
            }
        }

        // ── delete confirmation (destructive) ──────────────────────────
        if let Some(target) = delete_target.read().clone() {
            div {
                class: "modal-backdrop",
                onclick: move |_| {
                    if !*delete_busy.read() {
                        delete_target.set(None);
                    }
                },
                onkeydown: move |evt| {
                    if evt.key() == Key::Escape && !*delete_busy.read() {
                        evt.prevent_default();
                        delete_target.set(None);
                    }
                },
                div {
                    class: "modal modal-sm",
                    role: "alertdialog",
                    "aria-modal": "true",
                    "aria-labelledby": "token-delete-title",
                    "aria-describedby": "token-delete-body",
                    onclick: move |evt| evt.stop_propagation(),
                    div { class: "modal-header",
                        h2 { id: "token-delete-title", "Delete privilege key?" }
                    }
                    div { class: "modal-body",
                        p { id: "token-delete-body",
                            if target.token_description.trim().is_empty() {
                                "Anyone who already has this key can no longer redeem it; clients who redeemed it keep their group."
                            } else {
                                "Anyone who already has the “{target.token_description}” key can no longer redeem it; clients who redeemed it keep their group."
                            }
                        }
                    }
                    div { class: "modal-footer",
                        button {
                            r#type: "button",
                            class: "btn btn-ghost",
                            autofocus: true,
                            disabled: *delete_busy.read(),
                            onclick: move |_| delete_target.set(None),
                            "Cancel"
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-danger",
                            disabled: *delete_busy.read(),
                            onclick: confirm_delete.clone(),
                            if *delete_busy.read() { "Deleting…" } else { "Delete key" }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CreateKeyModalProps {
    lookups: LookupData,
    form_type: Signal<i64>,
    form_group: Signal<i64>,
    form_channel: Signal<i64>,
    form_description: Signal<String>,
    form_customset: Signal<String>,
    form_advanced: Signal<bool>,
    busy: bool,
    /// `Some` once the key has been minted — swaps the modal to the
    /// copyable result state.
    result: Option<String>,
    on_select_type: EventHandler<i64>,
    on_submit: EventHandler<()>,
    on_close: EventHandler<()>,
}

/// The create-key modal — a progressive single-column form that swaps to a
/// copyable result state on success (UX brief §7.2).
#[component]
fn CreateKeyModal(props: CreateKeyModalProps) -> Element {
    let mut form_group = props.form_group;
    let mut form_channel = props.form_channel;
    let mut form_description = props.form_description;
    let mut form_customset = props.form_customset;
    let mut form_advanced = props.form_advanced;
    let busy = props.busy;
    let on_close = props.on_close;
    let on_submit = props.on_submit;
    let on_select_type = props.on_select_type;

    let key_type = *props.form_type.read();
    let lookups = props.lookups.clone();
    let has_groups = if key_type == 0 {
        !lookups.server_groups.is_empty()
    } else {
        !lookups.channel_groups.is_empty()
    };

    rsx! {
        div {
            class: "modal-backdrop",
            onclick: move |_| {
                if !busy {
                    on_close.call(());
                }
            },
            onkeydown: move |evt| {
                if evt.key() == Key::Escape && !busy {
                    evt.prevent_default();
                    on_close.call(());
                }
            },
            div {
                class: "modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "token-create-title",
                onclick: move |evt| evt.stop_propagation(),

                if let Some(key) = props.result.as_ref() {
                    // ── result state — the copyable artifact ───────────
                    div { class: "modal-header",
                        h2 { id: "token-create-title", "Privilege key created" }
                    }
                    div { class: "modal-body stack-md",
                        div { class: "token-result-key",
                            code { class: "mono", "{key}" }
                        }
                        div { class: "token-result-copy",
                            Button {
                                variant: ButtonVariant::Primary,
                                onclick: {
                                    let key = key.clone();
                                    move |_| copy_to_clipboard(&key)
                                },
                                "Copy key"
                            }
                        }
                        Banner { variant: BannerVariant::Info,
                            "Copy this key now and share it over a secure channel. You will not be able to read it in full again."
                        }
                    }
                    div { class: "modal-footer",
                        button {
                            r#type: "button",
                            class: "btn btn-primary",
                            onclick: move |_| on_close.call(()),
                            "Done"
                        }
                    }
                } else {
                    // ── form state ─────────────────────────────────────
                    div { class: "modal-header",
                        h2 { id: "token-create-title", "Create privilege key" }
                    }
                    form {
                        onsubmit: move |evt| {
                            evt.prevent_default();
                            on_submit.call(());
                        },
                        div { class: "modal-body stack-md",
                            fieldset { class: "form-row",
                                legend { "Key type" }
                                label { class: "field-inline",
                                    input {
                                        r#type: "radio",
                                        name: "token-type",
                                        checked: key_type == 0,
                                        onchange: move |_| on_select_type.call(0),
                                    }
                                    span { "Server group" }
                                }
                                label { class: "field-inline",
                                    input {
                                        r#type: "radio",
                                        name: "token-type",
                                        checked: key_type == 1,
                                        onchange: move |_| on_select_type.call(1),
                                    }
                                    span { "Channel group" }
                                }
                            }

                            div { class: "form-row",
                                label { r#for: "token-group", "Group" }
                                if has_groups {
                                    select {
                                        id: "token-group",
                                        class: "input",
                                        value: "{form_group.read()}",
                                        onchange: move |e| {
                                            form_group.set(e.value().parse().unwrap_or(0));
                                        },
                                        if key_type == 0 {
                                            for g in lookups.server_groups.iter() {
                                                option { key: "{g.sgid}", value: "{g.sgid}", "{g.name}" }
                                            }
                                        } else {
                                            for g in lookups.channel_groups.iter() {
                                                option { key: "{g.cgid}", value: "{g.cgid}", "{g.name}" }
                                            }
                                        }
                                    }
                                } else {
                                    p { class: "muted",
                                        "No groups available for this server."
                                    }
                                }
                            }

                            // Channel picker — only a channel-group key
                            // needs a target channel (`tokenid2 = cid`).
                            if key_type == 1 {
                                div { class: "form-row",
                                    label { r#for: "token-channel", "Channel" }
                                    select {
                                        id: "token-channel",
                                        class: "input",
                                        value: "{form_channel.read()}",
                                        onchange: move |e| {
                                            form_channel.set(e.value().parse().unwrap_or(0));
                                        },
                                        for c in lookups.channels.iter() {
                                            option { key: "{c.cid}", value: "{c.cid}", "# {c.channel_name}" }
                                        }
                                    }
                                }
                            }

                            div { class: "form-row",
                                label { r#for: "token-desc", "Description (optional)" }
                                input {
                                    id: "token-desc",
                                    class: "input",
                                    placeholder: "e.g. Staff onboarding",
                                    value: "{form_description.read()}",
                                    oninput: move |e| form_description.set(e.value()),
                                }
                            }

                            details {
                                open: *form_advanced.read(),
                                ontoggle: move |_| {
                                    let now = *form_advanced.peek();
                                    form_advanced.set(!now);
                                },
                                summary { "Advanced — custom client properties" }
                                div { class: "form-row",
                                    label { r#for: "token-customset",
                                        "Custom set (tokencustomset)"
                                    }
                                    input {
                                        id: "token-customset",
                                        class: "input",
                                        placeholder: "ident=value|ident=value",
                                        value: "{form_customset.read()}",
                                        oninput: move |e| form_customset.set(e.value()),
                                    }
                                    p { class: "muted field-hint",
                                        "Client properties applied when the key is redeemed. Leave blank unless you need them."
                                    }
                                }
                            }
                        }
                        div { class: "modal-footer",
                            button {
                                r#type: "button",
                                class: "btn btn-ghost",
                                disabled: busy,
                                onclick: move |_| on_close.call(()),
                                "Cancel"
                            }
                            Button {
                                variant: ButtonVariant::Primary,
                                kind: ButtonType::Submit,
                                loading: busy,
                                disabled: !has_groups,
                                "Create key"
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── data fetch ──────────────────────────────────────────────────────────

async fn fetch_tokens(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<TokenItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/tokens");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

/// Fetch the group + channel names for the GRANTS cell and the create
/// pickers. Each sub-fetch is independent: a failure leaves that vec empty
/// so the page still renders (GRANTS degrades to the raw id).
async fn fetch_lookups(gate: Arc<RefreshGate>, config_id: i64, sid: i64) -> LookupData {
    let base = api::api_base();
    let server_groups = api::authorized_get_json::<Vec<ServerGroupItem>>(
        &gate,
        &base,
        &format!("/api/servers/{config_id}/vs/{sid}/server-groups"),
    )
    .await
    .unwrap_or_default();
    let channel_groups = api::authorized_get_json::<Vec<ChannelGroupItem>>(
        &gate,
        &base,
        &format!("/api/servers/{config_id}/vs/{sid}/channel-groups"),
    )
    .await
    .unwrap_or_default();
    let channels = api::authorized_get_json::<Vec<ChannelTreeNode>>(
        &gate,
        &base,
        &format!("/api/servers/{config_id}/vs/{sid}/channels"),
    )
    .await
    .unwrap_or_default();
    LookupData {
        server_groups,
        channel_groups,
        channels,
    }
}

// ── presentation helpers ────────────────────────────────────────────────

/// Decode a token's `token_type` + ids into operator-facing GRANTS text.
/// Returns `(primary, channel)` — `channel` is `Some` only for a
/// channel-group key (`token_type == 1`). The operator never sees a raw
/// `token_type` integer (UX brief §7.1, Plain Language).
fn grants_label(token: &TokenItem, lookups: &LookupData) -> (String, Option<String>) {
    match token.token_type {
        0 => {
            let name = lookups
                .server_groups
                .iter()
                .find(|g| g.sgid == token.token_id1)
                .map(|g| g.name.clone())
                .unwrap_or_else(|| format!("#{}", token.token_id1));
            (format!("Server group · {name}"), None)
        }
        1 => {
            let name = lookups
                .channel_groups
                .iter()
                .find(|g| g.cgid == token.token_id1)
                .map(|g| g.name.clone())
                .unwrap_or_else(|| format!("#{}", token.token_id1));
            let channel = lookups
                .channels
                .iter()
                .find(|c| c.cid == token.token_id2)
                .map(|c| c.channel_name.clone())
                .unwrap_or_else(|| format!("channel #{}", token.token_id2));
            (format!("Channel group · {name}"), Some(channel))
        }
        other => (format!("Unknown key type ({other})"), None),
    }
}

fn trim_to_option(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Best-effort copy of `text` to the system clipboard. No-op off the
/// browser (SSR / unit tests). Mirrors `ui::pages::widgets::copy_to_clipboard`.
fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(token_type: i64, id1: i64, id2: i64) -> TokenItem {
        TokenItem {
            token: "abc".into(),
            token_type,
            token_id1: id1,
            token_id2: id2,
            token_description: String::new(),
            token_created: 0,
            token_customset: String::new(),
        }
    }

    #[test]
    fn grants_label_decodes_server_group() {
        let lookups = LookupData {
            server_groups: vec![ServerGroupItem {
                sgid: 6,
                name: "Server Admin".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (primary, channel) = grants_label(&token(0, 6, 0), &lookups);
        assert_eq!(primary, "Server group · Server Admin");
        assert!(channel.is_none());
    }

    #[test]
    fn grants_label_decodes_channel_group_with_channel() {
        let lookups = LookupData {
            channel_groups: vec![ChannelGroupItem {
                cgid: 9,
                name: "Operator".into(),
                ..Default::default()
            }],
            channels: vec![ChannelTreeNode {
                cid: 3,
                channel_name: "Lobby".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (primary, channel) = grants_label(&token(1, 9, 3), &lookups);
        assert_eq!(primary, "Channel group · Operator");
        assert_eq!(channel.as_deref(), Some("Lobby"));
    }

    #[test]
    fn grants_label_degrades_to_raw_id_when_unresolved() {
        let (primary, _) = grants_label(&token(0, 42, 0), &LookupData::default());
        assert_eq!(primary, "Server group · #42");
    }

    #[test]
    fn grants_label_handles_unknown_type() {
        let (primary, channel) = grants_label(&token(7, 1, 0), &LookupData::default());
        assert!(primary.starts_with("Unknown key type"));
        assert!(channel.is_none());
    }

    #[test]
    fn trim_to_option_collapses_blank() {
        assert_eq!(trim_to_option("   "), None);
        assert_eq!(trim_to_option(" hi "), Some("hi".into()));
    }
}
