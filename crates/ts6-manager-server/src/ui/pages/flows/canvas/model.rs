//! Canvas model — the palette catalogue, per-kind port shapes, and the
//! graph-mutation helpers the editor signal path shares.
//!
//! The production canvas binds **directly** to the v2 wire types
//! ([`ts6_manager_shared::flows::v2`]) — the spike's `SpikeNode`/`SpikeEdge`
//! are gone. The editor's single source of truth is a `Signal<FlowGraph>`;
//! every mutator here operates on that graph in place so the inspector,
//! the (future) validation banner, and the run-overlay all read one model
//! (canvas-spike report — "the graph *is* the Dioxus signal").

use std::collections::BTreeMap;

use ts6_manager_shared::flows::v2::{
    BranchCase, Edge, EdgeId, FlowGraph, Node, NodeId, NodeKind, Position, TransformOutput,
};
use ts6_manager_shared::flows::{Action, FlowId, Trigger};

/// One palette entry — the seven v2 node kinds (`ui-brief.md` §4.1,
/// `architecture.md` §4). Distinct from [`NodeKind`]: a [`PaletteKind`] is
/// the *choice* the operator drags; it carries no config until placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteKind {
    Trigger,
    Action,
    Branch,
    Parallel,
    Delay,
    Transform,
    Subflow,
}

impl PaletteKind {
    /// Palette order — `Trigger` first; it is the graph entry node.
    pub const ALL: [PaletteKind; 7] = [
        PaletteKind::Trigger,
        PaletteKind::Action,
        PaletteKind::Branch,
        PaletteKind::Parallel,
        PaletteKind::Delay,
        PaletteKind::Transform,
        PaletteKind::Subflow,
    ];

    /// Operator-facing kind name — always shown as text on the node card
    /// (the glyph is decorative, `ui-brief.md` §7).
    pub fn label(self) -> &'static str {
        match self {
            PaletteKind::Trigger => "Trigger",
            PaletteKind::Action => "Action",
            PaletteKind::Branch => "Branch",
            PaletteKind::Parallel => "Parallel",
            PaletteKind::Delay => "Delay",
            PaletteKind::Transform => "Transform",
            PaletteKind::Subflow => "Sub-flow",
        }
    }

    /// Decorative glyph (`ui-brief.md` §7) — `aria-hidden`, never the sole
    /// carrier of meaning.
    ///
    /// Codepoints are the PURA-276 glyph audit's resolved set, not the §7
    /// first picks: four were swapped off emoji-presentation or rare-block
    /// codepoints onto text-presentation Arrows / Geometric-Shapes
    /// codepoints with broad font coverage. See
    /// `docs/flows/v2/canvas-visual-spec.md` §6.
    pub fn glyph(self) -> &'static str {
        match self {
            PaletteKind::Trigger => "\u{21af}",   // ↯  (was ⚡ U+26A1 — emoji)
            PaletteKind::Action => "\u{00bb}",    // »
            PaletteKind::Branch => "\u{22d4}",    // ⋔  (was ⑂ U+2442 — rare)
            PaletteKind::Parallel => "\u{21c9}",  // ⇉
            PaletteKind::Delay => "\u{25f7}",     // ◷  (was ⏱ U+23F1 — emoji)
            PaletteKind::Transform => "\u{21c4}", // ⇄
            PaletteKind::Subflow => "\u{25a3}",   // ▣  (was ⧉ U+29C9 — rare)
        }
    }

    /// One-line palette description.
    pub fn description(self) -> &'static str {
        match self {
            PaletteKind::Trigger => "What starts the flow",
            PaletteKind::Action => "Run one effect",
            PaletteKind::Branch => "Take one of several paths",
            PaletteKind::Parallel => "Fan out over a collection",
            PaletteKind::Delay => "Wait, then continue",
            PaletteKind::Transform => "Reshape data, no side effects",
            PaletteKind::Subflow => "Run another flow",
        }
    }

    /// The wire `kind` discriminant — the slug prefix for generated ids and
    /// the `NodeResult.kind` string.
    pub fn discriminant(self) -> &'static str {
        match self {
            PaletteKind::Trigger => "trigger",
            PaletteKind::Action => "action",
            PaletteKind::Branch => "branch",
            PaletteKind::Parallel => "parallel",
            PaletteKind::Delay => "delay",
            PaletteKind::Transform => "transform",
            PaletteKind::Subflow => "subflow",
        }
    }

    /// The default [`NodeKind`] config a freshly-dropped node carries. The
    /// inspector then edits it; defaults are chosen so a new node is in a
    /// legible (if not yet runnable) state.
    pub fn default_node_kind(self) -> NodeKind {
        match self {
            PaletteKind::Trigger => NodeKind::Trigger {
                config: Trigger::ManualFire,
            },
            PaletteKind::Action => NodeKind::Action {
                config: Action::LogLine {
                    message: String::new(),
                },
            },
            PaletteKind::Branch => NodeKind::Branch {
                cases: vec![BranchCase {
                    label: "case 1".to_string(),
                    when: String::new(),
                }],
            },
            PaletteKind::Parallel => NodeKind::Parallel {
                collection: String::new(),
                sub_flow_id: FlowId(0),
                max_concurrency: 4,
            },
            PaletteKind::Delay => NodeKind::Delay {
                r#for: "30s".to_string(),
            },
            PaletteKind::Transform => NodeKind::Transform {
                output: TransformOutput::Expr(String::new()),
            },
            PaletteKind::Subflow => NodeKind::Subflow {
                sub_flow_id: FlowId(0),
            },
        }
    }
}

