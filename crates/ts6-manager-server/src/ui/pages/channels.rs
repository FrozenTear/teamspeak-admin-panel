//! `/channels` — channel tree with per-channel client list. PURA-73.
//!
//! - Snapshots `GET /api/servers/{configId}/vs/{sid}/channels` (spec §7.7).
//! - Subscribes to `server:{configId}:channels` for live edits + the
//!   `server:{configId}:clients` topic so the per-channel client roster
//!   updates as people connect/move.
//! - Tree assembly: the REST layer returns a flat list ordered by upstream
//!   `channel_order`. We group by `pid`, recursing from the synthetic root
//!   (channels with `pid == 0`).
//! - Spacers (`[l/c/r/*]spacer<n>]…`, `[*l/r/c]…`, or all-glyph names)
//!   render as labelled rules — same heuristic the public widget renderer
//!   (PURA-86) uses, kept module-local for now to avoid a premature
//!   shared crate.
//!
//! Writes pair with the drafted control routes (PR #25): create / edit /
//! delete / move. Those handlers are admin-only (`check_admin`); the UI
//! disables the same actions for non-admins instead of hiding the chrome.

use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem};

use crate::client::api::{self, ApiError};
use crate::client::channels as ch_api;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

const ADMIN_ONLY_HINT: &str =
    "Admin-only. Channel create, edit, delete, and move require an admin role.";

const PAGE_LEDE: &str = "Live channel tree for the selected server. Occupied channels list clients underneath. Admins can create, edit, delete, and reorder channels.";

#[derive(Clone, PartialEq)]
enum ChannelDialog {
    None,
    Create,
    Edit(ChannelTreeNode),
    Delete(ChannelTreeNode),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Permanence {
    Permanent,
    SemiPermanent,
    Temporary,
}

impl Permanence {
    fn from_node(n: &ChannelTreeNode) -> Self {
        if n.channel_flag_semi_permanent != 0 {
            Permanence::SemiPermanent
        } else if n.channel_flag_permanent == 0 {
            Permanence::Temporary
        } else {
            Permanence::Permanent
        }
    }

