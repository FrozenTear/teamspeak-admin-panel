//! `/moderation/channel-groups` — channel-group list. PURA-378 (UX brief §5).
//!
//! A structural sibling of [`super::server_groups`]: `GET channelgrouplist`
//! rendered as a `data-table` with per-row member / permission counts and a
//! New-group modal. The one real difference is the membership model —
//! channel-group membership is **per-channel** (`channelgroupclientlist`
//! returns `(cid, cldbid)` pairs), so the detail page's Members tab carries
//! a Channel column. The copy is explicit about this because operators
//! carry a server-group mental model where a group is server-wide.
//!
//! TS6 channel groups have no `channelgroupcopy` command, so unlike server
//! groups there is no copy-on-create and no Duplicate affordance.
//!
//! Gating (UX brief §2.1): **read** is any operator with access to the
//! selected server; **write** (create / delete) is admin-only — the create
//! button and per-row delete are suppressed for non-admins and the
//! `/api/.../channel-groups` routes re-check `check_admin` server-side.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ChannelGroupClientItem, ChannelGroupItem, GroupCreateRequest, GroupPermItem,
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
use crate::ui::routes::Route;

use super::format_error;
use super::server_groups::{group_type_label, group_type_protected};

#[component]
pub fn ChannelGroupsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let hub = use_ws_hub();
    let servers_ctx = use_servers_context();

    let is_admin = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin"))
        .unwrap_or(false);

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Moderation · Channel groups" }
            h1 { "Channel groups" }
            div { class: "empty",
                div { class: "icon", "⚑" }
                h3 { "No server selected" }
                p { "Select a server to manage its channel permission groups." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut groups_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_groups(gate, server_id, sid).await }
        }
    });

    // WS — channel-group lifecycle events publish on the per-server
    // `moderation` topic (routes/control/channel_groups.rs).
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
                    if env.kind.starts_with("ts:channel_group:") {
                        groups_res.restart();
                    }
                }
            }
        });
    }

    let mut modal_open = use_signal(|| false);
    let mut delete_target: Signal<Option<ChannelGroupItem>> = use_signal(|| None);

    let groups_snapshot = groups_res.read().clone();

    rsx! {
        div { class: "crumb", "Moderation · Channel groups · {server_name}" }
        div { class: "mod-panel-head",
            h1 { "Channel groups" }
            if is_admin {
                Button {
                    variant: ButtonVariant::Primary,
                    size: ButtonSize::Small,
                    onclick: move |_| modal_open.set(true),
                    "+ New group"
                }
            }
        }
        p { class: "info-hint",
            "Permission groups that apply inside a channel. Unlike a server group, a client's channel group is scoped to one channel — they can hold a different channel group in every channel they enter."
        }

        if !is_admin {
            Banner {
                variant: BannerVariant::Info,
                title: "Read-only".to_string(),
                "Channel-group and permission changes require an admin account."
            }
        }

        match groups_snapshot {
            None => rsx! {
                table { class: "data-table",
                    thead {
                        tr {
                            th { scope: "col", "Name" }
                            th { scope: "col", "Type" }
                            th { scope: "col", "Members" }
                            th { scope: "col", "Permissions" }
                            th { scope: "col", class: "actions-col", "" }
                        }
                    }
                    tbody {
                        for i in 0..4 {
                            tr { key: "{i}",
                                td { colspan: "5", div { class: "skeleton sg-skeleton-row" } }
                            }
                        }
                    }
                }
            },
            Some(Err(e)) => rsx! {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load channel groups".to_string(),
                    "{format_error(&e)}"
                    div { class: "perm-editor-retry",
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            onclick: move |_| groups_res.restart(),
                            "Retry"
                        }
                    }
                }
            },
            Some(Ok(groups)) if groups.is_empty() => rsx! {
                div { class: "empty",
                    div { class: "icon", "⚑" }
                    h3 { "No channel groups" }
                    p { "This server has no channel permission groups. TeamSpeak normally ships default channel groups — create one to get started." }
                    if is_admin {
                        Button {
                            variant: ButtonVariant::Primary,
                            size: ButtonSize::Small,
                            onclick: move |_| modal_open.set(true),
                            "+ New group"
                        }
                    }
                }
            },
            Some(Ok(groups)) => rsx! {
                table { class: "data-table",
                    "aria-label": "Channel groups",
                    thead {
                        tr {
                            th { scope: "col", "Name" }
                            th { scope: "col", "Type" }
                            th { scope: "col", class: "num-col", "Members" }
                            th { scope: "col", class: "num-col", "Permissions" }
                            th { scope: "col", class: "actions-col", "" }
                        }
                    }
                    tbody {
                        for g in groups.iter().cloned() {
                            GroupRow {
                                key: "{g.cgid}",
                                group: g.clone(),
                                server_id,
                                sid,
                                is_admin,
                                on_delete: EventHandler::new(move |grp| delete_target.set(Some(grp))),
                            }
                        }
                    }
                }
            },
        }

        if *modal_open.read() {
            NewChannelGroupModal {
                server_id,
                sid,
                on_close: EventHandler::new(move |_: ()| modal_open.set(false)),
                on_created: EventHandler::new(move |_: ()| {
                    modal_open.set(false);
                    groups_res.restart();
                }),
            }
        }

        if let Some(target) = delete_target.read().clone() {
            DeleteChannelGroupModal {
                server_id,
                sid,
                group: target,
                on_close: EventHandler::new(move |_: ()| delete_target.set(None)),
                on_deleted: EventHandler::new(move |_: ()| {
                    delete_target.set(None);
                    groups_res.restart();
                }),
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct GroupRowProps {
    group: ChannelGroupItem,
    server_id: i64,
    sid: i64,
    is_admin: bool,
    on_delete: EventHandler<ChannelGroupItem>,
}

/// One channel-group table row. Member / permission counts are fetched
/// per-row (the list endpoint carries neither) so each row owns its own
/// data and a slow count never blocks the table.
#[component]
fn GroupRow(props: GroupRowProps) -> Element {
    let nav = use_navigator();
    let gate = use_auth_gate();
    let g = props.group.clone();
    let cgid = g.cgid;
    let server_id = props.server_id;
    let sid = props.sid;
    let protected = group_type_protected(g.group_type);

    let counts = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_counts(gate, server_id, sid, cgid).await }
        }
    });

    let count_cell = |value: Option<usize>| match value {
        Some(n) => rsx! { "{n}" },
        None => rsx! { span { class: "muted", "—" } },
    };
    let (members, perms) = match counts.read().clone() {
        Some(Ok((m, p))) => (Some(m), Some(p)),
        _ => (None, None),
    };

    let go_detail = move |_| {
        nav.push(Route::ChannelGroupDetailPage { cgid });
    };
    let on_delete = props.on_delete;
    let group_for_delete = g.clone();

    rsx! {
        tr { key: "{cgid}", class: "sg-row",
            td {
                class: "sg-name-cell",
                role: "link",
                tabindex: "0",
                onclick: go_detail,
                onkeydown: move |e: KeyboardEvent| {
                    if matches!(e.key(), Key::Enter | Key::Character(_)) && e.key() == Key::Enter {
                        nav.push(Route::ChannelGroupDetailPage { cgid });
                    }
                },
                span { class: "sg-name", "{g.name}" }
                span { class: "sg-id mono", "cgid {cgid}" }
            }
            td {
                span { class: "tag tag-neutral", "{group_type_label(g.group_type)}" }
            }
            td { class: "num-col mono", {count_cell(members)} }
            td { class: "num-col mono", {count_cell(perms)} }
            td { class: "actions-col",
                Button {
                    variant: ButtonVariant::Ghost,
                    size: ButtonSize::Small,
                    onclick: go_detail,
                    "Open"
                }
                if props.is_admin {
                    Button {
                        variant: ButtonVariant::Danger,
                        size: ButtonSize::Small,
                        disabled: protected,
                        onclick: move |_| {
                            if !protected {
                                on_delete.call(group_for_delete.clone());
                            }
                        },
                        "Delete"
                    }
                }
            }
        }
    }
}

