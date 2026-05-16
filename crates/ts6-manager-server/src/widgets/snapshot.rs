//! Snapshot builder — assembles a [`WidgetData`] payload from upstream
//! `serverinfo` + `channellist` + `clientlist` per spec §27.1.
//!
//! The pipeline is:
//!
//! 1. Group human clients by `cid` (skip query clients, `client_type != 0`).
//! 2. Convert each `channellist` row into a flat [`WidgetChannelNode`]
//!    (with spacer fields populated by [`detect_spacer`]).
//! 3. Link parents via `pid` to assemble the tree (root nodes have `pid==0`).
//! 4. Sort each level by `channel_order` so the tree mirrors the operator's
//!    arrangement in the TS client.
//! 5. Apply the `maxChannelDepth` cap — any node at depth ≥ cap drops its
//!    children entirely (the cap is a hard contract, not just a render hint).
//! 6. If `hideEmptyChannels`, prune real channels with no clients anywhere
//!    in their subtree. Spacers are *always* kept.
//! 7. Apply `showClients=false` by emptying every node's `clients` array
//!    after the tree is final (kept until the end so step 6's "has clients"
//!    check stays correct).
//! 8. Build the [`WidgetServer`] header. `virtualserver_platform` is forced
//!    to `"TeamSpeak"` and `virtualserver_version` to `""` per §7.29.
//!
//! `clients_online` is derived from the human-client count in `clientlist`,
//! not the upstream `virtualserver_clientsonline`, because the
//! `clientsonline` counter on `serverinfo` includes ServerQuery clients
//! we don't expose to the public widget.

use ts6_manager_shared::widgets::{
    SpacerType, WidgetChannelNode, WidgetClient, WidgetData, WidgetServer,
};

use crate::repos::widgets::Widget;
use crate::webquery::models::{ChannelEntry, ClientEntry, ServerInfo};

/// All upstream inputs the builder needs. `WebQueryClient::serverinfo` /
/// `channellist_with_flags(["flags"])` / `clientlist_with_flags(&[])` feed
/// the three fields below; the route layer fans out the calls in parallel.
#[derive(Debug, Clone)]
pub struct WidgetInputs {
    pub server: ServerInfo,
    pub channels: Vec<ChannelEntry>,
    pub clients: Vec<ClientEntry>,
}

/// Build the [`WidgetData`] payload from upstream inputs and the operator's
/// per-widget configuration.
pub fn build_widget_data(widget: &Widget, inputs: WidgetInputs) -> WidgetData {
    let WidgetInputs {
        server,
        channels,
        clients,
    } = inputs;

    let max_depth = widget.maxChannelDepth.max(0) as u32;
    let human_clients: Vec<&ClientEntry> = clients.iter().filter(|c| c.client_type == 0).collect();
    let clients_online = human_clients.len() as u32;
    let mut clients_by_cid: std::collections::HashMap<i64, Vec<WidgetClient>> =
        std::collections::HashMap::new();
    for c in &human_clients {
        clients_by_cid.entry(c.cid).or_default().push(WidgetClient {
            clid: c.clid,
            nickname: c.client_nickname.clone(),
            is_away: c.client_away != 0,
            // Self-reported mic/speaker state — distinct from the operator
            // talker flag (`client_is_talker`). Both are valid reads; the
            // snapshot widget shows the client's own hardware mute state.
            is_muted: c.client_input_muted != 0 || c.client_output_muted != 0,
        });
    }

    let mut nodes: Vec<WidgetChannelNode> = channels
        .iter()
        .map(|ch| {
            let (is_spacer, spacer_type, spacer_text) = detect_spacer(&ch.channel_name);
            WidgetChannelNode {
                cid: ch.cid,
                name: ch.channel_name.clone(),
                has_password: ch.channel_flag_password != 0,
                clients: clients_by_cid.remove(&ch.cid).unwrap_or_default(),
                children: Vec::new(),
                is_spacer,
                spacer_type,
                spacer_text,
            }
        })
        .collect();
    let pids: Vec<i64> = channels.iter().map(|c| c.pid).collect();
    let orders: Vec<i64> = channels.iter().map(|c| c.channel_order).collect();

    let mut tree = link_tree(&mut nodes, &pids, &orders);
    apply_max_depth(&mut tree, max_depth, 1);
    if widget.hideEmptyChannels {
        prune_empty(&mut tree);
    }
    if !widget.showClients {
        clear_clients(&mut tree);
    }
    if !widget.showChannelTree {
        tree.clear();
    }

    let max_clients = u32::try_from(server.virtualserver_maxclients.max(0)).unwrap_or(u32::MAX);
    let uptime_seconds = u64::try_from(server.virtualserver_uptime.max(0)).unwrap_or(0);

    WidgetData {
        name: widget.name.clone(),
        theme: widget.theme.clone(),
        server_config_id: widget.serverConfigId,
        show_channel_tree: widget.showChannelTree,
        show_clients: widget.showClients,
        hide_empty_channels: widget.hideEmptyChannels,
        max_channel_depth: max_depth,
        server: WidgetServer {
            name: server.virtualserver_name,
            clients_online,
            max_clients,
            uptime_seconds,
            // Spec §7.29 redaction. Hard-coded constants — never propagate
            // upstream platform/version to a public surface.
            platform: "TeamSpeak".to_string(),
            version: String::new(),
        },
        channels: tree,
    }
}