    fn flags(self) -> (i64, i64, i64) {
        match self {
            Permanence::Permanent => (1, 0, 0),
            Permanence::SemiPermanent => (0, 1, 0),
            Permanence::Temporary => (0, 0, 1),
        }
    }
}

#[component]
pub fn ChannelsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let hub = use_ws_hub();
    let toaster = use_toaster();
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
            ChannelsChrome { server_name: None, is_admin: is_admin, on_create: None }
            div { class: "empty",
                div { class: "icon", aria_hidden: "true", "#" }
                h3 { "No server selected" }
                p { "Add a server to view its channel tree." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut channels: Signal<Vec<ChannelTreeNode>> = use_signal(Vec::new);
    let mut clients: Signal<Vec<ClientListItem>> = use_signal(Vec::new);
    let mut dialog: Signal<ChannelDialog> = use_signal(|| ChannelDialog::None);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let mut channels_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { fetch_channels(gate, server_id, sid).await }
        }
    });
    let mut clients_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { fetch_clients(gate, server_id, sid).await }
        }
    });

    {
        use_effect(move || match &*channels_resource.read_unchecked() {
            Some(Ok(rows)) => {
                channels.set(rows.clone());
                error.set(None);
            }
            Some(Err(e)) => error.set(Some(e.clone())),
            None => {}
        });
    }
    {
        use_effect(move || {
            if let Some(Ok(rows)) = &*clients_resource.read_unchecked() {
                clients.set(rows.clone());
            }
        });
    }

    // WS — refetch on any channel/client edit. PR #25 publishes
    // `ts:channel:created|edited|deleted|moved` on this topic; the SSH
    // notify bridge uses the same kinds. Restarting the snapshot keeps
    // the tree honest without per-event reducers.
    {
        let hub = hub.clone();
        let _resource = use_resource(move || {
            let hub = hub.clone();
            async move {
                let topic = format!("server:{server_id}:channels");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(_env) = rx.next().await {
                    channels_resource.restart();
                }
            }
        });
    }
    {
        let hub = hub.clone();
        let _resource = use_resource(move || {
            let hub = hub.clone();
            async move {
                let topic = format!("server:{server_id}:clients");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(env) = rx.next().await {
                    if matches!(
                        env.kind.as_str(),
                        "ts:client:moved"
                            | "ts:client:kicked_from_server"
                            | "ts:client:kicked_from_channel"
                    ) {
                        clients_resource.restart();
                    }
                    let _ = (env,);
                }
            }
        });
    }

    let channels_loading = channels_resource.read_unchecked().is_none();
    let bump = move || reload.with_mut(|n| *n += 1);
    let on_refresh = EventHandler::new({
        let mut bump = bump;
        move |_: ()| bump()
    });

    let on_reorder = EventHandler::new({
        let gate = gate.clone();
        move |(cid, up): (i64, bool)| {
            let rows = channels.read().clone();
            let Some(node) = rows.iter().find(|c| c.cid == cid).cloned() else {
                return;
            };
            let siblings = siblings_of(&rows, node.pid);
            let Some(order) = sibling_move_order(&siblings, cid, up) else {
                return;
            };
            let gate = gate.clone();
            let toaster = toaster;
            let mut bump = bump;
            spawn(async move {
                let body = ch_api::ChannelMoveRequest {
                    cpid: node.pid,
                    order: Some(order),
                };
                match ch_api::move_channel(gate, server_id, sid, cid, &body).await {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Moved “{}”", node.channel_name),
                            None,
                        );
                        bump();
                    }
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Move failed", Some(format_error(&e)))
                    }
                }
            });
        }
    });

    let dialog_now = dialog.read().clone();

    rsx! {
        ChannelsChrome {
            server_name: Some(server_name),
            is_admin: is_admin,
            on_create: Some(EventHandler::new(move |_: ()| dialog.set(ChannelDialog::Create))),
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load channels".to_string(),
                p { "{format_error(err)}" }
                if let Some(hint) = err.transport_hint() {
                    p { class: "banner-hint", "{hint}" }
                }
            }
        }

        section { class: "stack-md",
            ChannelsTree {
                channels: channels.read().clone(),
                clients: clients.read().clone(),
                loading: channels_loading,
                is_admin: is_admin,
                on_refresh: on_refresh,
                on_create: EventHandler::new(move |_: ()| dialog.set(ChannelDialog::Create)),
                on_edit: EventHandler::new(move |n: ChannelTreeNode| dialog.set(ChannelDialog::Edit(n))),
                on_delete: EventHandler::new(move |n: ChannelTreeNode| dialog.set(ChannelDialog::Delete(n))),
                on_move_up: EventHandler::new(move |cid: i64| on_reorder.call((cid, true))),
                on_move_down: EventHandler::new(move |cid: i64| on_reorder.call((cid, false))),
            }
        }

        match dialog_now {
            ChannelDialog::Create => rsx! {
                CreateChannelModal {
                    server_id: server_id,
                    sid: sid,
                    channels: channels.read().clone(),
                    on_close: EventHandler::new(move |_: ()| dialog.set(ChannelDialog::None)),
                    on_created: EventHandler::new({
                        let mut bump = bump;
                        move |_: ()| {
                            dialog.set(ChannelDialog::None);
                            bump();
                        }
                    }),
                }
            },
            ChannelDialog::Edit(node) => rsx! {
                EditChannelModal {
                    server_id: server_id,
                    sid: sid,
                    node: node,
                    channels: channels.read().clone(),
                    on_close: EventHandler::new(move |_: ()| dialog.set(ChannelDialog::None)),
                    on_saved: EventHandler::new({
                        let mut bump = bump;
                        move |_: ()| {
                            dialog.set(ChannelDialog::None);
                            bump();
                        }
                    }),
                }
            },
            ChannelDialog::Delete(node) => rsx! {
                DeleteChannelModal {
                    server_id: server_id,
                    sid: sid,
                    node: node,
                    on_close: EventHandler::new(move |_: ()| dialog.set(ChannelDialog::None)),
                    on_deleted: EventHandler::new({
                        let mut bump = bump;
                        move |_: ()| {
                            dialog.set(ChannelDialog::None);
                            bump();
                        }
                    }),
                }
            },
            ChannelDialog::None => rsx! { "" },
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelsChromeProps {
    server_name: Option<String>,
    is_admin: bool,
    on_create: Option<EventHandler<()>>,
}

#[component]
fn ChannelsChrome(props: ChannelsChromeProps) -> Element {
    let crumb = match props.server_name.as_deref() {
        Some(name) => format!("Channels · {name}"),
        None => "Channels".into(),
    };
    let can_create = props.is_admin && props.on_create.is_some();
    let on_create = props.on_create;
    rsx! {
        div { class: "crumb", "{crumb}" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Channels" }
                p { class: "page-lede", "{PAGE_LEDE}" }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    disabled: !can_create,
                    title: if can_create { None } else { Some(ADMIN_ONLY_HINT.to_string()) },
                    onclick: move |_| {
                        if let Some(h) = on_create {
                            h.call(());
                        }
                    },
                    "+ New channel"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelsTreeProps {
    channels: Vec<ChannelTreeNode>,
    clients: Vec<ClientListItem>,
    loading: bool,
    is_admin: bool,
    on_refresh: EventHandler<()>,
    on_create: EventHandler<()>,
    on_edit: EventHandler<ChannelTreeNode>,
    on_delete: EventHandler<ChannelTreeNode>,
    on_move_up: EventHandler<i64>,
    on_move_down: EventHandler<i64>,
}

#[component]
fn ChannelsTree(props: ChannelsTreeProps) -> Element {
    if props.loading && props.channels.is_empty() {
        return rsx! {
            div { class: "card", aria_busy: "true",
                p { class: "muted", "Loading channels…" }
            }
        };
    }
    if props.channels.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", aria_hidden: "true", "#" }
                h3 { "No channels yet" }
                p { "Create a channel or wait for the selected server to report a tree." }
                div { class: "actions",
                    Button {
                        variant: ButtonVariant::Primary,
                        disabled: !props.is_admin,
                        title: if props.is_admin { None } else { Some(ADMIN_ONLY_HINT.to_string()) },
                        onclick: move |_| props.on_create.call(()),
                        "+ New channel"
                    }
                }
            }
        };
    }
    let groups = group_by_parent(&props.channels);
    let clients_by_cid = group_clients(&props.clients);
    let visible_clients = clients_by_cid.values().map(|v| v.len()).sum::<usize>();
    let channel_count = props.channels.len();
    let on_refresh = props.on_refresh;

    rsx! {
        div { class: "channel-tree-panel",
            div { class: "channel-tree-toolbar",
                div { class: "channel-tree-toolbar-meta",
                    span { class: "channel-tree-count",
                        "{channel_count} channels · {visible_clients} clients"
                    }
                    span { class: "channel-tree-hint",
                        if props.is_admin { "Live · admin writes" } else { "Live · read-only" }
                    }
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    size: ButtonSize::Small,
                    onclick: move |_| on_refresh.call(()),
                    "Refresh"
                }
            }
            ul { class: "channel-tree",
                "aria-label": "Channel tree",
                ChannelChildren {
                    pid: 0,
                    depth: 0,
                    groups: groups.clone(),
                    clients: clients_by_cid.clone(),
                    is_admin: props.is_admin,
                    on_edit: props.on_edit,
                    on_delete: props.on_delete,
                    on_move_up: props.on_move_up,
                    on_move_down: props.on_move_down,
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelChildrenProps {
    pid: i64,
    depth: usize,
    groups: Arc<HashMap<i64, Vec<ChannelTreeNode>>>,
    clients: Arc<HashMap<i64, Vec<ClientListItem>>>,
    is_admin: bool,
    on_edit: EventHandler<ChannelTreeNode>,
    on_delete: EventHandler<ChannelTreeNode>,
    on_move_up: EventHandler<i64>,
    on_move_down: EventHandler<i64>,
}

#[component]
fn ChannelChildren(props: ChannelChildrenProps) -> Element {
    let kids = props.groups.get(&props.pid).cloned().unwrap_or_default();
    if kids.is_empty() {
        return rsx! { "" };
    }
    let last = kids.len().saturating_sub(1);
    rsx! {
        for (i, c) in kids.iter().enumerate() {
            {
                let c = c.clone();
                let cid = c.cid;
                let kind = spacer_kind(&c.channel_name);
                let groups = props.groups.clone();
                let clients = props.clients.clone();
                let depth = props.depth;
                let row_clients = clients.get(&cid).cloned().unwrap_or_default();
                let has_children = groups.get(&cid).is_some_and(|k| !k.is_empty());
                let row_class = if is_spacer(&c.channel_name) {
                    "channel-row channel-spacer"
                } else {
                    "channel-row"
                };
                let is_first = i == 0;
                let is_last = i == last;
                rsx! {
                    li { key: "{cid}",
                        class: "{row_class}",
                        style: "--channel-depth: {depth}",
                        ChannelHeader {
                            node: c.clone(),
                            spacer: kind,
                            client_count: row_clients.len(),
                            is_admin: props.is_admin,
                            is_first: is_first,
                            is_last: is_last,
                            on_edit: props.on_edit,
                            on_delete: props.on_delete,
                            on_move_up: props.on_move_up,
                            on_move_down: props.on_move_down,
                        }
                        if !row_clients.is_empty() {
                            ul { class: "channel-clients",
                                "aria-label": "Clients in {c.channel_name}",
                                for r in row_clients.iter() {
                                    {
                                        let r = r.clone();
                                        rsx! { ChannelClientBadge { client: r } }
                                    }
                                }
                            }
                        }
                        if has_children {
                            ul { class: "channel-tree-children",
                                ChannelChildren {
                                    pid: cid,
                                    depth: depth + 1,
                                    groups: groups,
                                    clients: clients,
                                    is_admin: props.is_admin,
                                    on_edit: props.on_edit,
                                    on_delete: props.on_delete,
                                    on_move_up: props.on_move_up,
                                    on_move_down: props.on_move_down,
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelHeaderProps {
    node: ChannelTreeNode,
    spacer: Option<SpacerKind>,
    client_count: usize,
    is_admin: bool,
    is_first: bool,
    is_last: bool,
    on_edit: EventHandler<ChannelTreeNode>,
    on_delete: EventHandler<ChannelTreeNode>,
    on_move_up: EventHandler<i64>,
    on_move_down: EventHandler<i64>,
}

#[component]
fn ChannelHeader(props: ChannelHeaderProps) -> Element {
    let n = props.node.clone();
    if let Some(kind) = props.spacer {
        return rsx! {
            div { class: "channel-row-body",
                ChannelSpacer { name: n.channel_name.clone(), kind: kind }
                ChannelRowActions {
                    node: n,
                    is_admin: props.is_admin,
                    is_first: props.is_first,
                    is_last: props.is_last,
                    on_edit: props.on_edit,
                    on_delete: props.on_delete,
                    on_move_up: props.on_move_up,
                    on_move_down: props.on_move_down,
                }
            }
        };
    }
    let topic = n.channel_topic.trim();
    rsx! {
        div { class: "channel-row-body",
            div { class: "channel-header",
                div { class: "channel-identity",
                    div { class: "channel-identity-row",
                        span { class: "channel-glyph", aria_hidden: "true", "#" }
                        span { class: "channel-name", "{n.channel_name}" }
                    }
                    if !topic.is_empty() {
                        span { class: "channel-topic", "{topic}" }
                    }
                }
                div { class: "channel-meta",
                    if n.channel_flag_password != 0 {
                        span { class: "tag tag-warning", title: "Password protected",
                            span { class: "tag-icn", aria_hidden: "true", "🔒" }
                            "Password"
                        }
                    }
                    if n.channel_flag_default != 0 {
                        span { class: "tag tag-info", "Default" }
                    }
                    if n.channel_flag_permanent != 0 {
                        span { class: "tag tag-neutral", "Permanent" }
                    } else if n.channel_flag_semi_permanent != 0 {
                        span { class: "tag tag-neutral", "Semi-permanent" }
                    }
                    span {
                        class: "tag tag-neutral channel-count",
                        title: "Clients in this channel",
                        "{props.client_count}"
                        if n.channel_maxclients > 0 {
                            " / {n.channel_maxclients}"
                        }
                    }
                }
            }
            ChannelRowActions {
                node: n,
                is_admin: props.is_admin,
                is_first: props.is_first,
                is_last: props.is_last,
                on_edit: props.on_edit,
                on_delete: props.on_delete,
                on_move_up: props.on_move_up,
                on_move_down: props.on_move_down,
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelRowActionsProps {
    node: ChannelTreeNode,
    is_admin: bool,
    is_first: bool,
    is_last: bool,
    on_edit: EventHandler<ChannelTreeNode>,
    on_delete: EventHandler<ChannelTreeNode>,
    on_move_up: EventHandler<i64>,
    on_move_down: EventHandler<i64>,
}

#[component]
fn ChannelRowActions(props: ChannelRowActionsProps) -> Element {
    let node = props.node;
    let cid = node.cid;
    let edit_node = node.clone();
    let delete_node = node;
    let write_title = if props.is_admin {
        None
    } else {
        Some(ADMIN_ONLY_HINT.to_string())
    };
    rsx! {
        div { class: "row-actions channel-row-actions",
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: !props.is_admin,
                title: write_title.clone(),
                onclick: move |_| props.on_edit.call(edit_node.clone()),
                "Edit"
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: !props.is_admin || props.is_first,
                title: write_title.clone(),
                aria_label: Some("Move up".into()),
                onclick: move |_| props.on_move_up.call(cid),
                "↑"
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: !props.is_admin || props.is_last,
                title: write_title.clone(),
                aria_label: Some("Move down".into()),
                onclick: move |_| props.on_move_down.call(cid),
                "↓"
            }
            Button {
                variant: ButtonVariant::Danger,
                size: ButtonSize::Small,
                disabled: !props.is_admin,
                title: write_title,
                onclick: move |_| props.on_delete.call(delete_node.clone()),
                "Delete"
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelSpacerProps {
    name: String,
    kind: SpacerKind,
}

#[component]
fn ChannelSpacer(props: ChannelSpacerProps) -> Element {
    let label = spacer_text(&props.name);
    match props.kind {
        SpacerKind::Line => rsx! {
            div { class: "channel-spacer-line", "aria-hidden": "true" }
        },
        SpacerKind::Dashline => rsx! {
            div { class: "channel-spacer-line dashed", "aria-hidden": "true" }
        },
        SpacerKind::Dotline => rsx! {
            div { class: "channel-spacer-line dotted", "aria-hidden": "true" }
        },
        SpacerKind::Left | SpacerKind::Center | SpacerKind::Right => {
            let align = match props.kind {
                SpacerKind::Center => "center",
                SpacerKind::Right => "right",
                _ => "left",
            };
            let shown = if label.is_empty() {
                props.name.clone()
            } else {
                label
            };
            rsx! {
                div {
                    class: "channel-spacer-label {align}",
                    "aria-hidden": "true",
                    "{shown}"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelClientBadgeProps {
    client: ClientListItem,
}

#[component]
fn ChannelClientBadge(props: ChannelClientBadgeProps) -> Element {
    let r = props.client;
    let away = r.client_away != 0;
    let muted = r.client_input_muted != 0 || r.client_output_muted != 0 || r.client_is_talker == 0;
    let talking = r.client_flag_talking != 0 && !muted;
    let mut class = String::from("channel-client");
    if away {
        class.push_str(" is-away");
    }
    if muted {
        class.push_str(" is-muted");
    }
    if talking {
        class.push_str(" is-talking");
    }
    rsx! {
        li { key: "client-{r.clid}",
            class: "{class}",
            span { class: "client-dot", aria_hidden: "true" }
            span { class: "client-name", "{r.client_nickname}" }
            if away {
                span { class: "client-flag", title: "{r.client_away_message}", "away" }
            }
            if muted {
                span { class: "client-flag", "muted" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CreateChannelModalProps {
    server_id: i64,
    sid: i64,
    channels: Vec<ChannelTreeNode>,
    on_close: EventHandler<()>,
    on_created: EventHandler<()>,
}

#[component]
fn CreateChannelModal(props: CreateChannelModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_created = props.on_created;
    let parents = parent_options(&props.channels, None);

    let mut name: Signal<String> = use_signal(String::new);
    let mut topic: Signal<String> = use_signal(String::new);
    let mut password: Signal<String> = use_signal(String::new);
    let mut parent: Signal<i64> = use_signal(|| 0i64);
    let mut permanence: Signal<Permanence> = use_signal(|| Permanence::Permanent);
    let mut default_flag: Signal<bool> = use_signal(|| false);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed = name.read().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("Name is required.".into()));
            return;
        }
        submitting.set(true);
        error.set(None);
        let (perm, semi, temp) = permanence.read().flags();
        let topic_trim = topic.read().trim().to_string();
        let password_trim = password.read().trim().to_string();
        let cpid = *parent.read();
        let body = ch_api::ChannelCreateRequest {
            channel_name: trimmed.clone(),
            cpid: if cpid == 0 { None } else { Some(cpid) },
            channel_topic: (!topic_trim.is_empty()).then_some(topic_trim),
            channel_password: (!password_trim.is_empty()).then_some(password_trim),
            channel_flag_permanent: Some(perm),
            channel_flag_semi_permanent: Some(semi),
            channel_flag_temporary: Some(temp),
            channel_flag_default: Some(if *default_flag.read() { 1 } else { 0 }),
            ..Default::default()
        };
        let gate = gate.clone();
        let server_id = props.server_id;
        let sid = props.sid;
        spawn(async move {
            match ch_api::create_channel(gate, server_id, sid, &body).await {
                Ok(created) => {
                    submitting.set(false);
                    toaster.push(
                        ToastVariant::Success,
                        format!("Created “{trimmed}” (cid {})", created.cid),
                        None,
                    );
                    on_created.call(());
                }
                Err(e) => {
                    submitting.set(false);
                    error.set(Some(format_error(&e)));
                }
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "create-channel-title",
                div { class: "modal-header",
                    h2 { id: "create-channel-title", "New channel" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not create channel".to_string(),
                            "{msg}"
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Name" }
                        input {
                            class: "input",
                            value: "{name.read()}",
                            placeholder: "Lobby",
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Parent" }
                        select {
                            class: "input",
                            value: "{parent.read()}",
                            onchange: move |e| {
                                if let Ok(v) = e.value().parse::<i64>() {
                                    parent.set(v);
                                }
                            },
                            option { value: "0", "Top level" }
                            for (cid, label) in parents.iter() {
                                option { value: "{cid}", "{label}" }
                            }
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Topic (optional)" }
                        input {
                            class: "input",
                            value: "{topic.read()}",
                            oninput: move |e| topic.set(e.value()),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Password (optional)" }
                        input {
                            class: "input",
                            r#type: "password",
                            value: "{password.read()}",
                            autocomplete: "new-password",
                            oninput: move |e| password.set(e.value()),
                        }
                    }
                    PermanenceField {
                        value: *permanence.read(),
                        on_change: EventHandler::new(move |p: Permanence| permanence.set(p)),
                    }
                    label { class: "field-inline",
                        input {
                            r#type: "checkbox",
                            checked: *default_flag.read(),
                            oninput: move |e| default_flag.set(e.value() == "true"),
                        }
                        " Default channel"
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        "Create channel"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct EditChannelModalProps {
    server_id: i64,
    sid: i64,
    node: ChannelTreeNode,
    channels: Vec<ChannelTreeNode>,
    on_close: EventHandler<()>,
    on_saved: EventHandler<()>,
}

#[component]
fn EditChannelModal(props: EditChannelModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_saved = props.on_saved;
    let node = props.node.clone();
    let cid = node.cid;
    let parents = parent_options(&props.channels, Some(cid));

    let mut name: Signal<String> = use_signal(|| node.channel_name.clone());
    let mut topic: Signal<String> = use_signal(|| node.channel_topic.clone());
    let mut password: Signal<String> = use_signal(String::new);
    let mut parent: Signal<i64> = use_signal(|| node.pid);
    let mut permanence: Signal<Permanence> = use_signal(|| Permanence::from_node(&node));
    let mut default_flag: Signal<bool> = use_signal(|| node.channel_flag_default != 0);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let original_pid = node.pid;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed = name.read().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("Name is required.".into()));
            return;
        }
        let new_parent = *parent.read();
        if would_cycle(&props.channels, cid, new_parent) {
            error.set(Some(
                "Cannot move a channel under itself or one of its children.".into(),
            ));
            return;
        }
        submitting.set(true);
        error.set(None);
        let (perm, semi, temp) = permanence.read().flags();
        let topic_trim = topic.read().trim().to_string();
        let password_trim = password.read();
        let mut edit = ch_api::ChannelEditRequest {
            channel_name: Some(trimmed.clone()),
            channel_topic: Some(topic_trim),
            channel_flag_permanent: Some(perm),
            channel_flag_semi_permanent: Some(semi),
            channel_flag_temporary: Some(temp),
            channel_flag_default: Some(if *default_flag.read() { 1 } else { 0 }),
            ..Default::default()
        };
        if !password_trim.is_empty() {
            edit.channel_password = Some(password_trim.clone());
        }
        let move_body = if new_parent != original_pid {
            Some(ch_api::ChannelMoveRequest {
                cpid: new_parent,
                order: None,
            })
        } else {
            None
        };
        let gate = gate.clone();
        let server_id = props.server_id;
        let sid = props.sid;
        spawn(async move {
            if let Err(e) = ch_api::edit_channel(gate.clone(), server_id, sid, cid, &edit).await {
                submitting.set(false);
                error.set(Some(format_error(&e)));
                return;
            }
            if let Some(body) = move_body
                && let Err(e) = ch_api::move_channel(gate, server_id, sid, cid, &body).await
            {
                submitting.set(false);
                error.set(Some(format_error(&e)));
                return;
            }
            submitting.set(false);
            toaster.push(ToastVariant::Success, format!("Updated “{trimmed}”"), None);
            on_saved.call(());
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "edit-channel-title",
                div { class: "modal-header",
                    h2 { id: "edit-channel-title", "Edit channel" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not update channel".to_string(),
                            "{msg}"
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Name" }
                        input {
                            class: "input",
                            value: "{name.read()}",
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Parent" }
                        select {
                            class: "input",
                            value: "{parent.read()}",
                            onchange: move |e| {
                                if let Ok(v) = e.value().parse::<i64>() {
                                    parent.set(v);
                                }
                            },
                            option { value: "0", "Top level" }
                            for (id, label) in parents.iter() {
                                option { value: "{id}", selected: *id == original_pid, "{label}" }
                            }
                        }
                        span { class: "field-help", "Changing parent calls POST …/channels/{cid}/move (not the edit body)." }
                    }
                    label { class: "field",
                        span { class: "field-label", "Topic" }
                        input {
                            class: "input",
                            value: "{topic.read()}",
                            oninput: move |e| topic.set(e.value()),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Password" }
                        input {
                            class: "input",
                            r#type: "password",
                            value: "{password.read()}",
                            autocomplete: "new-password",
                            placeholder: "Leave blank to keep the current password",
                            oninput: move |e| password.set(e.value()),
                        }
                    }
                    PermanenceField {
                        value: *permanence.read(),
                        on_change: EventHandler::new(move |p: Permanence| permanence.set(p)),
                    }
                    label { class: "field-inline",
                        input {
                            r#type: "checkbox",
                            checked: *default_flag.read(),
                            oninput: move |e| default_flag.set(e.value() == "true"),
                        }
                        " Default channel"
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        "Save changes"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DeleteChannelModalProps {
    server_id: i64,
    sid: i64,
    node: ChannelTreeNode,
    on_close: EventHandler<()>,
    on_deleted: EventHandler<()>,
}

#[component]
fn DeleteChannelModal(props: DeleteChannelModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_deleted = props.on_deleted;
    let cid = props.node.cid;
    let channel_name = props.node.channel_name.clone();

    let mut force: Signal<bool> = use_signal(|| true);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_delete = move |_| {
        if *submitting.read() {
            return;
        }
        submitting.set(true);
        error.set(None);
        let gate = gate.clone();
        let server_id = props.server_id;
        let sid = props.sid;
        let force_flag = *force.read();
        let channel_name = channel_name.clone();
        spawn(async move {
            match ch_api::delete_channel(gate, server_id, sid, cid, force_flag).await {
                Ok(()) => {
                    submitting.set(false);
                    toaster.push(
                        ToastVariant::Success,
                        format!("Deleted “{channel_name}”"),
                        None,
                    );
                    on_deleted.call(());
                }
                Err(e) => {
                    submitting.set(false);
                    error.set(Some(format_error(&e)));
                }
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            div {
                class: "modal modal-sm",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "delete-channel-title",
                div { class: "modal-header",
                    h2 { id: "delete-channel-title", "Delete channel" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not delete channel".to_string(),
                            "{msg}"
                        }
                    }
                    p { "Delete “{props.node.channel_name}”? This cannot be undone." }
                    label { class: "field-inline",
                        input {
                            r#type: "checkbox",
                            checked: *force.read(),
                            oninput: move |e| force.set(e.value() == "true"),
                        }
                        " Kick occupants and delete (force=1)"
                    }
                    p { class: "field-help",
                        "Uncheck to send force=0 — the TeamSpeak server rejects the delete if anyone is still in the channel."
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        loading: *submitting.read(),
                        onclick: on_delete,
                        "Delete channel"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PermanenceFieldProps {
    value: Permanence,
    on_change: EventHandler<Permanence>,
}

#[component]
fn PermanenceField(props: PermanenceFieldProps) -> Element {
    let current = props.value;
    rsx! {
        fieldset { class: "field",
            legend { class: "field-label", "Lifetime" }
            label { class: "field-inline",
                input {
                    r#type: "radio",
                    name: "channel-permanence",
                    checked: current == Permanence::Permanent,
                    oninput: move |_| props.on_change.call(Permanence::Permanent),
                }
                " Permanent"
            }
            label { class: "field-inline",
                input {
                    r#type: "radio",
                    name: "channel-permanence",
                    checked: current == Permanence::SemiPermanent,
                    oninput: move |_| props.on_change.call(Permanence::SemiPermanent),
                }
                " Semi-permanent"
            }
            label { class: "field-inline",
                input {
                    r#type: "radio",
                    name: "channel-permanence",
                    checked: current == Permanence::Temporary,
                    oninput: move |_| props.on_change.call(Permanence::Temporary),
                }
                " Temporary"
            }
        }
    }
}

fn group_by_parent(rows: &[ChannelTreeNode]) -> Arc<HashMap<i64, Vec<ChannelTreeNode>>> {
    let mut map: HashMap<i64, Vec<ChannelTreeNode>> = HashMap::new();
    for c in rows.iter().cloned() {
        map.entry(c.pid).or_default().push(c);
    }
    for kids in map.values_mut() {
        kids.sort_by_key(|c| c.channel_order);
    }
    Arc::new(map)
}

fn group_clients(rows: &[ClientListItem]) -> Arc<HashMap<i64, Vec<ClientListItem>>> {
    let mut map: HashMap<i64, Vec<ClientListItem>> = HashMap::new();
    for c in rows.iter().cloned() {
        if c.client_type == 1 {
            // ServerQuery clients (type 1) are admin-tooling slots — hide
            // them from the channel-tree roster the same way the desktop
            // client does.
            continue;
        }
        map.entry(c.cid).or_default().push(c);
    }
    Arc::new(map)
}

fn siblings_of(rows: &[ChannelTreeNode], pid: i64) -> Vec<ChannelTreeNode> {
    let mut kids: Vec<ChannelTreeNode> = rows.iter().filter(|c| c.pid == pid).cloned().collect();
    kids.sort_by_key(|c| c.channel_order);
    kids
}

/// `order` for `channelmove` is the upstream sort-after channel id.
/// Moving up inserts before the previous sibling (same `order` that
/// sibling currently uses). Moving down sorts after the next sibling.
fn sibling_move_order(siblings: &[ChannelTreeNode], cid: i64, up: bool) -> Option<i64> {
    let idx = siblings.iter().position(|c| c.cid == cid)?;
    if up {
        if idx == 0 {
            return None;
        }
        Some(siblings[idx - 1].channel_order)
    } else {
        let next = siblings.get(idx + 1)?;
        Some(next.cid)
    }
}

fn parent_options(rows: &[ChannelTreeNode], exclude_cid: Option<i64>) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = rows
        .iter()
        .filter(|c| !is_spacer(&c.channel_name))
        .filter(|c| exclude_cid != Some(c.cid))
        .filter(|c| exclude_cid.is_none_or(|ex| !would_cycle(rows, ex, c.cid)))
        .map(|c| (c.cid, c.channel_name.clone()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// `true` when `new_parent` is `cid` itself or a descendant of `cid`.
fn would_cycle(rows: &[ChannelTreeNode], cid: i64, new_parent: i64) -> bool {
    if new_parent == 0 {
        return false;
    }
    if new_parent == cid {
        return true;
    }
    let mut cursor = new_parent;
    let mut guard = 0usize;
    while cursor != 0 && guard < rows.len() + 1 {
        if cursor == cid {
            return true;
        }
        cursor = rows
            .iter()
            .find(|c| c.cid == cursor)
            .map(|c| c.pid)
            .unwrap_or(0);
        guard += 1;
    }
    false
}

/// Visual treatment for a TeamSpeak spacer channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpacerKind {
    Line,
    Dashline,
    Dotline,
    Left,
    Center,
    Right,
}

/// Recognise TS spacer channels — spec §27.2 `[<prefix>spacer<n>]<text>`,
/// the `[*l/r/c]…` shorthand, or names made entirely of separator glyphs.
fn is_spacer(name: &str) -> bool {
    spacer_kind(name).is_some()
}

fn spacer_kind(name: &str) -> Option<SpacerKind> {
    if let Some(kind) = spacer_from_bracket(name) {
        return Some(kind);
    }
    if let Some(kind) = spacer_from_shorthand(name) {
        return Some(kind);
    }
    if is_glyph_spacer(name) {
        return Some(SpacerKind::Line);
    }
    None
}

/// Spec §27.2: `^\[([lcr]?\*?)spacer\d*\](.*)$/i`.
fn spacer_from_bracket(name: &str) -> Option<SpacerKind> {
    let bytes = name.as_bytes();
    if bytes.first().copied() != Some(b'[') {
        return None;
    }
    let close = name.find(']')?;
    let inside = &name[1..close];
    let lower = inside.to_ascii_lowercase();
    let spacer_pos = lower.find("spacer")?;
    let prefix = &lower[..spacer_pos];
    if !matches!(prefix, "" | "l" | "c" | "r" | "*" | "l*" | "c*" | "r*") {
        return None;
    }
    let trailing = &lower[spacer_pos + "spacer".len()..];
    if !trailing.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let text = &name[close + 1..];
    Some(classify_spacer_text(prefix, text))
}

/// `[*l]`, `[*r]`, `[*c]` — kept so the original tree heuristic still
/// treats those names as dividers even when they omit the `spacer` token.
fn spacer_from_shorthand(name: &str) -> Option<SpacerKind> {
    if name.starts_with("[*l") {
        return Some(SpacerKind::Left);
    }
    if name.starts_with("[*r") {
        return Some(SpacerKind::Right);
    }
    if name.starts_with("[*c") {
        return Some(SpacerKind::Center);
    }
    None
}

fn classify_spacer_text(prefix: &str, text: &str) -> SpacerKind {
    if text == "---" {
        return SpacerKind::Dashline;
    }
    if text == "..." {
        return SpacerKind::Dotline;
    }
    if !text.is_empty()
        && text
            .chars()
            .all(|c| matches!(c, '=' | '-' | '_' | '.' | '─' | '—'))
    {
        return SpacerKind::Line;
    }
    match prefix {
        "c" | "c*" => SpacerKind::Center,
        "r" | "r*" => SpacerKind::Right,
        _ => SpacerKind::Left,
    }
}

fn spacer_text(name: &str) -> String {
    name.find(']')
        .map(|close| name[close + 1..].to_string())
        .unwrap_or_default()
}

fn is_glyph_spacer(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| matches!(c, '─' | '=' | '*' | '-' | '—' | '_' | '.' | '·'))
}

async fn fetch_channels(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<ChannelTreeNode>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels");
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

fn format_error(err: &ApiError) -> String {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut s = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(": ");
                s.push_str(d);
            }
            if let Some(c) = code {
                s.push_str(&format!(" (code {c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::SessionAnonymous => "Loading…".into(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Channel data unavailable in this view.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(cid: i64, pid: i64, order: i64, name: &str) -> ChannelTreeNode {
        ChannelTreeNode {
            cid,
            pid,
            channel_order: order,
            channel_name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn group_by_parent_preserves_channel_order() {
        let rows = vec![ch(2, 0, 5, "B"), ch(1, 0, 1, "A"), ch(3, 1, 1, "A.1")];
        let groups = group_by_parent(&rows);
        let roots: Vec<i64> = groups.get(&0).unwrap().iter().map(|c| c.cid).collect();
        assert_eq!(roots, vec![1, 2], "channel_order asc");
        let kids: Vec<i64> = groups.get(&1).unwrap().iter().map(|c| c.cid).collect();
        assert_eq!(kids, vec![3]);
    }

    #[test]
    fn is_spacer_recognises_named_and_glyph_spacers() {
        assert!(is_spacer("[*spacer]"));
        assert!(is_spacer("[*spacer1]====="));
        assert!(is_spacer("[*l]"));
        assert!(is_spacer("─────"));
        assert!(is_spacer("****"));
        assert!(is_spacer("[cspacer]Hello"));
        assert!(is_spacer("[lspacer]Left"));
        assert!(is_spacer("[rspacer3]Right"));
        assert!(!is_spacer("Lobby"));
        assert!(!is_spacer("Channel 12"));
        assert!(!is_spacer("[chat] room"));
    }

    #[test]
    fn spacer_kind_classifies_alignment_and_rules() {
        assert_eq!(spacer_kind("[cspacer]Hello"), Some(SpacerKind::Center));
        assert_eq!(spacer_kind("[lspacer]Left"), Some(SpacerKind::Left));
        assert_eq!(spacer_kind("[rspacer3]Right"), Some(SpacerKind::Right));
        assert_eq!(spacer_kind("[spacer]---"), Some(SpacerKind::Dashline));
        assert_eq!(spacer_kind("[spacer]..."), Some(SpacerKind::Dotline));
        assert_eq!(spacer_kind("[*spacer1]====="), Some(SpacerKind::Line));
        assert_eq!(spacer_kind("─────"), Some(SpacerKind::Line));
        assert_eq!(spacer_text("[cspacer]Hello"), "Hello");
    }

    #[test]
    fn group_clients_skips_server_query_type() {
        let rows = vec![
            ClientListItem {
                clid: 1,
                cid: 7,
                client_type: 0,
                ..Default::default()
            },
            ClientListItem {
                clid: 2,
                cid: 7,
                client_type: 1,
                ..Default::default()
            },
        ];
        let g = group_clients(&rows);
        assert_eq!(
            g.get(&7).unwrap().len(),
            1,
            "ServerQuery client must be hidden"
        );
        assert_eq!(g.get(&7).unwrap()[0].clid, 1);
    }

    #[test]
    fn sibling_move_order_uses_sort_after_cid() {
        // A (order 0), B (order A), C (order B) — TS channel_order.
        let siblings = vec![ch(10, 0, 0, "A"), ch(20, 0, 10, "B"), ch(30, 0, 20, "C")];
        assert_eq!(sibling_move_order(&siblings, 20, true), Some(0));
        assert_eq!(sibling_move_order(&siblings, 20, false), Some(30));
        assert_eq!(sibling_move_order(&siblings, 10, true), None);
        assert_eq!(sibling_move_order(&siblings, 30, false), None);
        assert_eq!(sibling_move_order(&siblings, 10, false), Some(20));
    }

    #[test]
    fn would_cycle_blocks_self_and_descendants() {
        let rows = vec![
            ch(1, 0, 0, "Root"),
            ch(2, 1, 1, "Child"),
            ch(3, 2, 2, "Grand"),
        ];
        assert!(would_cycle(&rows, 1, 1));
        assert!(would_cycle(&rows, 1, 2));
        assert!(would_cycle(&rows, 1, 3));
        assert!(!would_cycle(&rows, 2, 0));
        assert!(!would_cycle(&rows, 3, 1));
    }
}