// ── New-group modal ─────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct NewChannelGroupModalProps {
    server_id: i64,
    sid: i64,
    on_close: EventHandler<()>,
    on_created: EventHandler<()>,
}

/// Create an empty channel group (`channelgroupadd`). TS6 has no
/// `channelgroupcopy`, so — unlike the server-group modal — there is no
/// copy-from picker; a new channel group always starts with no permissions.
#[component]
fn NewChannelGroupModal(props: NewChannelGroupModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_created = props.on_created;
    let server_id = props.server_id;
    let sid = props.sid;

    let mut name = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let on_submit = {
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *busy.peek() {
                return;
            }
            let group_name = name.peek().trim().to_string();
            if group_name.is_empty() {
                error.set(Some("Enter a name for the new channel group.".into()));
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            error.set(None);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/channel-groups");
                let body = GroupCreateRequest {
                    name: group_name.clone(),
                    r#type: None,
                };
                let res: Result<ts6_manager_shared::control::ChannelGroupCreated, ApiError> =
                    api::authorized_post_json(&gate, &api::api_base(), &path, Some(&body)).await;
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Created channel group “{group_name}”"),
                            None,
                        );
                        on_created.call(());
                    }
                    Err(e) => error.set(Some(format_error(&e))),
                }
            });
        }
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            form {
                class: "modal modal-sm",
                onclick: move |e| e.stop_propagation(),
                onsubmit: on_submit,
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "new-channel-group-title",
                div { class: "modal-header",
                    h2 { id: "new-channel-group-title", "New channel group" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "\u{2715}"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not create channel group".to_string(),
                            "{msg}"
                        }
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "new-channel-group-name", "Name" }
                        input {
                            id: "new-channel-group-name",
                            class: "input",
                            r#type: "text",
                            placeholder: "e.g. Channel Admin",
                            value: "{name.read()}",
                            disabled: *busy.read(),
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    p { class: "field-help",
                        "The new channel group starts empty. Add permissions on its detail page, then assign clients to it per channel."
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *busy.read(),
                        disabled: *busy.read(),
                        "Create group"
                    }
                }
            }
        }
    }
}

