//! `/moderation/server-groups` — server-group list. PURA-375 (UX brief §4.1).
//!
//! `GET servergrouplist` rendered as a `data-table`: name, type pill, live
//! member / permission counts, and per-row actions. The whole row links
//! into the group detail page. A New-group modal creates an empty group or
//! copies an existing one's permissions (`servergroupadd` / `servergroupcopy`).
//!
//! Gating (UX brief §2.1): **read** is any operator with access to the
//! selected server; **write** (create / delete) is admin-only — the create
//! button and per-row delete are suppressed for non-admins and the
//! `/api/.../server-groups` routes re-check `check_admin` server-side.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    GroupCreateRequest, GroupPermItem, ServerGroupCopyRequest, ServerGroupCreated, ServerGroupItem,
    ServerGroupMember,
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

/// TS6 `PermissionGroupDBTypes` — server-group `type` field. `1` (Regular)
/// is the only operator-creatable kind; `0` (Template) and `2` (Query) are
/// server-managed and protected from deletion (UX brief §4.4).
pub(crate) fn group_type_label(t: i64) -> &'static str {
    match t {
        0 => "Template",
        1 => "Regular",
        2 => "Query",
        _ => "Unknown",
    }
}

/// `true` for the server-managed group types — their detail page renders,
/// but Delete is disabled (UX brief §4.4).
pub(crate) fn group_type_protected(t: i64) -> bool {
    t == 0 || t == 2
}

