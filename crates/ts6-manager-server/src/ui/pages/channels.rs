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
//! Channel create / edit / delete / reorder are **not** wired. The control
//! router only mounts `GET …/channels` (see `routes/control/channels.rs`);
//! header and row actions render as disabled “coming soon” affordances so
//! the chrome matches Music bots without inventing write routes.

use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

/// Tooltip on every write-shaped control. The control API has no
/// channel POST/PUT/DELETE/reorder routes yet — do not invent them.
const CHANNEL_WRITES_HINT: &str =
    "Coming soon — channel create, edit, delete, and reorder are not on the control API yet.";

const PAGE_LEDE: &str = "Live channel tree for the selected server. Occupied channels list clients underneath. Create, edit, delete, and reorder wait on write routes that are not shipped yet.";

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
            ChannelsChrome { server_name: None }
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

    let channels_loading = channels_resource.read_unchecked().is_none();
    let on_refresh = EventHandler::new({
        let mut channels_resource = channels_resource;
        let mut clients_resource = clients_resource;
        move |_: ()| {
            channels_resource.restart();
            clients_resource.restart();
        }
    });

    rsx! {
        ChannelsChrome { server_name: Some(server_name) }

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
                on_refresh: on_refresh,
            }
        }
    }
}

#[component]
fn ChannelsChrome(server_name: Option<String>) -> Element {
    let crumb = match server_name.as_deref() {
        Some(name) => format!("Channels · {name}"),
        None => "Channels".into(),
    };
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
                    disabled: true,
                    title: Some(CHANNEL_WRITES_HINT.to_string()),
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
    on_refresh: EventHandler<()>,
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
                p { "Configured channels will appear here once the selected server reports a tree." }
                div { class: "actions",
                    Button {
                        variant: ButtonVariant::Primary,
                        disabled: true,
                        title: Some(CHANNEL_WRITES_HINT.to_string()),
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
                    span { class: "channel-tree-hint", "Read-only · live" }
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
                ChannelChildren { pid: 0, depth: 0, groups: groups.clone(), clients: clients_by_cid.clone() }
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
                rsx! {
                    li { key: "{cid}",
                        class: "{row_class}",
                        style: "--channel-depth: {depth}",
                        ChannelHeader {
                            node: c.clone(),
                            spacer: kind,
                            client_count: row_clients.len(),
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
}

#[component]
fn ChannelHeader(props: ChannelHeaderProps) -> Element {
    let n = props.node;
    if let Some(kind) = props.spacer {
        return rsx! {
            div { class: "channel-row-body",
                ChannelSpacer { name: n.channel_name.clone(), kind: kind }
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
            ComingSoonActions {}
        }
    }
}

#[component]
fn ComingSoonActions() -> Element {
    rsx! {
        div { class: "row-actions channel-row-actions",
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: true,
                title: Some(CHANNEL_WRITES_HINT.to_string()),
                "Edit"
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: true,
                title: Some(CHANNEL_WRITES_HINT.to_string()),
                aria_label: Some("Move up — coming soon".into()),
                "↑"
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: true,
                title: Some(CHANNEL_WRITES_HINT.to_string()),
                aria_label: Some("Move down — coming soon".into()),
                "↓"
            }
            Button {
                variant: ButtonVariant::Danger,
                size: ButtonSize::Small,
                disabled: true,
                title: Some(CHANNEL_WRITES_HINT.to_string()),
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
}