/// Spacer regex per spec §27.2: `^\[([lcr]?\*?)spacer\d*\](.*)$/i`.
///
/// Returns `(is_spacer, spacer_type, spacer_text)`. For non-spacer channels
/// returns `(false, SpacerType::None, "")`.
pub fn detect_spacer(channel_name: &str) -> (bool, SpacerType, String) {
    let bytes = channel_name.as_bytes();
    if bytes.first().copied() != Some(b'[') {
        return (false, SpacerType::None, String::new());
    }
    // Walk the bracket: [<prefix>spacer<digits>]<text>
    // prefix ∈ "" | "l" | "c" | "r" | "*" | "l*" | "c*" | "r*"
    let close = match channel_name.find(']') {
        Some(p) => p,
        None => return (false, SpacerType::None, String::new()),
    };
    let inside = &channel_name[1..close];
    let lower = inside.to_ascii_lowercase();
    let spacer_pos = match lower.find("spacer") {
        Some(p) => p,
        None => return (false, SpacerType::None, String::new()),
    };
    let prefix = &lower[..spacer_pos];
    if !is_valid_spacer_prefix(prefix) {
        return (false, SpacerType::None, String::new());
    }
    let trailing = &lower[spacer_pos + "spacer".len()..];
    if !trailing.chars().all(|c| c.is_ascii_digit()) {
        return (false, SpacerType::None, String::new());
    }

    let text = channel_name[close + 1..].to_string();
    let spacer_type = classify_spacer(prefix, &text);
    (true, spacer_type, text)
}

fn is_valid_spacer_prefix(prefix: &str) -> bool {
    matches!(prefix, "" | "l" | "c" | "r" | "*" | "l*" | "c*" | "r*")
}

fn classify_spacer(prefix: &str, text: &str) -> SpacerType {
    if text == "---" {
        return SpacerType::Dashline;
    }
    if text == "..." {
        return SpacerType::Dotline;
    }
    if !text.is_empty() && text.chars().all(|c| matches!(c, '=' | '-' | '_' | '.')) {
        return SpacerType::Line;
    }
    match prefix {
        "c" | "c*" => SpacerType::Center,
        "r" | "r*" => SpacerType::Right,
        _ => SpacerType::Left,
    }
}