#[component]
pub fn ServerGroupsPage() -> Element {
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
            div { class: "crumb", "Moderation · Server groups" }
            h1 { "Server groups" }
            div { class: "empty",
                div { class: "icon", "⚐" }
                h3 { "No server selected" }
                p { "Select a server to manage its permission groups." }
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

    // WS — server-group lifecycle events publish on the per-server
    // `moderation` topic (routes/control/server_groups.rs).
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
                    if env.kind.starts_with("ts:server_group:") {
                        groups_res.restart();
                    }
                }
            }
        });
    }

    let mut modal_open = use_signal(|| false);
    let mut delete_target: Signal<Option<ServerGroupItem>> = use_signal(|| None);

    let groups_snapshot = groups_res.read().clone();

    rsx! {
        div { class: "crumb", "Moderation · Server groups · {server_name}" }
        div { class: "mod-panel-head",
            h1 { "Server groups" }
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
            "Permission groups assigned to clients on this server. A client holds the union of every group they are in."
        }

        if !is_admin {
            Banner {
                variant: BannerVariant::Info,
                title: "Read-only".to_string(),
                "Group and permission changes require an admin account."
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
                    title: "Could not load server groups".to_string(),
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
                    div { class: "icon", "⚐" }
                    h3 { "No server groups" }
                    p { "This server has no permission groups. TeamSpeak normally ships default groups — create one to get started." }
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
                    "aria-label": "Server groups",
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
                                key: "{g.sgid}",
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
            NewGroupModal {
                server_id,
                sid,
                existing: groups_res.read().clone().and_then(|r| r.ok()).unwrap_or_default(),
                on_close: EventHandler::new(move |_: ()| modal_open.set(false)),
                on_created: EventHandler::new(move |_: ()| {
                    modal_open.set(false);
                    groups_res.restart();
                }),
            }
        }

        if let Some(target) = delete_target.read().clone() {
            DeleteGroupModal {
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
    group: ServerGroupItem,
    server_id: i64,
    sid: i64,
    is_admin: bool,
    on_delete: EventHandler<ServerGroupItem>,
}

/// One server-group table row. Member / permission counts are fetched
/// per-row (the list endpoint carries neither) so each row owns its own
/// data and a slow count never blocks the table.
#[component]
fn GroupRow(props: GroupRowProps) -> Element {
    let nav = use_navigator();
    let gate = use_auth_gate();
    let g = props.group.clone();
    let sgid = g.sgid;
    let server_id = props.server_id;
    let sid = props.sid;
    let protected = group_type_protected(g.group_type);

    let counts = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_counts(gate, server_id, sid, sgid).await }
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
        nav.push(Route::ServerGroupDetailPage { sgid });
    };
    let on_delete = props.on_delete;
    let group_for_delete = g.clone();

    rsx! {
        tr { key: "{sgid}", class: "sg-row",
            td {
                class: "sg-name-cell",
                role: "link",
                tabindex: "0",
                onclick: go_detail,
                onkeydown: move |e: KeyboardEvent| {
                    if matches!(e.key(), Key::Enter | Key::Character(_)) && e.key() == Key::Enter {
                        nav.push(Route::ServerGroupDetailPage { sgid });
                    }
                },
                span { class: "sg-name", "{g.name}" }
                span { class: "sg-id mono", "sgid {sgid}" }
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
struct NewGroupModalProps {
    server_id: i64,
    sid: i64,
    existing: Vec<ServerGroupItem>,
    on_close: EventHandler<()>,
    on_created: EventHandler<()>,
}

#[component]
fn NewGroupModal(props: NewGroupModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_created = props.on_created;
    let server_id = props.server_id;
    let sid = props.sid;

    let mut name = use_signal(String::new);
    // `0` = copy from nothing (empty group); else the source sgid.
    let mut copy_from = use_signal(|| 0i64);
    let mut busy = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let existing = props.existing.clone();

    let on_submit = {
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *busy.peek() {
                return;
            }
            let group_name = name.peek().trim().to_string();
            if group_name.is_empty() {
                error.set(Some("Enter a name for the new group.".into()));
                return;
            }
            let source = *copy_from.peek();
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            error.set(None);
            spawn(async move {
                let base = api::api_base();
                let res: Result<ServerGroupCreated, ApiError> = if source > 0 {
                    let path =
                        format!("/api/servers/{server_id}/vs/{sid}/server-groups/{source}/copy");
                    let body = ServerGroupCopyRequest {
                        name: group_name.clone(),
                        r#type: None,
                    };
                    api::authorized_post_json(&gate, &base, &path, Some(&body)).await
                } else {
                    let path = format!("/api/servers/{server_id}/vs/{sid}/server-groups");
                    let body = GroupCreateRequest {
                        name: group_name.clone(),
                        r#type: None,
                    };
                    api::authorized_post_json(&gate, &base, &path, Some(&body)).await
                };
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Created group “{group_name}”"),
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
                "aria-labelledby": "new-group-title",
                div { class: "modal-header",
                    h2 { id: "new-group-title", "New server group" }
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
                        Banner { variant: BannerVariant::Danger, title: "Could not create group".to_string(),
                            "{msg}"
                        }
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "new-group-name", "Name" }
                        input {
                            id: "new-group-name",
                            class: "input",
                            r#type: "text",
                            placeholder: "e.g. Moderators",
                            value: "{name.read()}",
                            disabled: *busy.read(),
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "new-group-copy", "Copy permissions from" }
                        select {
                            id: "new-group-copy",
                            class: "input",
                            disabled: *busy.read(),
                            onchange: move |e| copy_from.set(e.value().parse::<i64>().unwrap_or(0)),
                            option { value: "0", "Empty group — no permissions" }
                            for src in existing.iter() {
                                option { key: "{src.sgid}", value: "{src.sgid}",
                                    "{src.name} ({group_type_label(src.group_type)})"
                                }
                            }
                        }
                        p { class: "field-help",
                            "Copying seeds the new group with another group's permissions so you start from a sensible baseline."
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
pub(crate) struct DeleteGroupModalProps {
    pub server_id: i64,
    pub sid: i64,
    pub group: ServerGroupItem,
    pub on_close: EventHandler<()>,
    pub on_deleted: EventHandler<()>,
}

/// Destructive group delete — type-to-confirm the group name (UX brief §3).
/// Shared by the list page and the detail Settings danger zone.
#[component]
pub(crate) fn DeleteGroupModal(props: DeleteGroupModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_deleted = props.on_deleted;
    let group = props.group.clone();
    let sgid = group.sgid;
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
                let path = format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}");
                let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Deleted group “{group_name}”"),
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
                "aria-labelledby": "delete-group-title",
                div { class: "modal-header",
                    h2 { id: "delete-group-title", "Delete server group" }
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
                        Banner { variant: BannerVariant::Danger, title: "Could not delete group".to_string(),
                            "{msg}"
                        }
                    }
                    Banner {
                        variant: BannerVariant::Warning,
                        title: "This cannot be undone".to_string(),
                        "Every member of this group loses every permission it grants. Members themselves are not removed from the server."
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "delete-group-confirm",
                            "Type the group name "
                            strong { "{group_name}" }
                            " to confirm"
                        }
                        input {
                            id: "delete-group-confirm",
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
) -> Result<Vec<ServerGroupItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/server-groups");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

/// Member + permission count for one group. Two reads; a failure of either
/// degrades that count to "—" rather than failing the whole row.
async fn fetch_counts(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    sgid: i64,
) -> Result<(usize, usize), ApiError> {
    let base = api::api_base();
    let members_path = format!("/api/servers/{config_id}/vs/{sid}/server-groups/{sgid}/members");
    let perms_path = format!("/api/servers/{config_id}/vs/{sid}/server-groups/{sgid}/permissions");
    let members =
        api::authorized_get_json::<Vec<ServerGroupMember>>(&gate, &base, &members_path).await?;
    let perms = api::authorized_get_json::<Vec<GroupPermItem>>(&gate, &base, &perms_path).await?;
    Ok((members.len(), perms.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_labels_cover_the_three_ts_kinds() {
        assert_eq!(group_type_label(0), "Template");
        assert_eq!(group_type_label(1), "Regular");
        assert_eq!(group_type_label(2), "Query");
        assert_eq!(group_type_label(9), "Unknown");
    }

    #[test]
    fn template_and_query_groups_are_protected() {
        assert!(group_type_protected(0));
        assert!(!group_type_protected(1));
        assert!(group_type_protected(2));
    }
}
