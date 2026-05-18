//! `/moderation/messages` — TS6 offline-message inbox. PURA-377, Phase B of
//! the [PURA-369](/PURA/issues/PURA-369) moderation-completion plan; spec is
//! the PURA-371 UX brief §8.
//!
//! Backed by `messagelist` / `messageget` / `messageadd` / `messagedel` via
//! the `routes/control/messages.rs` REST module (PURA-373). Reading the inbox
//! needs server access; composing and deleting are admin-only — the write
//! affordances are suppressed for non-admin sessions, and the route layer
//! re-checks the role server-side regardless (this gate is cosmetic).
//!
//! ## Sender display
//!
//! `messagelist` / `messageget` carry only the sender's `cluid` (unique
//! identifier) — there is no nickname on the wire. Offline-message senders
//! are by definition usually offline, so a `clientlist` cross-reference would
//! resolve nothing most of the time. The inbox therefore renders the `cluid`
//! verbatim as the sender; this is the honest, complete data we have.
//!
//! ## Layout
//!
//! Desktop is a side-by-side inbox list + reading pane. Mobile collapses to a
//! single column: the inbox is the page, and selecting a row swaps in a
//! full-screen reading view with a back affordance. Both are pure CSS off the
//! `.msg-layout.is-reading` modifier — see `components.css`.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ClientListItem, MessageCreateRequest, MessageDetailResponse, MessageListItem,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::dropdown::{
    Dropdown, Menu, MenuEmpty, MenuFilter, MenuItem, MenuItemKind,
};
use crate::ui::components::toast::{use_toaster, ToastVariant};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, Field,
};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

use super::format_error;

