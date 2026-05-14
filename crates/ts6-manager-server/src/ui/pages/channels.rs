//! `/channels` — channel tree with per-channel client list. PURA-73.
//!
//! - Snapshots `GET /api/servers/{configId}/vs/{sid}/channels` (spec §7.7).
//! - Subscribes to `server:{configId}:channels` for live edits + the
//!   `server:{configId}:clients` topic so the per-channel client roster
//!   updates as people connect/move.
//! - Tree assembly: the REST layer returns a flat list ordered by upstream
//!   `channel_order`. We group by `pid`, recursing from the synthetic root
//!   (channels with `pid == 0`).
//! - Spacers (`channel_name` matching `[*r/l/c]nnnnn[…]` or all-glyph
//!   names) render as horizontal rules — same heuristic the public widget
//!   renderer (PURA-86) will use, kept module-local for now to avoid a
//!   premature shared crate.

use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

#[component]
pub fn ChannelsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let hub = use_ws_hub();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Channels" }
            h1 { "Channels" }
            div { class: "empty",
                div { class: "icon", "#" }
                h3 { "No server selected" }
                p { "Add a server to view its channel tree." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut channels_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_channels(gate, server_id, sid).await }
        }
    });
    let mut clients_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_clients(gate, server_id, sid).await }
        }
    });

    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut channels: Signal<Vec<ChannelTreeNode>> = use_signal(Vec::new);
    let mut clients: Signal<Vec<ClientListItem>> = use_signal(Vec::new);

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

    // WS subscription — refetch on any channel/client edit. The control
    // surface in PURA-71 publishes only on writes; PURA-70a will add the
    // server-notify stream that gives us per-event reductions, at which
    // point this can drop to a targeted update.
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

    rsx! {
        div { class: "crumb", "Channels · {server_name}" }
        h1 { "Channels" }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load channels".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            ChannelsTree {
                channels: channels.read().clone(),
                clients: clients.read().clone(),
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelsTreeProps {
    channels: Vec<ChannelTreeNode>,
    clients: Vec<ClientListItem>,
}

#[component]
fn ChannelsTree(props: ChannelsTreeProps) -> Element {
    if props.channels.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "#" }
                h3 { "No channels yet" }
                p { "Configured channels will appear here." }
            }
        };
    }
    let groups = group_by_parent(&props.channels);
    let clients_by_cid = group_clients(&props.clients);

    rsx! {
        ul { class: "channel-tree",
            "aria-label": "Channel tree",
            ChannelChildren { pid: 0, depth: 0, groups: groups.clone(), clients: clients_by_cid.clone() }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ChannelChildrenProps {
    pid: i64,
    depth: usize,
    groups: Arc<HashMap<i64, Vec<ChannelTreeNode>>>,
    clients: Arc<HashMap<i64, Vec<ClientListItem>>>,
}

#[component]
fn ChannelChildren(props: ChannelChildrenProps) -> Element {
    let kids = props.groups.get(&props.pid).cloned().unwrap_or_default();
    if kids.is_empty() {
        return rsx! { "" };
    }
    rsx! {
        for c in kids.iter() {
            {
                let c = c.clone();
                let cid = c.cid;
                let is_spacer = is_spacer(&c.channel_name);
                let groups = props.groups.clone();
                let clients = props.clients.clone();
                let depth = props.depth;
                let row_clients = clients.get(&cid).cloned().unwrap_or_default();
                rsx! {
                    li { key: "{cid}",
                        class: if is_spacer { "channel-row channel-spacer" } else { "channel-row" },
                        style: "--channel-depth: {depth}",
                        ChannelHeader { node: c.clone(), is_spacer: is_spacer, client_count: row_clients.len() }
                        if !row_clients.is_empty() {
                            ul { class: "channel-clients",
                                for r in row_clients.iter() {
                                    li { key: "client-{r.clid}",
                                        class: "channel-client",
                                        span { class: "client-name", "{r.client_nickname}" }
                                    }
                                }
                            }
                        }
                        ChannelChildren {
                            pid: cid,
                            depth: depth + 1,
                            groups: groups,
                            clients: clients,
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
    is_spacer: bool,
    client_count: usize,
}

#[component]
fn ChannelHeader(props: ChannelHeaderProps) -> Element {
    let n = props.node;
    if props.is_spacer {
        return rsx! {
            div { class: "spacer",
                "aria-hidden": "true",
                span { "{n.channel_name}" }
            }
        };
    }
    rsx! {
        div { class: "channel-header",
            span { class: "channel-name", "{n.channel_name}" }
            span { class: "channel-meta",
                if n.channel_flag_password != 0 { span { "title": "Password protected", "🔒" } }
                if n.channel_flag_default != 0 { span { class: "tag", "default" } }
                if n.channel_flag_permanent != 0 { span { class: "tag", "permanent" } }
                else if n.channel_flag_semi_permanent != 0 { span { class: "tag", "semi-permanent" } }
                span { class: "client-count",
                    "{props.client_count}"
                    if n.channel_maxclients > 0 {
                        " / {n.channel_maxclients}"
                    }
                }
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

/// Recognise TS spacer channels — names of the form `[*spacer]…`,
/// `[*l/r/c]…`, or made entirely of repeated separator glyphs. The TS
/// desktop client treats these as visual dividers, not joinable channels.
fn is_spacer(name: &str) -> bool {
    if name.starts_with("[*spacer") {
        return true;
    }
    if name.starts_with("[*l") || name.starts_with("[*r") || name.starts_with("[*c") {
        return true;
    }
    // All-glyph spacers: fewer than 3 chars or all of "─=*-—_.·".
    if !name.is_empty()
        && name
            .chars()
            .all(|c| matches!(c, '─' | '=' | '*' | '-' | '—' | '_' | '.' | '·'))
    {
        return true;
    }
    false
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

    #[test]
    fn group_by_parent_preserves_channel_order() {
        let rows = vec![
            ChannelTreeNode {
                cid: 2,
                pid: 0,
                channel_order: 5,
                channel_name: "B".into(),
                ..Default::default()
            },
            ChannelTreeNode {
                cid: 1,
                pid: 0,
                channel_order: 1,
                channel_name: "A".into(),
                ..Default::default()
            },
            ChannelTreeNode {
                cid: 3,
                pid: 1,
                channel_order: 1,
                channel_name: "A.1".into(),
                ..Default::default()
            },
        ];
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
        assert!(!is_spacer("Lobby"));
        assert!(!is_spacer("Channel 12"));
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
}