/// One output port of a node — `name` is the wire port id used in
/// [`ts6_manager_shared::flows::v2::PortRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub name: String,
    /// The try/catch error seam. Rendered shape/label-distinct, **not**
    /// colour-only (`ui-brief.md` §7, WCAG 1.4.1).
    pub is_err: bool,
}

/// The wire `kind` discriminant of an existing node.
pub fn kind_discriminant(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Trigger { .. } => "trigger",
        NodeKind::Action { .. } => "action",
        NodeKind::Branch { .. } => "branch",
        NodeKind::Parallel { .. } => "parallel",
        NodeKind::Delay { .. } => "delay",
        NodeKind::Transform { .. } => "transform",
        NodeKind::Subflow { .. } => "subflow",
    }
}

/// The matching [`PaletteKind`] for an existing node kind.
pub fn palette_kind(kind: &NodeKind) -> PaletteKind {
    match kind {
        NodeKind::Trigger { .. } => PaletteKind::Trigger,
        NodeKind::Action { .. } => PaletteKind::Action,
        NodeKind::Branch { .. } => PaletteKind::Branch,
        NodeKind::Parallel { .. } => PaletteKind::Parallel,
        NodeKind::Delay { .. } => PaletteKind::Delay,
        NodeKind::Transform { .. } => PaletteKind::Transform,
        NodeKind::Subflow { .. } => PaletteKind::Subflow,
    }
}

/// Whether a node kind accepts inbound edges. Only [`NodeKind::Trigger`] —
/// the unique graph source (`architecture.md` §3.1.1) — does not.
pub fn has_input(kind: &NodeKind) -> bool {
    !matches!(kind, NodeKind::Trigger { .. })
}

/// The ordered output port set for a node kind.
///
/// - `branch` exposes one port per case label, plus an implicit `default`.
/// - the side-effecting kinds (`action`/`transform`/`parallel`/`subflow`)
///   carry an `out` port and a distinct `err` port — the try/catch seam.
/// - `trigger`/`delay` carry a single `out` port.
pub fn output_ports(kind: &NodeKind) -> Vec<PortSpec> {
    let plain = |name: &str| PortSpec {
        name: name.to_string(),
        is_err: false,
    };
    match kind {
        NodeKind::Trigger { .. } | NodeKind::Delay { .. } => vec![plain("out")],
        NodeKind::Branch { cases } => {
            let mut ports: Vec<PortSpec> = cases.iter().map(|c| plain(&c.label)).collect();
            ports.push(plain("default"));
            ports
        }
        NodeKind::Action { .. }
        | NodeKind::Transform { .. }
        | NodeKind::Parallel { .. }
        | NodeKind::Subflow { .. } => vec![
            plain("out"),
            PortSpec {
                name: "err".to_string(),
                is_err: true,
            },
        ],
    }
}