#[component]
pub fn MessagesPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        // AppShell bounces anon sessions to /login; render nothing so there
        // is no flash of operator chrome.
        return rsx! { "" };
    }

    // Read = server access (any operator); write = admin. The page renders
    // for every authenticated session — only the compose/delete affordances
    // are gated. The `/api/.../messages` POST + DELETE re-check admin.
    let role = session
        .state
        .read()
        .user()
        .map(|u| u.role.clone())
        .unwrap_or_default();
    let is_admin = role.eq_ignore_ascii_case("admin");

    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let hub = use_ws_hub();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Messages" }
            h1 { "Messages" }
            div { class: "empty",
                div { class: "icon", "✉" }
                h3 { "No server selected" }
                p { "Add a server to read its offline-message inbox." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut messages_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_messages(gate, server_id, sid).await }
        }
    });
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut messages: Signal<Vec<MessageListItem>> = use_signal(Vec::new);
    {
        use_effect(move || match &*messages_resource.read_unchecked() {
            Some(Ok(rows)) => {
                messages.set(rows.clone());
                error.set(None);
            }
            Some(Err(e)) => error.set(Some(e.clone())),
            None => {}
        });
    }

    // WS — message mutations publish on the per-server `moderation` topic
    // (`publish_moderation` in routes/control/mod.rs).
    {
        let hub = hub.clone();
        let _resource = use_resource(move || {
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
                    if matches!(
                        env.kind.as_str(),
                        "ts:message:created" | "ts:message:deleted"
                    ) {
                        messages_resource.restart();
                    }
                }
            }
        });
    }

    // Reading-pane state. `selected` is the open msgid; `detail` is its
    // fetched body. Both clear together on delete / back.
    let mut selected: Signal<Option<i64>> = use_signal(|| None::<i64>);
    let mut detail: Signal<Option<MessageDetailResponse>> = use_signal(|| None);
    let mut detail_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut detail_loading: Signal<bool> = use_signal(|| false);

    let mut compose_open: Signal<bool> = use_signal(|| false);
    let mut confirm_delete: Signal<bool> = use_signal(|| false);
    let mut delete_busy: Signal<bool> = use_signal(|| false);

    // Open a message: select it immediately, then fetch the body. `messageget`
    // marks it read server-side, so we also flip the local `flag_read` to
    // clear the unread dot without a full inbox refetch.
    let open_message = {
        let gate = gate.clone();
        move |msgid: i64| {
            selected.set(Some(msgid));
            detail.set(None);
            detail_error.set(None);
            detail_loading.set(true);
            confirm_delete.set(false);
            let gate = gate.clone();
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/messages/{msgid}");
                let res = api::authorized_get_json::<MessageDetailResponse>(
                    &gate,
                    &api::api_base(),
                    &path,
                )
                .await;
                // Guard against an out-of-order response when the operator
                // clicked another row before this fetch landed.
                if *selected.peek() != Some(msgid) {
                    return;
                }
                detail_loading.set(false);
                match res {
                    Ok(d) => {
                        detail.set(Some(d));
                        let mut list = messages.write();
                        if let Some(row) = list.iter_mut().find(|m| m.msgid == msgid) {
                            row.flag_read = 1;
                        }
                    }
                    Err(e) => detail_error.set(Some(e)),
                }
            });
        }
    };

    let close_reading = move |_| {
        selected.set(None);
        detail.set(None);
        detail_error.set(None);
        confirm_delete.set(false);
    };

    let do_delete = {
        let gate = gate.clone();
        move |_| {
            let Some(msgid) = *selected.peek() else {
                return;
            };
            if *delete_busy.peek() {
                return;
            }
            delete_busy.set(true);
            let gate = gate.clone();
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/messages/{msgid}");
                let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
                delete_busy.set(false);
                match res {
                    Ok(()) => {
                        confirm_delete.set(false);
                        selected.set(None);
                        detail.set(None);
                        toaster.push(ToastVariant::Success, "Message deleted", None);
                        messages_resource.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not delete message",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let snapshot = messages.read().clone();
    let is_loading = messages_resource.read_unchecked().is_none();
    let load_error = error.read().clone();
    let selected_id = *selected.read();
    let layout_class = if selected_id.is_some() {
        "msg-layout is-reading"
    } else {
        "msg-layout"
    };

    rsx! {
        div { class: "crumb", "Messages · {server_name}" }
        div { class: "msg-head",
            h1 { "Messages" }
            if is_admin {
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: move |_| compose_open.set(true),
                    "New message"
                }
            }
        }

        if let Some(err) = load_error.as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load messages".to_string(),
                "{format_error(err)}"
            }
        }

        if is_loading {
            div { class: "msg-loading", "Loading inbox…" }
        } else if load_error.is_none() && snapshot.is_empty() {
            div { class: "empty",
                div { class: "icon", "✉" }
                h3 { "No messages" }
                p { "Offline messages addressed to this operator identity land here." }
                if is_admin {
                    div { class: "actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: move |_| compose_open.set(true),
                            "New message"
                        }
                    }
                }
            }
        } else if load_error.is_none() {
            div { class: "{layout_class}",
                // ── Inbox list ──────────────────────────────────────────
                div { class: "msg-inbox",
                    ul { class: "msg-list", "aria-label": "Inbox",
                        for m in snapshot.iter() {
                            {
                                let m = m.clone();
                                let msgid = m.msgid;
                                let unread = m.flag_read == 0;
                                let is_sel = selected_id == Some(msgid);
                                let mut row_class = String::from("msg-row");
                                if unread { row_class.push_str(" is-unread"); }
                                if is_sel { row_class.push_str(" is-selected"); }
                                let open = open_message.clone();
                                let subject = if m.subject.trim().is_empty() {
                                    "(no subject)".to_string()
                                } else {
                                    m.subject.clone()
                                };
                                rsx! {
                                    li { key: "{msgid}",
                                        button {
                                            r#type: "button",
                                            class: "{row_class}",
                                            "aria-current": if is_sel { "true" } else { "false" },
                                            onclick: move |_| open.clone()(msgid),
                                            span {
                                                class: "msg-dot",
                                                aria_hidden: "true",
                                                "data-unread": "{unread}",
                                            }
                                            span { class: "msg-row-text",
                                                span { class: "msg-row-subject", "{subject}" }
                                                span { class: "msg-row-meta",
                                                    span { class: "msg-row-sender", "{m.cluid}" }
                                                    span { class: "msg-row-dotsep", aria_hidden: "true", "·" }
                                                    span { "{super::relative_from_unix(m.timestamp)}" }
                                                }
                                            }
                                            if unread {
                                                span { class: "sr-only", "Unread" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── Reading pane ────────────────────────────────────────
                div { class: "msg-reading",
                    if selected_id.is_none() {
                        div { class: "msg-reading-empty",
                            p { "Select a message to read it." }
                        }
                    } else if *detail_loading.read() {
                        button {
                            r#type: "button",
                            class: "msg-back",
                            onclick: close_reading,
                            "‹ Inbox"
                        }
                        div { class: "msg-loading", "Loading message…" }
                    } else if let Some(err) = detail_error.read().as_ref() {
                        button {
                            r#type: "button",
                            class: "msg-back",
                            onclick: close_reading,
                            "‹ Inbox"
                        }
                        Banner { variant: BannerVariant::Danger, title: "Could not open message".to_string(),
                            "{format_error(err)}"
                        }
                    } else if let Some(d) = detail.read().as_ref() {
                        button {
                            r#type: "button",
                            class: "msg-back",
                            onclick: close_reading,
                            "‹ Inbox"
                        }
                        div { class: "msg-reading-head",
                            h2 {
                                if d.subject.trim().is_empty() { "(no subject)" } else { "{d.subject}" }
                            }
                            p { class: "msg-reading-meta",
                                "From "
                                span { class: "msg-row-sender", "{d.cluid}" }
                                " · {super::relative_from_unix(d.timestamp)}"
                            }
                        }
                        div { class: "msg-reading-body",
                            if d.message.trim().is_empty() {
                                span { class: "muted", "(empty message body)" }
                            } else {
                                "{d.message}"
                            }
                        }
                        if is_admin {
                            div { class: "msg-reading-actions",
                                Button {
                                    variant: ButtonVariant::Danger,
                                    size: ButtonSize::Small,
                                    onclick: move |_| confirm_delete.set(true),
                                    "Delete"
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Delete confirmation — low-stakes `confirm` (spec §8.1) ───────
        if *confirm_delete.read() {
            DeleteConfirm {
                busy: *delete_busy.read(),
                on_cancel: move |_| { if !*delete_busy.peek() { confirm_delete.set(false); } },
                on_confirm: do_delete,
            }
        }

        // ── Compose ──────────────────────────────────────────────────────
        if *compose_open.read() {
            ComposeModal {
                config_id: server_id,
                sid,
                on_close: move |_| compose_open.set(false),
            }
        }
    }
}

/// Low-stakes delete confirmation. Deleting a *received* message only drops
/// the operator's own inbox copy, so this is a plain `confirm` (primary
/// button), not a destructive `btn-danger` escalation (spec §8.1).
#[component]
fn DeleteConfirm(busy: bool, on_cancel: EventHandler<()>, on_confirm: EventHandler<()>) -> Element {
    rsx! {
        div {
            class: "modal-backdrop",
            onclick: move |_| on_cancel.call(()),
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    evt.prevent_default();
                    on_cancel.call(());
                }
            },
            div {
                class: "modal modal-sm",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-labelledby": "msg-del-title",
                "aria-describedby": "msg-del-body",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-header",
                    h2 { id: "msg-del-title", "Delete message?" }
                }
                div { class: "modal-body",
                    p { id: "msg-del-body",
                        "This removes the message from your inbox. The sender is not notified."
                    }
                }
                div { class: "modal-footer",
                    button {
                        r#type: "button",
                        class: "btn btn-ghost",
                        autofocus: true,
                        disabled: busy,
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        loading: busy,
                        onclick: move |_| on_confirm.call(()),
                        "Delete"
                    }
                }
            }
        }
    }
}

/// Compose-a-message modal. Recipient is a TS6 client **UID** — opaque and
/// unmemorable — so the primary affordance is a client picker that resolves a
/// nickname to its UID. A raw-UID escape-hatch field stays editable for
/// recipients not in the online client list (Postel's Law — spec §8.2).
#[component]
fn ComposeModal(config_id: i64, sid: i64, on_close: EventHandler<()>) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    // Online client list — the picker source. Most offline-message recipients
    // are *offline* (hence the escape-hatch field), so a miss is expected.
    let clients_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_clients(gate, config_id, sid).await }
        }
    });

    let mut recipient_uid: Signal<String> = use_signal(String::new);
    let mut recipient_name: Signal<Option<String>> = use_signal(|| None::<String>);
    let mut subject: Signal<String> = use_signal(String::new);
    let mut body: Signal<String> = use_signal(String::new);
    let mut busy: Signal<bool> = use_signal(|| false);
    let mut send_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);

    // Picker dropdown state.
    let picker_open: Signal<bool> = use_signal(|| false);
    let picker_active: Signal<Option<String>> = use_signal(|| None::<String>);
    let mut picker_filter: Signal<String> = use_signal(String::new);

    let can_send = !recipient_uid.read().trim().is_empty()
        && !subject.read().trim().is_empty()
        && !body.read().trim().is_empty();

    let on_submit = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() || !can_send {
                return;
            }
            let req = MessageCreateRequest {
                cluid: recipient_uid.read().trim().to_string(),
                subject: subject.read().trim().to_string(),
                message: body.read().to_string(),
            };
            busy.set(true);
            send_error.set(None);
            let gate = gate.clone();
            spawn(async move {
                let path = format!("/api/servers/{config_id}/vs/{sid}/messages");
                let res = api::authorized_post_json::<MessageCreateRequest, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Message sent", None);
                        on_close.call(());
                    }
                    Err(e) => send_error.set(Some(e)),
                }
            });
        }
    };

    // Filter the picker rows. `client_type == 0` keeps voice clients only —
    // ServerQuery sessions are not message recipients.
    let clients_snapshot: Vec<ClientListItem> = match &*clients_resource.read_unchecked() {
        Some(Ok(rows)) => rows
            .iter()
            .filter(|c| c.client_type == 0)
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let filter_text = picker_filter.read().to_lowercase();
    let filtered: Vec<ClientListItem> = clients_snapshot
        .iter()
        .filter(|c| {
            filter_text.is_empty() || c.client_nickname.to_lowercase().contains(&filter_text)
        })
        .cloned()
        .collect();
    let clients_failed = matches!(&*clients_resource.read_unchecked(), Some(Err(_)));
    let chosen_caption = recipient_name.read().clone();

    rsx! {
        div {
            class: "modal-backdrop",
            onclick: move |_| { if !*busy.peek() { on_close.call(()); } },
            onkeydown: move |evt| {
                if evt.key() == Key::Escape && !*busy.peek() {
                    evt.prevent_default();
                    on_close.call(());
                }
            },
            div {
                class: "modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "msg-compose-title",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-header",
                    h2 { id: "msg-compose-title", "New message" }
                }
                form {
                    onsubmit: {
                        let on_submit = on_submit.clone();
                        move |evt: FormEvent| { evt.prevent_default(); on_submit.clone()(()); }
                    },
                    div { class: "modal-body stack-md",
                        if let Some(err) = send_error.read().as_ref() {
                            Banner { variant: BannerVariant::Danger, title: "Could not send message".to_string(),
                                "{format_error(err)}"
                            }
                        }

                        // ── Recipient ────────────────────────────────────
                        Field {
                            label: "Recipient".to_string(),
                            id: "msg-recipient".to_string(),
                            required: true,
                            helper: "Pick an online client, or paste a UID for an offline recipient.".to_string(),
                            div { class: "msg-recipient-row",
                                Dropdown {
                                    trigger_id: "msg-recipient-trigger".to_string(),
                                    menu_id: "msg-recipient-menu".to_string(),
                                    open: picker_open,
                                    active_id: picker_active,
                                    trigger: rsx! {
                                        button {
                                            r#type: "button",
                                            id: "msg-recipient-trigger",
                                            class: "btn btn-secondary",
                                            "aria-haspopup": "menu",
                                            "aria-expanded": if *picker_open.read() { "true" } else { "false" },
                                            "aria-controls": "msg-recipient-menu",
                                            onclick: {
                                                let mut picker_open = picker_open;
                                                move |_| {
                                                    let next = !*picker_open.peek();
                                                    if next { picker_filter.set(String::new()); }
                                                    picker_open.set(next);
                                                }
                                            },
                                            "Pick a client ▾"
                                        }
                                    },
                                    Menu {
                                        id: "msg-recipient-menu".to_string(),
                                        labelled_by: "msg-recipient-trigger".to_string(),
                                        active_id: picker_active.read().clone(),
                                        MenuFilter {
                                            value: picker_filter,
                                            placeholder: "Filter clients…".to_string(),
                                            aria_label: "Filter clients".to_string(),
                                        }
                                        if clients_failed {
                                            MenuEmpty { text: "Client list unavailable — paste a UID below.".to_string() }
                                        } else if clients_snapshot.is_empty() {
                                            MenuEmpty { text: "No clients online — paste a UID below.".to_string() }
                                        } else if filtered.is_empty() {
                                            MenuEmpty { text: "No clients match the filter.".to_string() }
                                        } else {
                                            for c in filtered.iter() {
                                                {
                                                    let c = c.clone();
                                                    let item_id = format!("msg-cli-{}", c.client_database_id);
                                                    let uid = c.client_unique_identifier.clone();
                                                    let nick = c.client_nickname.clone();
                                                    rsx! {
                                                        MenuItem {
                                                            key: "{item_id}",
                                                            id: item_id.clone(),
                                                            kind: MenuItemKind::Action,
                                                            rich: true,
                                                            onselect: move |_| {
                                                                recipient_uid.set(uid.clone());
                                                                recipient_name.set(Some(nick.clone()));
                                                            },
                                                            span { class: "msg-pick-nick", "{c.client_nickname}" }
                                                            span { class: "msg-pick-uid", "{c.client_unique_identifier}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                input {
                                    id: "msg-recipient",
                                    class: "input",
                                    r#type: "text",
                                    placeholder: "Client UID",
                                    value: "{recipient_uid.read()}",
                                    required: true,
                                    oninput: move |e| {
                                        recipient_uid.set(e.value());
                                        // A hand-edited UID no longer matches
                                        // the picked nickname caption.
                                        recipient_name.set(None);
                                    },
                                }
                            }
                            if let Some(name) = chosen_caption.as_ref() {
                                p { class: "msg-recipient-pick",
                                    "Sending to "
                                    strong { "{name}" }
                                }
                            }
                        }

                        // ── Subject ──────────────────────────────────────
                        Field {
                            label: "Subject".to_string(),
                            id: "msg-subject".to_string(),
                            required: true,
                            input {
                                id: "msg-subject",
                                class: "input",
                                r#type: "text",
                                placeholder: "Message subject",
                                value: "{subject.read()}",
                                required: true,
                                oninput: move |e| subject.set(e.value()),
                            }
                        }

                        // ── Body ─────────────────────────────────────────
                        Field {
                            label: "Message".to_string(),
                            id: "msg-body".to_string(),
                            required: true,
                            textarea {
                                id: "msg-body",
                                class: "input",
                                rows: "5",
                                placeholder: "Write your message…",
                                value: "{body.read()}",
                                required: true,
                                oninput: move |e| body.set(e.value()),
                            }
                        }
                    }
                    div { class: "modal-footer",
                        button {
                            r#type: "button",
                            class: "btn btn-ghost",
                            disabled: *busy.read(),
                            onclick: move |_| { if !*busy.peek() { on_close.call(()); } },
                            "Cancel"
                        }
                        Button {
                            variant: ButtonVariant::Primary,
                            kind: ButtonType::Submit,
                            loading: *busy.read(),
                            disabled: !can_send,
                            "Send message"
                        }
                    }
                }
            }
        }
    }
}

async fn fetch_messages(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<MessageListItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/messages");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

async fn fetch_clients(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<ClientListItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/clients");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}