// ── Delete-group modal (type-to-confirm) ────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub(crate) struct DeleteChannelGroupModalProps {
    pub server_id: i64,
    pub sid: i64,
    pub group: ChannelGroupItem,
    pub on_close: EventHandler<()>,
    pub on_deleted: EventHandler<()>,
}

/// Destructive channel-group delete — type-to-confirm the group name.
/// Shared by the list page and the detail Settings danger zone. The delete
/// forwards `force=1`, so members are re-homed to the server default
/// channel group rather than blocking the delete.
#[component]
pub(crate) fn DeleteChannelGroupModal(props: DeleteChannelGroupModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_deleted = props.on_deleted;
    let group = props.group.clone();
    let cgid = group.cgid;
    let server_id = props.server_id;
    let sid = props.sid;
    let group_name = group.name.clone();

    let mut confirm = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let confirmed = confirm.read().trim() == group_name.trim() && !group_name.trim().is_empty();

    let on_delete = {
        let gate = gate.clone();
        let group_name = group_name.clone();
        move |_| {
            if *busy.peek() || !confirmed {
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            let group_name = group_name.clone();
            busy.set(true);
            error.set(None);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/channel-groups/{cgid}");
                let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Deleted channel group “{group_name}”"),
                            None,
                        );
                        on_deleted.call(());
                    }
                    Err(e) => error.set(Some(format_error(&e))),
                }
            });
        }
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            div {
                class: "modal modal-sm",
                onclick: move |e| e.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "delete-channel-group-title",
                div { class: "modal-header",
                    h2 { id: "delete-channel-group-title", "Delete channel group" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "\u{2715}"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not delete channel group".to_string(),
                            "{msg}"
                        }
                    }
                    Banner {
                        variant: BannerVariant::Warning,
                        title: "This cannot be undone".to_string(),
                        "Every client currently in this channel group falls back to the server's default channel group in the affected channels. The clients themselves are not removed from the server."
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "delete-channel-group-confirm",
                            "Type the group name "
                            strong { "{group_name}" }
                            " to confirm"
                        }
                        input {
                            id: "delete-channel-group-confirm",
                            class: "input",
                            r#type: "text",
                            autocomplete: "off",
                            value: "{confirm.read()}",
                            disabled: *busy.read(),
                            oninput: move |e| confirm.set(e.value()),
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        kind: ButtonType::Button,
                        disabled: !confirmed || *busy.read(),
                        loading: *busy.read(),
                        onclick: on_delete,
                        "Delete group"
                    }
                }
            }
        }
    }
}

// ── data helpers ────────────────────────────────────────────────────────

async fn fetch_groups(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<ChannelGroupItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channel-groups");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

/// Member + permission count for one channel group. Two reads; a failure of
/// either degrades that count to "—" rather than failing the whole row.
async fn fetch_counts(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    cgid: i64,
) -> Result<(usize, usize), ApiError> {
    let base = api::api_base();
    let clients_path = format!("/api/servers/{config_id}/vs/{sid}/channel-groups/{cgid}/clients");
    let perms_path = format!("/api/servers/{config_id}/vs/{sid}/channel-groups/{cgid}/permissions");
    let clients =
        api::authorized_get_json::<Vec<ChannelGroupClientItem>>(&gate, &base, &clients_path)
            .await?;
    let perms = api::authorized_get_json::<Vec<GroupPermItem>>(&gate, &base, &perms_path).await?;
    Ok((clients.len(), perms.len()))
}