/// Link the flat node list into a tree by `pid`. Roots are entries whose
/// `pid` is 0. Each level is sorted by `channel_order` to mirror the
/// operator's arrangement.
fn link_tree(
    nodes: &mut Vec<WidgetChannelNode>,
    pids: &[i64],
    orders: &[i64],
) -> Vec<WidgetChannelNode> {
    debug_assert_eq!(nodes.len(), pids.len());
    debug_assert_eq!(nodes.len(), orders.len());

    let mut indexed: Vec<(i64, i64, WidgetChannelNode)> = nodes
        .drain(..)
        .enumerate()
        .map(|(i, n)| (pids[i], orders[i], n))
        .collect();
    // Build child buckets keyed by parent cid. Owning the nodes outright
    // here keeps the recursion simple — no two-pass borrow split.
    let mut by_parent: std::collections::HashMap<i64, Vec<(i64, WidgetChannelNode)>> =
        std::collections::HashMap::new();
    for (pid, order, node) in indexed.drain(..) {
        by_parent.entry(pid).or_default().push((order, node));
    }
    let mut roots = by_parent.remove(&0).unwrap_or_default();
    roots.sort_by_key(|(o, _)| *o);
    let mut out: Vec<WidgetChannelNode> = roots.into_iter().map(|(_, n)| n).collect();
    for n in &mut out {
        attach_children(n, &mut by_parent);
    }
    out
}

fn attach_children(
    node: &mut WidgetChannelNode,
    by_parent: &mut std::collections::HashMap<i64, Vec<(i64, WidgetChannelNode)>>,
) {
    if let Some(mut kids) = by_parent.remove(&node.cid) {
        kids.sort_by_key(|(o, _)| *o);
        node.children = kids.into_iter().map(|(_, n)| n).collect();
        for child in &mut node.children {
            attach_children(child, by_parent);
        }
    }
}

fn apply_max_depth(nodes: &mut [WidgetChannelNode], max_depth: u32, current_depth: u32) {
    if max_depth == 0 {
        // Pathological 0 cap → no nesting ever. Drop all children.
        for n in nodes.iter_mut() {
            n.children.clear();
        }
        return;
    }
    if current_depth >= max_depth {
        for n in nodes.iter_mut() {
            n.children.clear();
        }
        return;
    }
    for n in nodes.iter_mut() {
        apply_max_depth(&mut n.children, max_depth, current_depth + 1);
    }
}

fn prune_empty(nodes: &mut Vec<WidgetChannelNode>) {
    nodes.retain_mut(|node| {
        prune_empty(&mut node.children);
        if node.is_spacer {
            // Spacers are always preserved per §27.1 step 5.
            return true;
        }
        if !node.clients.is_empty() {
            return true;
        }
        !node.children.is_empty()
    });
}

