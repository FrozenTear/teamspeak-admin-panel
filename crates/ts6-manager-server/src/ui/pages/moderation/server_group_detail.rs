//! `/moderation/server-groups/:sgid` — server-group detail. PURA-375
//! (UX brief §4.2).
//!
//! Three [`Tabs`]: **Permissions** (the shared [`PermissionEditor`]),
//! **Members** (`servergroupclientlist` + add/remove), **Settings**
//! (rename + the danger-zone delete). The header carries Duplicate and a
//! breadcrumb back to the list.
//!
//! Read is any operator with server access; every write affordance is
//! suppressed for non-admins (UX brief §2.1) — the route layer re-checks.

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ClientListItem, GroupMemberAddRequest, GroupRenameRequest, ServerGroupCopyRequest,
    ServerGroupCreated, ServerGroupItem, ServerGroupMember,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, TabItem, TabPanel, Tabs,
};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::routes::Route;

use super::format_error;
use super::permeditor::PermissionEditor;
use super::server_groups::{DeleteGroupModal, group_type_label, group_type_protected};

#[component]
pub fn ServerGroupDetailPage(sgid: i64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
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
            h1 { "Server group" }
            div { class: "empty",
                div { class: "icon", "⚐" }
                h3 { "No server selected" }
                p { "Select a server to manage its permission groups." }
            }
        };
    };
    let server_id = server.id;
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // The list endpoint is the only place a group's name + type live, so the
    // detail page fetches it and finds this `sgid`.
    let mut groups_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/server-groups");
                api::authorized_get_json::<Vec<ServerGroupItem>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let mut active_tab = use_signal(|| "perms".to_string());
    let mut show_delete = use_signal(|| false);
    let mut show_duplicate = use_signal(|| false);

    let groups_snapshot = groups_res.read().clone();

    let group = match &groups_snapshot {
        None => {
            return rsx! {
                div { class: "crumb", "Moderation · Server groups" }
                p { class: "info-hint", "Loading group…" }
            };
        }
        Some(Err(e)) => {
            return rsx! {
                div { class: "crumb", "Moderation · Server groups" }
                h1 { "Server group" }
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load the group".to_string(),
                    "{format_error(e)}"
                    div { class: "perm-editor-retry",
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            onclick: move |_| groups_res.restart(),
                            "Retry"
                        }
                    }
                }
            };
        }
        Some(Ok(groups)) => groups.iter().find(|g| g.sgid == sgid).cloned(),
    };

    let Some(group) = group else {
        return rsx! {
            div { class: "crumb",
                Link { to: Route::ServerGroupsPage {}, "Moderation · Server groups" }
            }
            h1 { "Server group" }
            div { class: "empty",
                div { class: "icon", "⚐" }
                h3 { "Group not found" }
                p { "No server group with sgid {sgid} exists on this server. It may have been deleted." }
                Link { class: "btn btn-secondary btn-sm", to: Route::ServerGroupsPage {},
                    "Back to server groups"
                }
            }
        };
    };

    let protected = group_type_protected(group.group_type);
    let group_name = group.name.clone();
    let perms_path = format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}/permissions");
    let catalog_path = format!("/api/servers/{server_id}/vs/{sid}/permissions");

    let tabs = vec![
        TabItem::new("perms", "Permissions"),
        TabItem::new("members", "Members"),
        TabItem::new("settings", "Settings"),
    ];
    let current = active_tab.read().clone();
    let nav = use_navigator();

    rsx! {
        div { class: "crumb",
            Link { to: Route::ServerGroupsPage {}, "Moderation · Server groups" }
            " · {group_name}"
        }
        div { class: "mod-panel-head",
            div {
                h1 { "{group_name}" }
                p { class: "info-hint",
                    span { class: "tag tag-neutral", "{group_type_label(group.group_type)}" }
                    " · sgid {sgid}"
                }
            }
            if is_admin {
                div { class: "sg-detail-actions",
                    Button {
                        variant: ButtonVariant::Secondary,
                        size: ButtonSize::Small,
                        onclick: move |_| show_duplicate.set(true),
                        "Duplicate"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        size: ButtonSize::Small,
                        disabled: protected,
                        title: if protected { "Server-managed group — cannot be deleted" } else { "Delete this group" },
                        onclick: move |_| {
                            if !protected {
                                show_delete.set(true);
                            }
                        },
                        "Delete"
                    }
                }
            }
        }

        if !is_admin {
            Banner {
                variant: BannerVariant::Info,
                title: "Read-only".to_string(),
                "Group and permission changes require an admin account."
            }
        }
        if protected {
            Banner {
                variant: BannerVariant::Warning,
                title: "Server-managed group".to_string(),
                "This is a {group_type_label(group.group_type)} group. It cannot be deleted; edit its permissions with care."
            }
        }

        Tabs {
            tabs: tabs.clone(),
            active: current.clone(),
            id: "sg-detail".to_string(),
            aria_label: "Server group sections".to_string(),
            onselect: move |id| active_tab.set(id),
        }

        TabPanel { id: "perms".to_string(), tabs_id: "sg-detail".to_string(), active: current == "perms",
            PermissionEditor {
                perms_path: perms_path.clone(),
                catalog_path: catalog_path.clone(),
                can_write: is_admin,
                channel_group: false,
            }
        }
        TabPanel { id: "members".to_string(), tabs_id: "sg-detail".to_string(), active: current == "members",
            MembersTab { server_id, sid, sgid, can_write: is_admin }
        }
        TabPanel { id: "settings".to_string(), tabs_id: "sg-detail".to_string(), active: current == "settings",
            SettingsTab {
                server_id,
                sid,
                group: group.clone(),
                can_write: is_admin,
                on_renamed: EventHandler::new(move |_: ()| groups_res.restart()),
            }
        }

        if *show_delete.read() {
            DeleteGroupModal {
                server_id,
                sid,
                group: group.clone(),
                on_close: EventHandler::new(move |_: ()| show_delete.set(false)),
                on_deleted: EventHandler::new(move |_: ()| {
                    show_delete.set(false);
                    nav.push(Route::ServerGroupsPage {});
                }),
            }
        }
        if *show_duplicate.read() {
            DuplicateGroupModal {
                server_id,
                sid,
                sgid,
                source_name: group_name.clone(),
                on_close: EventHandler::new(move |_: ()| show_duplicate.set(false)),
                on_duplicated: EventHandler::new(move |new_sgid: i64| {
                    show_duplicate.set(false);
                    nav.push(Route::ServerGroupDetailPage { sgid: new_sgid });
                }),
            }
        }
    }
}