/// Mint a node id unique within `graph` — a stable, legible slug
/// (`architecture.md` §3.1: ids are slugs, referenced by expressions and
/// run records, so they must not collide).
pub fn new_node_id(graph: &FlowGraph, kind: PaletteKind) -> NodeId {
    let prefix = kind.discriminant();
    for n in 1.. {
        let candidate = NodeId(format!("{prefix}_{n}"));
        if !graph.nodes.iter().any(|node| node.id == candidate) {
            return candidate;
        }
    }
    unreachable!("the 1.. range is unbounded")
}

/// Mint an edge id unique within `graph`.
pub fn new_edge_id(graph: &FlowGraph) -> EdgeId {
    for n in 1.. {
        let candidate = EdgeId(format!("e{n}"));
        if !graph.edges.iter().any(|e| e.id == candidate) {
            return candidate;
        }
    }
    unreachable!("the 1.. range is unbounded")
}

/// Place a fresh node of `kind` at `pos` and return its id. Selection is
/// the caller's concern.
pub fn add_node(graph: &mut FlowGraph, kind: PaletteKind, pos: Position) -> NodeId {
    let id = new_node_id(graph, kind);
    graph.nodes.push(Node {
        id: id.clone(),
        label: None,
        position: pos,
        kind: kind.default_node_kind(),
    });
    id
}

/// Remove a node and every edge incident to it.
pub fn remove_node(graph: &mut FlowGraph, id: &NodeId) {
    graph.nodes.retain(|n| &n.id != id);
    graph
        .edges
        .retain(|e| &e.from.node != id && &e.to.node != id);
}

/// Connect `from_node:from_port` → `to_node:in`. Returns the new edge id,
/// or `None` if the connection is rejected: a self-loop, a duplicate, or a
/// target that has no input port (a `trigger`).
pub fn connect(
    graph: &mut FlowGraph,
    from_node: &NodeId,
    from_port: &str,
    to_node: &NodeId,
) -> Option<EdgeId> {
    if from_node == to_node {
        return None;
    }
    let target = graph.nodes.iter().find(|n| &n.id == to_node)?;
    if !has_input(&target.kind) {
        return None;
    }
    let dup = graph
        .edges
        .iter()
        .any(|e| &e.from.node == from_node && e.from.port == from_port && &e.to.node == to_node);
    if dup {
        return None;
    }
    let id = new_edge_id(graph);
    graph.edges.push(Edge {
        id: id.clone(),
        from: ts6_manager_shared::flows::v2::PortRef {
            node: from_node.clone(),
            port: from_port.to_string(),
        },
        to: ts6_manager_shared::flows::v2::PortRef {
            node: to_node.clone(),
            port: "in".to_string(),
        },
        join_policy: Default::default(),
    });
    Some(id)
}

/// Drop edges that dangle off an output port the node no longer exposes —
/// called after a `branch`'s case list is edited so removing a case also
/// removes its wires.
pub fn prune_dangling_edges(graph: &mut FlowGraph) {
    let valid: BTreeMap<NodeId, Vec<String>> = graph
        .nodes
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                output_ports(&n.kind).into_iter().map(|p| p.name).collect(),
            )
        })
        .collect();
    graph.edges.retain(|e| {
        valid
            .get(&e.from.node)
            .map(|ports| ports.iter().any(|p| p == &e.from.port))
            .unwrap_or(false)
    });
}

/// A starter graph for `/flows/new` — a single `trigger` node, already
/// satisfying the exactly-one-trigger invariant (`architecture.md` §3.1.1).
pub fn starter_graph() -> FlowGraph {
    FlowGraph {
        nodes: vec![Node {
            id: NodeId("trigger_1".to_string()),
            label: None,
            position: Position { x: 80.0, y: 120.0 },
            kind: NodeKind::Trigger {
                config: Trigger::ManualFire,
            },
        }],
        edges: Vec::new(),
    }
}

/// Whether the graph already has a trigger — drives the palette's
/// exactly-one-trigger disable (`ui-brief.md` §4.1).
pub fn has_trigger(graph: &FlowGraph) -> bool {
    graph
        .nodes
        .iter()
        .any(|n| matches!(n.kind, NodeKind::Trigger { .. }))
}