fn clear_clients(nodes: &mut [WidgetChannelNode]) {
    for n in nodes.iter_mut() {
        n.clients.clear();
        clear_clients(&mut n.children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ch(cid: i64, pid: i64, order: i64, name: &str, password: bool) -> ChannelEntry {
        ChannelEntry {
            cid,
            pid,
            channel_order: order,
            channel_name: name.to_string(),
            channel_flag_password: if password { 1 } else { 0 },
            ..Default::default()
        }
    }

    fn cl(clid: i64, cid: i64, name: &str) -> ClientEntry {
        ClientEntry {
            clid,
            cid,
            client_type: 0,
            client_nickname: name.to_string(),
            ..Default::default()
        }
    }

    fn widget(opts: impl FnOnce(&mut Widget)) -> Widget {
        let now = Utc::now();
        let mut w = Widget {
            id: 1,
            name: "Test".into(),
            token: "tok".into(),
            serverConfigId: 1,
            virtualServerId: 1,
            theme: "dark".into(),
            showChannelTree: true,
            showClients: true,
            hideEmptyChannels: false,
            maxChannelDepth: 5,
            createdAt: now,
            updatedAt: now,
        };
        opts(&mut w);
        w
    }

    fn server_info() -> ServerInfo {
        ServerInfo {
            virtualserver_name: "TS".into(),
            virtualserver_platform: "Linux".into(),
            virtualserver_version: "3.13.7".into(),
            virtualserver_maxclients: 32,
            virtualserver_uptime: 4242,
            virtualserver_total_packetloss_total: 0.0,
            virtualserver_total_ping: 0.0,
        }
    }

    #[test]
    fn detect_spacer_classifies_dashline() {
        let (is, ty, text) = detect_spacer("[cspacer]---");
        assert!(is);
        assert_eq!(ty, SpacerType::Dashline);
        assert_eq!(text, "---");
    }

    #[test]
    fn detect_spacer_classifies_dotline() {
        let (is, ty, _) = detect_spacer("[lspacer1]...");
        assert!(is);
        assert_eq!(ty, SpacerType::Dotline);
    }

    #[test]
    fn detect_spacer_classifies_line_run() {
        for line in ["===", "___", "---", "...", "==-", "_-_-"] {
            let s = format!("[spacer]{line}");
            let (is, ty, _) = detect_spacer(&s);
            assert!(is, "{line}");
            // Repeated dash/dot/equals/underscore reduce to line/dotline/dashline.
            // The exact mapping is asserted in the dedicated tests above; here
            // only the spacer classification matters.
            assert!(matches!(
                ty,
                SpacerType::Line | SpacerType::Dashline | SpacerType::Dotline
            ));
        }
    }

    #[test]
    fn detect_spacer_classifies_text_alignment() {
        let (_, ty, text) = detect_spacer("[cspacer]Hello");
        assert_eq!(ty, SpacerType::Center);
        assert_eq!(text, "Hello");
        let (_, ty, _) = detect_spacer("[rspacer3]Right side");
        assert_eq!(ty, SpacerType::Right);
        let (_, ty, _) = detect_spacer("[lspacer]Left side");
        assert_eq!(ty, SpacerType::Left);
        let (_, ty, _) = detect_spacer("[spacer]No prefix");
        assert_eq!(ty, SpacerType::Left);
    }

    #[test]
    fn detect_spacer_rejects_real_channels() {
        for n in ["Lobby", "[Lobby]", "[chat] room", "[s] not spacer"] {
            let (is, _, _) = detect_spacer(n);
            assert!(!is, "expected `{n}` to NOT be a spacer");
        }
    }

    #[test]
    fn build_assembles_tree_in_order_and_filters_query_clients() {
        let widget = widget(|_| {});
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![
                ch(2, 0, 2, "Voice", false),
                ch(1, 0, 1, "Lobby", false),
                ch(3, 1, 1, "Sub", false),
            ],
            clients: vec![
                cl(10, 1, "Alice"),
                cl(11, 1, "Bob"),
                cl(99, 1, "QueryBot"), // skipped
                cl(12, 3, "Carol"),
            ],
        };
        // Make the query bot have a non-zero client_type to skip it.
        let mut inputs = inputs;
        inputs.clients[2].client_type = 1;
        let data = build_widget_data(&widget, inputs);

        // Roots sorted by channel_order: Lobby(1) before Voice(2).
        assert_eq!(data.channels.len(), 2);
        assert_eq!(data.channels[0].name, "Lobby");
        assert_eq!(data.channels[1].name, "Voice");
        // Query bot (clid=99) excluded from the count.
        assert_eq!(data.server.clients_online, 3);
        // Lobby's children include Sub.
        assert_eq!(data.channels[0].children.len(), 1);
        assert_eq!(data.channels[0].children[0].cid, 3);
        // Sub has Carol; Lobby has Alice + Bob in order.
        assert_eq!(
            data.channels[0]
                .clients
                .iter()
                .map(|c| c.nickname.as_str())
                .collect::<Vec<_>>(),
            vec!["Alice", "Bob"]
        );
        assert_eq!(
            data.channels[0].children[0]
                .clients
                .iter()
                .map(|c| c.nickname.as_str())
                .collect::<Vec<_>>(),
            vec!["Carol"]
        );
    }

    #[test]
    fn build_redacts_platform_and_version() {
        let widget = widget(|_| {});
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![],
            clients: vec![],
        };
        let data = build_widget_data(&widget, inputs);
        assert_eq!(data.server.platform, "TeamSpeak");
        assert_eq!(data.server.version, "");
    }

    #[test]
    fn build_marks_password_channel_and_spacer() {
        let widget = widget(|_| {});
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![
                ch(1, 0, 1, "Lobby", true),
                ch(2, 0, 2, "[cspacer]Section", false),
            ],
            clients: vec![],
        };
        let data = build_widget_data(&widget, inputs);
        assert!(data.channels[0].has_password);
        assert!(data.channels[1].is_spacer);
        assert_eq!(data.channels[1].spacer_type, SpacerType::Center);
    }

    #[test]
    fn build_caps_max_depth() {
        let widget = widget(|w| w.maxChannelDepth = 2);
        // 1 → 2 → 3 → 4 chain. With cap=2, depth-2 (cid 2) keeps no children.
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![
                ch(1, 0, 1, "L1", false),
                ch(2, 1, 1, "L2", false),
                ch(3, 2, 1, "L3", false),
                ch(4, 3, 1, "L4", false),
            ],
            clients: vec![],
        };
        let data = build_widget_data(&widget, inputs);
        assert_eq!(data.channels[0].name, "L1");
        assert_eq!(data.channels[0].children[0].name, "L2");
        // Cap=2 means depth-2 (L2) drops its children.
        assert!(data.channels[0].children[0].children.is_empty());
    }

    #[test]
    fn build_hide_empty_keeps_spacers_and_subtrees_with_clients() {
        let widget = widget(|w| w.hideEmptyChannels = true);
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![
                ch(1, 0, 1, "Empty", false),
                ch(2, 0, 2, "[cspacer]Section", false),
                ch(3, 0, 3, "ParentOfClient", false),
                ch(4, 3, 1, "WithClient", false),
                ch(5, 0, 4, "EntirelyEmptyTree", false),
                ch(6, 5, 1, "AlsoEmpty", false),
            ],
            clients: vec![cl(10, 4, "Alice")],
        };
        let data = build_widget_data(&widget, inputs);
        let names: Vec<&str> = data.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["[cspacer]Section", "ParentOfClient"]);
        assert_eq!(data.channels[1].children.len(), 1);
    }

    #[test]
    fn build_show_clients_false_clears_client_arrays_but_keeps_tree() {
        let widget = widget(|w| w.showClients = false);
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![ch(1, 0, 1, "Lobby", false)],
            clients: vec![cl(10, 1, "Alice")],
        };
        let data = build_widget_data(&widget, inputs);
        // Tree stays.
        assert_eq!(data.channels.len(), 1);
        // Client array is empty.
        assert!(data.channels[0].clients.is_empty());
        // But the header count uses the upstream count, not the rendered one.
        assert_eq!(data.server.clients_online, 1);
    }

    #[test]
    fn build_show_channel_tree_false_drops_all_channels() {
        let widget = widget(|w| w.showChannelTree = false);
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![ch(1, 0, 1, "Lobby", false)],
            clients: vec![],
        };
        let data = build_widget_data(&widget, inputs);
        assert!(data.channels.is_empty());
    }

    #[test]
    fn build_marks_away_and_muted() {
        let widget = widget(|_| {});
        let mut alice = cl(10, 1, "Alice");
        alice.client_away = 1;
        let mut bob = cl(11, 1, "Bob");
        bob.client_input_muted = 1;
        let mut carol = cl(12, 1, "Carol");
        carol.client_output_muted = 1;
        let inputs = WidgetInputs {
            server: server_info(),
            channels: vec![ch(1, 0, 1, "Lobby", false)],
            clients: vec![alice, bob, carol],
        };
        let data = build_widget_data(&widget, inputs);
        let lobby = &data.channels[0];
        assert!(lobby.clients[0].is_away);
        assert!(!lobby.clients[0].is_muted);
        assert!(lobby.clients[1].is_muted);
        assert!(lobby.clients[2].is_muted);
    }
}