// ── Members tab ─────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct MembersTabProps {
    server_id: i64,
    sid: i64,
    sgid: i64,
    can_write: bool,
}

#[component]
fn MembersTab(props: MembersTabProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let server_id = props.server_id;
    let sid = props.sid;
    let sgid = props.sgid;
    let can_write = props.can_write;

    let mut members_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path =
                    format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}/members");
                api::authorized_get_json::<Vec<ServerGroupMember>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    // The client list backs the add-member picker so the operator picks a
    // name, not a raw `cldbid`.
    let clients_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients");
                api::authorized_get_json::<Vec<ClientListItem>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let mut add_cldbid = use_signal(|| 0i64);
    let mut busy = use_signal(|| false);

    let on_add = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() || !can_write {
                return;
            }
            let cldbid = *add_cldbid.peek();
            if cldbid <= 0 {
                toaster.push(ToastVariant::Warning, "Pick a client to add", None);
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let path =
                    format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}/members");
                let body = GroupMemberAddRequest { cldbid };
                let res =
                    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body))
                        .await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Member added", None);
                        add_cldbid.set(0);
                        members_res.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not add member",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let make_remove = {
        let gate = gate.clone();
        move |cldbid: i64| {
            let gate = gate.clone();
            let toaster = toaster;
            spawn(async move {
                let path = format!(
                    "/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}/members/{cldbid}"
                );
                match api::authorized_delete(&gate, &api::api_base(), &path).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Member removed", None);
                        members_res.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not remove member",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let clients = clients_res
        .read()
        .clone()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let members_snapshot = members_res.read().clone();

    rsx! {
        section { class: "stack-md",
            if can_write {
                div { class: "sg-add-member",
                    label { class: "field-label", r#for: "sg-add-member-select", "Add a member" }
                    div { class: "sg-add-member-row",
                        select {
                            id: "sg-add-member-select",
                            class: "input",
                            disabled: *busy.read() || clients.is_empty(),
                            onchange: move |e| add_cldbid.set(e.value().parse::<i64>().unwrap_or(0)),
                            option { value: "0", "Select a client…" }
                            for c in clients.iter() {
                                option { key: "{c.client_database_id}", value: "{c.client_database_id}",
                                    "{c.client_nickname} (cldbid {c.client_database_id})"
                                }
                            }
                        }
                        Button {
                            variant: ButtonVariant::Primary,
                            size: ButtonSize::Small,
                            loading: *busy.read(),
                            onclick: on_add,
                            "Add member"
                        }
                    }
                    if clients.is_empty() {
                        p { class: "field-help",
                            "No clients are currently visible on this server — only connected / database-known clients can be added here."
                        }
                    }
                }
            }

            match members_snapshot {
                None => rsx! {
                    p { class: "info-hint", "Loading members…" }
                },
                Some(Err(e)) => rsx! {
                    Banner {
                        variant: BannerVariant::Danger,
                        title: "Could not load members".to_string(),
                        "{format_error(&e)}"
                        div { class: "perm-editor-retry",
                            Button {
                                variant: ButtonVariant::Secondary,
                                size: ButtonSize::Small,
                                onclick: move |_| members_res.restart(),
                                "Retry"
                            }
                        }
                    }
                },
                Some(Ok(members)) if members.is_empty() => rsx! {
                    div { class: "empty",
                        div { class: "icon", "⚐" }
                        h3 { "No members yet" }
                        p { "No client is assigned to this group." }
                    }
                },
                Some(Ok(members)) => rsx! {
                    table { class: "data-table", "aria-label": "Group members",
                        thead {
                            tr {
                                th { scope: "col", "Client" }
                                th { scope: "col", "UID" }
                                th { scope: "col", class: "actions-col", "" }
                            }
                        }
                        tbody {
                            for m in members.iter().cloned() {
                                {
                                    let cldbid = m.cldbid;
                                    let remove = make_remove.clone();
                                    rsx! {
                                        tr { key: "{cldbid}",
                                            td {
                                                span { class: "sg-name", "{m.client_nickname}" }
                                                span { class: "sg-id mono", "cldbid {cldbid}" }
                                            }
                                            td { class: "mono", "{m.client_unique_identifier}" }
                                            td { class: "actions-col",
                                                if can_write {
                                                    Button {
                                                        variant: ButtonVariant::Danger,
                                                        size: ButtonSize::Small,
                                                        onclick: move |_| remove(cldbid),
                                                        "Remove"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

// ── Settings tab ────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct SettingsTabProps {
    server_id: i64,
    sid: i64,
    group: ServerGroupItem,
    can_write: bool,
    on_renamed: EventHandler<()>,
}

#[component]
fn SettingsTab(props: SettingsTabProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let server_id = props.server_id;
    let sid = props.sid;
    let group = props.group.clone();
    let sgid = group.sgid;
    let can_write = props.can_write;
    let on_renamed = props.on_renamed;

    let mut name = use_signal(|| group.name.clone());
    let mut busy = use_signal(|| false);
    let original_name = group.name.clone();
    let dirty = name.read().trim() != original_name.trim() && !name.read().trim().is_empty();

    let on_save = {
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *busy.peek() || !can_write {
                return;
            }
            let new_name = name.peek().trim().to_string();
            if new_name.is_empty() {
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}");
                let body = GroupRenameRequest {
                    name: new_name.clone(),
                };
                let res =
                    api::authorized_put_json::<_, ()>(&gate, &api::api_base(), &path, &body).await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Renamed to “{new_name}”"),
                            None,
                        );
                        on_renamed.call(());
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not rename group",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    rsx! {
        section { class: "stack-md mod-panel",
            form { class: "stack-md", onsubmit: on_save,
                div { class: "field",
                    label { class: "field-label", r#for: "sg-rename", "Group name" }
                    input {
                        id: "sg-rename",
                        class: "input",
                        r#type: "text",
                        value: "{name.read()}",
                        disabled: !can_write || *busy.read(),
                        oninput: move |e| name.set(e.value()),
                    }
                }
                if can_write {
                    div {
                        Button {
                            variant: ButtonVariant::Primary,
                            kind: ButtonType::Submit,
                            disabled: !dirty,
                            loading: *busy.read(),
                            "Save name"
                        }
                    }
                }
            }

            dl { class: "mod-kv",
                dt { "Type" }
                dd { "{group_type_label(group.group_type)}" }
                dt { "Server group id" }
                dd { class: "mono", "{sgid}" }
                dt { "Sort order" }
                dd { class: "mono", "{group.sortid}" }
            }
        }

        if can_write {
            section { class: "stack-md sg-danger-zone",
                Banner {
                    variant: BannerVariant::Warning,
                    title: "Danger zone".to_string(),
                    if group_type_protected(group.group_type) {
                        "This is a server-managed group and cannot be deleted."
                    } else {
                        "Deleting this group removes every permission it grants from its members."
                    }
                }
                if !group_type_protected(group.group_type) {
                    p { class: "info-hint",
                        "Use the Delete button at the top of the page to remove this group."
                    }
                }
            }
        }
    }
}

// ── Duplicate-group modal ───────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct DuplicateGroupModalProps {
    server_id: i64,
    sid: i64,
    sgid: i64,
    source_name: String,
    on_close: EventHandler<()>,
    on_duplicated: EventHandler<i64>,
}

#[component]
fn DuplicateGroupModal(props: DuplicateGroupModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_duplicated = props.on_duplicated;
    let server_id = props.server_id;
    let sid = props.sid;
    let sgid = props.sgid;

    let mut name = use_signal(|| format!("{} (copy)", props.source_name));
    let mut busy = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let on_submit = {
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *busy.peek() {
                return;
            }
            let new_name = name.peek().trim().to_string();
            if new_name.is_empty() {
                error.set(Some("Enter a name for the copy.".into()));
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            error.set(None);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/server-groups/{sgid}/copy");
                let body = ServerGroupCopyRequest {
                    name: new_name.clone(),
                    r#type: None,
                };
                let res: Result<ServerGroupCreated, ApiError> =
                    api::authorized_post_json(&gate, &api::api_base(), &path, Some(&body)).await;
                busy.set(false);
                match res {
                    Ok(created) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Duplicated as “{new_name}”"),
                            None,
                        );
                        on_duplicated.call(created.sgid);
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
                "aria-labelledby": "duplicate-group-title",
                div { class: "modal-header",
                    h2 { id: "duplicate-group-title", "Duplicate server group" }
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
                        Banner { variant: BannerVariant::Danger, title: "Could not duplicate group".to_string(),
                            "{msg}"
                        }
                    }
                    p { class: "info-hint",
                        "The new group is created as a Regular group with a copy of every permission this group sets."
                    }
                    div { class: "field",
                        label { class: "field-label", r#for: "duplicate-group-name", "New group name" }
                        input {
                            id: "duplicate-group-name",
                            class: "input",
                            r#type: "text",
                            value: "{name.read()}",
                            disabled: *busy.read(),
                            oninput: move |e| name.set(e.value()),
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
                        "Duplicate"
                    }
                }
            }
        }
    }
}
