//! `/moderation/channel-groups/:cgid` — channel-group detail. PURA-378
//! (UX brief §5).
//!
//! Three [`Tabs`]: **Permissions** (the shared [`PermissionEditor`] with
//! `channel_group: true`, which hides the negate / skip flags TS6
//! channel-group permissions do not carry), **Members**
//! (`channelgroupclientlist` + the per-channel Assign form), **Settings**
//! (rename + the danger-zone delete).
//!
//! The Members tab is the one real divergence from [`super::server_groups`]:
//! channel-group membership is **per-channel**. `channelgroupclientlist`
//! returns `(cid, cldbid)` pairs, so the table has a Channel column and the
//! Assign form needs both a client picker and a channel picker. The copy
//! leans into this — operators carry a server-group mental model where a
//! group is server-wide, and the per-channel scoping has to be explicit.
//!
//! TS6 channel groups have no `channelgroupcopy`, so there is no Duplicate
//! affordance (the server-group detail page has one).
//!
//! Read is any operator with server access; every write affordance is
//! suppressed for non-admins (UX brief §2.1) — the route layer re-checks.

use std::collections::HashMap;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ChannelGroupAssignRequest, ChannelGroupClientItem, ChannelGroupItem, ChannelTreeNode,
    ClientListItem, GroupRenameRequest,
};

use crate::client::api;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, TabItem, TabPanel, Tabs,
};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::routes::Route;

use super::channel_groups::DeleteChannelGroupModal;
use super::format_error;
use super::permeditor::PermissionEditor;
use super::server_groups::{group_type_label, group_type_protected};