// --- "Tidy" auto-layout (ui-brief.md §4.2) -------------------------------

/// Horizontal gap between layout columns (one column per graph layer).
const TIDY_COL_W: f64 = 280.0;
/// Vertical gap between rows within a column.
const TIDY_ROW_H: f64 = 144.0;
/// Top-left margin of the laid-out graph.
const TIDY_MARGIN_X: f64 = 64.0;
const TIDY_MARGIN_Y: f64 = 72.0;

/// The "Tidy" layered (Sugiyama-style) auto-layout (`ui-brief.md` §4.2).
///
/// Each node's column is its **longest-path depth from the trigger** — a
/// node sits one column right of its deepest predecessor — so edges flow
/// left-to-right and never point backward in a valid (acyclic) graph.
/// Nodes within a column stack into rows. Cycle members (which a valid
/// graph never has, but the editor may transiently) and trigger-unreachable
/// nodes fall back to column 0 rather than being lost.
///
/// This is **operator-initiated only** — never automatic — so hand-placed
/// nodes are never moved without intent (`ui-brief.md` §4.2).
pub fn tidy_layout(graph: &mut FlowGraph) {
    use std::collections::HashMap;

    let ids: Vec<NodeId> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let mut succ: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut indeg: HashMap<NodeId, usize> = ids.iter().map(|id| (id.clone(), 0)).collect();
    for e in &graph.edges {
        // Self-loops cannot contribute to a layering — skip them.
        if e.from.node == e.to.node {
            continue;
        }
        succ.entry(e.from.node.clone())
            .or_default()
            .push(e.to.node.clone());
        if let Some(d) = indeg.get_mut(&e.to.node) {
            *d += 1;
        }
    }

    // Kahn traversal — `column` is maxed over every predecessor, so a node
    // lands in the column after its *deepest* predecessor (longest path).
    let mut column: HashMap<NodeId, usize> = HashMap::new();
    let mut queue: Vec<NodeId> = ids
        .iter()
        .filter(|id| indeg.get(*id).copied().unwrap_or(0) == 0)
        .cloned()
        .collect();
    for id in &queue {
        column.insert(id.clone(), 0);
    }
    let mut head = 0;
    while head < queue.len() {
        let cur = queue[head].clone();
        head += 1;
        let cur_col = column.get(&cur).copied().unwrap_or(0);
        if let Some(targets) = succ.get(&cur).cloned() {
            for s in targets {
                let entry = column.entry(s.clone()).or_insert(0);
                *entry = (*entry).max(cur_col + 1);
                if let Some(d) = indeg.get_mut(&s) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push(s);
                    }
                }
            }
        }
    }

    // Group by column (BTreeMap keeps columns ordered) and stack into rows.
    let mut by_column: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
    for id in &ids {
        by_column
            .entry(column.get(id).copied().unwrap_or(0))
            .or_default()
            .push(id.clone());
    }
    for (col, members) in &by_column {
        for (row, id) in members.iter().enumerate() {
            if let Some(n) = graph.nodes.iter_mut().find(|n| &n.id == id) {
                n.position.x = TIDY_MARGIN_X + (*col as f64) * TIDY_COL_W;
                n.position.y = TIDY_MARGIN_Y + (row as f64) * TIDY_ROW_H;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_exposes_one_port_per_case_plus_default() {
        let kind = NodeKind::Branch {
            cases: vec![
                BranchCase {
                    label: "lobby".into(),
                    when: "x".into(),
                },
                BranchCase {
                    label: "afk".into(),
                    when: "y".into(),
                },
            ],
        };
        let ports = output_ports(&kind);
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].name, "lobby");
        assert_eq!(ports[2].name, "default");
        assert!(ports.iter().all(|p| !p.is_err));
    }

    #[test]
    fn side_effecting_kinds_carry_a_distinct_err_port() {
        let kind = NodeKind::Action {
            config: Action::LogLine {
                message: "x".into(),
            },
        };
        let ports = output_ports(&kind);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[1].name, "err");
        assert!(ports[1].is_err);
    }

    #[test]
    fn trigger_has_no_input_port() {
        assert!(!has_input(&NodeKind::Trigger {
            config: Trigger::ManualFire
        }));
        assert!(has_input(&NodeKind::Delay { r#for: "1s".into() }));
    }

    #[test]
    fn node_ids_are_unique_legible_slugs() {
        let mut graph = starter_graph();
        let a = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        let b = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        assert_eq!(a, NodeId("action_1".into()));
        assert_eq!(b, NodeId("action_2".into()));
        assert_ne!(a, b);
    }

    #[test]
    fn connect_rejects_self_loops_duplicates_and_triggerless_targets() {
        let mut graph = starter_graph();
        let trigger = NodeId("trigger_1".into());
        let action = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        // A trigger has no input port — cannot be a target.
        assert!(connect(&mut graph, &action, "out", &trigger).is_none());
        // First connection succeeds.
        assert!(connect(&mut graph, &trigger, "out", &action).is_some());
        // The duplicate is rejected.
        assert!(connect(&mut graph, &trigger, "out", &action).is_none());
        // A self-loop is rejected.
        assert!(connect(&mut graph, &action, "out", &action).is_none());
    }

    #[test]
    fn removing_a_node_drops_its_incident_edges() {
        let mut graph = starter_graph();
        let trigger = NodeId("trigger_1".into());
        let action = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        connect(&mut graph, &trigger, "out", &action);
        assert_eq!(graph.edges.len(), 1);
        remove_node(&mut graph, &action);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.nodes.len(), 1);
    }

    #[test]
    fn pruning_drops_edges_off_a_removed_branch_case() {
        let mut graph = starter_graph();
        let trigger = NodeId("trigger_1".into());
        let branch = add_node(&mut graph, PaletteKind::Branch, Position { x: 0.0, y: 0.0 });
        let action = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        connect(&mut graph, &trigger, "out", &branch);
        // Wire the branch's "case 1" port onward.
        connect(&mut graph, &branch, "case 1", &action);
        assert_eq!(graph.edges.len(), 2);
        // Operator clears the branch's cases — "case 1" port vanishes.
        if let Some(node) = graph.nodes.iter_mut().find(|n| n.id == branch) {
            node.kind = NodeKind::Branch { cases: vec![] };
        }
        prune_dangling_edges(&mut graph);
        // Only the trigger→branch edge survives; the case-1 edge is gone.
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from.node, trigger);
    }

    #[test]
    fn tidy_lays_a_path_out_left_to_right_by_longest_depth() {
        let mut graph = starter_graph();
        let trigger = NodeId("trigger_1".into());
        let a = add_node(&mut graph, PaletteKind::Action, Position { x: 999.0, y: 999.0 });
        let b = add_node(&mut graph, PaletteKind::Delay, Position { x: 5.0, y: 5.0 });
        connect(&mut graph, &trigger, "out", &a);
        connect(&mut graph, &a, "out", &b);

        tidy_layout(&mut graph);

        let x = |id: &NodeId| graph.nodes.iter().find(|n| &n.id == id).unwrap().position.x;
        // trigger → a → b each step one column further right.
        assert!(x(&trigger) < x(&a), "trigger left of a");
        assert!(x(&a) < x(&b), "a left of b");
        assert_eq!(x(&trigger), TIDY_MARGIN_X);
    }

    #[test]
    fn tidy_columns_a_node_after_its_deepest_predecessor() {
        // trigger → a → c  and  trigger → c directly: c must land in the
        // column after `a` (longest path), not after the trigger.
        let mut graph = starter_graph();
        let trigger = NodeId("trigger_1".into());
        let a = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        let c = add_node(&mut graph, PaletteKind::Action, Position { x: 0.0, y: 0.0 });
        connect(&mut graph, &trigger, "out", &a);
        connect(&mut graph, &a, "out", &c);
        connect(&mut graph, &trigger, "out", &c);

        tidy_layout(&mut graph);

        let x = |id: &NodeId| graph.nodes.iter().find(|n| &n.id == id).unwrap().position.x;
        assert!(x(&c) > x(&a), "c sits past its deepest predecessor a");
    }
}