#[component]
pub fn ChannelGroupDetailPage(cgid: i64) -> Element {
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
            div { class: "crumb", "Moderation · Channel groups" }
            h1 { "Channel group" }
            div { class: "empty",
                div { class: "icon", "⚑" }
                h3 { "No server selected" }
                p { "Select a server to manage its channel permission groups." }
            }
        };
    };
    let server_id = server.id;
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // The list endpoint is the only place a group's name + type live, so the
    // detail page fetches it and finds this `cgid`.
    let mut groups_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/channel-groups");
                api::authorized_get_json::<Vec<ChannelGroupItem>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let mut active_tab = use_signal(|| "perms".to_string());
    let mut show_delete = use_signal(|| false);

    let groups_snapshot = groups_res.read().clone();

    let group = match &groups_snapshot {
        None => {
            return rsx! {
                div { class: "crumb", "Moderation · Channel groups" }
                p { class: "info-hint", "Loading group…" }
            };
        }
        Some(Err(e)) => {
            return rsx! {
                div { class: "crumb", "Moderation · Channel groups" }
                h1 { "Channel group" }
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
        Some(Ok(groups)) => groups.iter().find(|g| g.cgid == cgid).cloned(),
    };

    let Some(group) = group else {
        return rsx! {
            div { class: "crumb",
                Link { to: Route::ChannelGroupsPage {}, "Moderation · Channel groups" }
            }
            h1 { "Channel group" }
            div { class: "empty",
                div { class: "icon", "⚑" }
                h3 { "Group not found" }
                p { "No channel group with cgid {cgid} exists on this server. It may have been deleted." }
                Link { class: "btn btn-secondary btn-sm", to: Route::ChannelGroupsPage {},
                    "Back to channel groups"
                }
            }
        };
    };

    let protected = group_type_protected(group.group_type);
    let group_name = group.name.clone();
    let perms_path = format!("/api/servers/{server_id}/vs/{sid}/channel-groups/{cgid}/permissions");
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
            Link { to: Route::ChannelGroupsPage {}, "Moderation · Channel groups" }
            " · {group_name}"
        }
        div { class: "mod-panel-head",
            div {
                h1 { "{group_name}" }
                p { class: "info-hint",
                    span { class: "tag tag-neutral", "{group_type_label(group.group_type)}" }
                    " · cgid {cgid}"
                }
            }
            if is_admin {
                div { class: "sg-detail-actions",
                    Button {
                        variant: ButtonVariant::Danger,
                        size: ButtonSize::Small,
                        disabled: protected,
                        title: if protected { "Server-managed group — cannot be deleted" } else { "Delete this channel group" },
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
                "Channel-group and permission changes require an admin account."
            }
        }
        if protected {
            Banner {
                variant: BannerVariant::Warning,
                title: "Server-managed group".to_string(),
                "This is a {group_type_label(group.group_type)} channel group. It cannot be deleted; edit its permissions with care."
            }
        }

        Tabs {
            tabs: tabs.clone(),
            active: current.clone(),
            id: "cg-detail".to_string(),
            aria_label: "Channel group sections".to_string(),
            onselect: move |id| active_tab.set(id),
        }

        TabPanel { id: "perms".to_string(), tabs_id: "cg-detail".to_string(), active: current == "perms",
            PermissionEditor {
                perms_path: perms_path.clone(),
                catalog_path: catalog_path.clone(),
                can_write: is_admin,
                channel_group: true,
            }
        }
        TabPanel { id: "members".to_string(), tabs_id: "cg-detail".to_string(), active: current == "members",
            MembersTab { server_id, sid, cgid, can_write: is_admin }
        }
        TabPanel { id: "settings".to_string(), tabs_id: "cg-detail".to_string(), active: current == "settings",
            SettingsTab {
                server_id,
                sid,
                group: group.clone(),
                can_write: is_admin,
                on_renamed: EventHandler::new(move |_: ()| groups_res.restart()),
            }
        }

        if *show_delete.read() {
            DeleteChannelGroupModal {
                server_id,
                sid,
                group: group.clone(),
                on_close: EventHandler::new(move |_: ()| show_delete.set(false)),
                on_deleted: EventHandler::new(move |_: ()| {
                    show_delete.set(false);
                    nav.push(Route::ChannelGroupsPage {});
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
    cgid: i64,
    can_write: bool,
}

#[component]
fn MembersTab(props: MembersTabProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let server_id = props.server_id;
    let sid = props.sid;
    let cgid = props.cgid;
    let can_write = props.can_write;

    // Channel-group membership is per-channel — each row is a (channel,
    // client) pair, not just a client (the structural difference from
    // server groups).
    let mut members_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path =
                    format!("/api/servers/{server_id}/vs/{sid}/channel-groups/{cgid}/clients");
                api::authorized_get_json::<Vec<ChannelGroupClientItem>>(
                    &gate,
                    &api::api_base(),
                    &path,
                )
                .await
            }
        }
    });

    // The client list backs the Assign picker and resolves a `cldbid` to a
    // nickname for the table — `channelgroupclientlist` carries only ids.
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

    // The channel list backs the channel picker and resolves a `cid` to a
    // channel name for the table.
    let channels_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/channels");
                api::authorized_get_json::<Vec<ChannelTreeNode>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let mut add_cldbid = use_signal(|| 0i64);
    let mut add_cid = use_signal(|| 0i64);
    let mut busy = use_signal(|| false);

    let on_assign = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() || !can_write {
                return;
            }
            let cldbid = *add_cldbid.peek();
            let cid = *add_cid.peek();
            if cldbid <= 0 {
                toaster.push(ToastVariant::Warning, "Pick a client to assign", None);
                return;
            }
            if cid <= 0 {
                toaster.push(
                    ToastVariant::Warning,
                    "Pick a channel for the assignment",
                    None,
                );
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let path =
                    format!("/api/servers/{server_id}/vs/{sid}/channel-groups/{cgid}/assign");
                let body = ChannelGroupAssignRequest { cid, cldbid };
                let res =
                    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body))
                        .await;
                busy.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            "Client assigned to this channel group",
                            None,
                        );
                        add_cldbid.set(0);
                        add_cid.set(0);
                        members_res.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not assign client",
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
    let channels = channels_res
        .read()
        .clone()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    // Lookup tables so each member row reads as a name, not a raw id pair.
    let nick_by_cldbid: HashMap<i64, String> = clients
        .iter()
        .map(|c| (c.client_database_id, c.client_nickname.clone()))
        .collect();
    let channel_by_cid: HashMap<i64, String> = channels
        .iter()
        .map(|c| (c.cid, c.channel_name.clone()))
        .collect();

    let members_snapshot = members_res.read().clone();

    rsx! {
        section { class: "stack-md",
            p { class: "info-hint",
                "A client belongs to exactly one channel group in any given channel. Assigning a client here sets their group for the chosen channel and replaces whatever channel group they held there before."
            }

            if can_write {
                div { class: "sg-add-member",
                    label { class: "field-label", r#for: "cg-assign-client", "Assign a client" }
                    div { class: "sg-add-member-row",
                        select {
                            id: "cg-assign-client",
                            class: "input",
                            "aria-label": "Client to assign",
                            disabled: *busy.read() || clients.is_empty(),
                            onchange: move |e| add_cldbid.set(e.value().parse::<i64>().unwrap_or(0)),
                            option { value: "0", "Select a client…" }
                            for c in clients.iter() {
                                option { key: "{c.client_database_id}", value: "{c.client_database_id}",
                                    "{c.client_nickname} (cldbid {c.client_database_id})"
                                }
                            }
                        }
                        select {
                            id: "cg-assign-channel",
                            class: "input",
                            "aria-label": "Channel for the assignment",
                            disabled: *busy.read() || channels.is_empty(),
                            onchange: move |e| add_cid.set(e.value().parse::<i64>().unwrap_or(0)),
                            option { value: "0", "Select a channel…" }
                            for ch in channels.iter() {
                                option { key: "{ch.cid}", value: "{ch.cid}",
                                    "{ch.channel_name} (cid {ch.cid})"
                                }
                            }
                        }
                        Button {
                            variant: ButtonVariant::Primary,
                            size: ButtonSize::Small,
                            loading: *busy.read(),
                            onclick: on_assign,
                            "Assign"
                        }
                    }
                    if clients.is_empty() {
                        p { class: "field-help",
                            "No clients are currently visible on this server — only connected / database-known clients can be assigned here."
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
                        div { class: "icon", "⚑" }
                        h3 { "No members yet" }
                        p { "No client is assigned to this channel group in any channel." }
                    }
                },
                Some(Ok(members)) => rsx! {
                    table { class: "data-table", "aria-label": "Channel group members",
                        thead {
                            tr {
                                th { scope: "col", "Client" }
                                th { scope: "col", "Channel" }
                            }
                        }
                        tbody {
                            for m in members.iter() {
                                {
                                    let client_label = nick_by_cldbid
                                        .get(&m.cldbid)
                                        .cloned()
                                        .unwrap_or_else(|| format!("cldbid {}", m.cldbid));
                                    let channel_label = channel_by_cid
                                        .get(&m.cid)
                                        .cloned()
                                        .unwrap_or_else(|| format!("cid {}", m.cid));
                                    rsx! {
                                        tr { key: "{m.cid}-{m.cldbid}",
                                            td {
                                                span { class: "sg-name", "{client_label}" }
                                                span { class: "sg-id mono", "cldbid {m.cldbid}" }
                                            }
                                            td {
                                                span { class: "sg-name", "{channel_label}" }
                                                span { class: "sg-id mono", "cid {m.cid}" }
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
    group: ChannelGroupItem,
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
    let cgid = group.cgid;
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
                let path = format!("/api/servers/{server_id}/vs/{sid}/channel-groups/{cgid}");
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
                            "Could not rename channel group",
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
                    label { class: "field-label", r#for: "cg-rename", "Group name" }
                    input {
                        id: "cg-rename",
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
                dt { "Channel group id" }
                dd { class: "mono", "{cgid}" }
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
                        "This is a server-managed channel group and cannot be deleted."
                    } else {
                        "Deleting this channel group re-homes every member to the server default channel group in the affected channels."
                    }
                }
                if !group_type_protected(group.group_type) {
                    p { class: "info-hint",
                        "Use the Delete button at the top of the page to remove this channel group."
                    }
                }
            }
        }
    }
}
