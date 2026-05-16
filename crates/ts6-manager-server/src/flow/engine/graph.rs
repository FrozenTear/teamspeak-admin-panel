//! v2 graph execution engine — flow-engine redesign (PURA-266).
//!
//! Replaces `flow/engine.rs`'s linear `for action in flow.actions` loop with
//! a **topological scheduler** (`docs/flows/v2/architecture.md` §5–§6):
//!
//! - **Readiness is event-driven** (§5.1). A run holds a settled-state per
//!   node and a control signal per edge. A node becomes *ready* when its
//!   inbound edges have settled per its [`JoinPolicy`]; a ready node is
//!   dispatched as a `tokio` task bounded by a per-run semaphore (§6.5).
//! - **Edge control signals** are `active` / `skipped` (§5.2). A branch's
//!   not-taken ports and an errored node's `out` port both emit `skipped`;
//!   an errored node's `err` port emits `active` carrying the error
//!   document — that is the try/catch seam (§6.2).
//! - **Per-node run records** ([`NodeResult`]) with an 8 kB `output` cap
//!   feed the run row's `nodeResults` (§6.4).
//! - **One engine.** A legacy linear flow is loaded through the projection
//!   shim ([`decode_flow_data`]) into a degenerate path graph and run by
//!   this same scheduler — observably identical to the old serial loop
//!   (§5.4).
//!
//! The three v1.1 "storage-full" boundaries map onto SurrealDB equivalents
//! at the repo layer; this module surfaces run-level failure as the
//! `errored` terminal status (§6.3).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use ts6_manager_shared::flows::v2::{
    BranchCase, Edge, EdgeId, FlowGraph, JoinPolicy, Node, NodeId, NodeKind, NodeResult,
    NodeStatus, TransformOutput, ValidationError, ValidationWarning, decode_flow_data,
};
use ts6_manager_shared::flows::{Action, FlowId, FlowRunId, FlowRunStatus};

use super::expr::{self, Blackboard};
use super::{ActionContext, ActionDispatcher, ActionOutcome};
use crate::db::Database;
use crate::repos::bot_flows;

/// Per-run node-task semaphore (`architecture.md` §6.5) — bounds concurrent
/// node tasks within one run, capping the blast radius of a wide static
/// fan-out.
const NODE_SEMAPHORE: usize = 8;

/// Per-node `output` byte cap (`architecture.md` §6.4). Over-cap stores a
/// `{ "_truncated": true }` marker.
const OUTPUT_CAP: usize = 8 * 1024;

/// Sub-flow nesting depth backstop (`architecture.md` §4.7 / §6.6).
const MAX_DEPTH: usize = 5;

/// `parallel` collection element cap (`architecture.md` §4.4).
const COLLECTION_CAP: usize = 256;

/// `delay` upper bound (`architecture.md` §4.5) — 15 minutes.
const DELAY_CAP: Duration = Duration::from_secs(15 * 60);

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Everything one graph run needs that is not the graph itself. Cheap to
/// clone — every field is `Arc`-shared or a small value.
#[derive(Clone)]
pub struct GraphDeps {
    pub db: Arc<Database>,
    pub dispatcher: Arc<dyn ActionDispatcher>,
    pub flow_id: FlowId,
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
}

/// The terminal result of a top-level graph run — what the engine writes
/// onto the `bot_flow_run` row.
#[derive(Debug)]
pub struct GraphRunOutcome {
    /// `Ok` or `Errored` (`architecture.md` §6.3). `interrupted` /
    /// `skipped_disabled` are decided by the caller, not the scheduler.
    pub status: FlowRunStatus,
    /// First unhandled-error message, when `status == Errored`.
    pub error: Option<String>,
    /// One entry per node that settled (§6.4).
    pub node_results: Vec<NodeResult>,
}

/// Run one graph instance to completion. The trigger node emits
/// `trigger_doc` (`architecture.md` §4.1); for a sub-flow invocation the
/// caller passes the sub-flow's input payload here (§4.7).
pub async fn run_graph(graph: FlowGraph, trigger_doc: Value, deps: GraphDeps) -> GraphRunOutcome {
    let inner = execute(graph, trigger_doc, deps, 0).await;
    GraphRunOutcome {
        status: inner.status,
        error: inner.error,
        node_results: inner.node_results,
    }
}

/// Structural validation (`architecture.md` §3.1). Runs defensively before
/// every execution; the HTTP routes layer reuses it for `POST /api/flows`
/// and `POST /api/flows/validate`. Returns the *first* failure as a flat
/// string — the engine's pre-run gate. For the structured `errors[]` /
/// `warnings[]` array the canvas renders inline, see [`validate_graph`].
///
/// Sub-flow-reference acyclicity (§3.1.6) needs the full flow set and stays
/// a write-time check — the engine backs it with a runtime depth cap
/// ([`MAX_DEPTH`]).
pub fn validate(graph: &FlowGraph) -> Result<(), String> {
    match validate_graph(graph).errors.into_iter().next() {
        Some(e) => Err(e.message),
        None => Ok(()),
    }
}

/// The structured structural validation report (`http-api.md` §3.1) — every
/// failure, not just the first, plus non-blocking advisories.
pub struct StructuralReport {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationWarning>,
}

/// Structured structural validation — collects *all* failures with their
/// stable `code`s so the canvas can render them inline. The check set is
/// identical to [`validate`]'s; this variant categorises and accumulates
/// rather than short-circuiting. Expression / duration parsing is a
/// separate write-time pass ([`validate_expressions`]) so the engine's
/// pre-run gate stays purely structural.
pub fn validate_graph(graph: &FlowGraph) -> StructuralReport {
    let mut errors: Vec<ValidationError> = Vec::new();
    let mut warnings: Vec<ValidationWarning> = Vec::new();

    // Size caps (`architecture.md` §3.1).
    if graph.nodes.len() > 64 {
        errors.push(ValidationError::new(
            "size_exceeded",
            format!("graph has {} nodes (cap 64)", graph.nodes.len()),
        ));
    }
    if graph.edges.len() > 128 {
        errors.push(ValidationError::new(
            "size_exceeded",
            format!("graph has {} edges (cap 128)", graph.edges.len()),
        ));
    }

    // Unique node ids.
    let mut seen: HashSet<&NodeId> = HashSet::new();
    for node in &graph.nodes {
        if !seen.insert(&node.id) {
            errors.push(
                ValidationError::new(
                    "duplicate_node",
                    format!("duplicate node id `{}`", node.id.0),
                )
                .at_node(&node.id),
            );
        }
    }
    let by_id: HashMap<&NodeId, &Node> = graph.nodes.iter().map(|n| (&n.id, n)).collect();

    // Exactly one trigger node.
    let triggers: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Trigger { .. }))
        .collect();
    match triggers.len() {
        1 => {}
        0 => errors.push(ValidationError::new(
            "no_trigger",
            "graph must have exactly one trigger node, found 0",
        )),
        n => errors.push(
            ValidationError::new(
                "multiple_triggers",
                format!("graph must have exactly one trigger node, found {n}"),
            )
            .with_nodes(triggers.iter().map(|t| t.id.0.clone()).collect()),
        ),
    }
    let trigger_id: Option<&NodeId> = (triggers.len() == 1).then(|| &triggers[0].id);

    // Ports exist and directions match; the trigger has no inbound edge.
    for edge in &graph.edges {
        let from = by_id.get(&edge.from.node);
        let to = by_id.get(&edge.to.node);
        if from.is_none() {
            errors.push(
                ValidationError::new(
                    "unknown_port",
                    format!(
                        "edge `{}` from unknown node `{}`",
                        edge.id.0, edge.from.node.0
                    ),
                )
                .at_edge(&edge.id),
            );
        }
        if to.is_none() {
            errors.push(
                ValidationError::new(
                    "unknown_port",
                    format!("edge `{}` to unknown node `{}`", edge.id.0, edge.to.node.0),
                )
                .at_edge(&edge.id),
            );
        }
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        if !output_ports(&from.kind)
            .iter()
            .any(|p| p == &edge.from.port)
        {
            errors.push(
                ValidationError::new(
                    "unknown_port",
                    format!(
                        "edge `{}`: `{}` is not an output port of node `{}`",
                        edge.id.0, edge.from.port, from.id.0
                    ),
                )
                .at_edge(&edge.id),
            );
        }
        if !is_input_port(&to.kind, &edge.to.port) {
            errors.push(
                ValidationError::new(
                    "unknown_port",
                    format!(
                        "edge `{}`: `{}` is not an input port of node `{}`",
                        edge.id.0, edge.to.port, to.id.0
                    ),
                )
                .at_edge(&edge.id),
            );
        }
        if Some(&edge.to.node) == trigger_id {
            errors.push(
                ValidationError::new(
                    "unknown_port",
                    "the trigger node cannot have an inbound edge",
                )
                .at_edge(&edge.id),
            );
        }
    }

    // Every non-trigger node has its `in` port connected.
    for node in &graph.nodes {
        if Some(&node.id) == trigger_id {
            continue;
        }
        if !graph.edges.iter().any(|e| e.to.node == node.id) {
            errors.push(
                ValidationError::new(
                    "port_unconnected",
                    format!("node `{}` has no inbound edge", node.id.0),
                )
                .at_node(&node.id),
            );
        }
    }

    // Acyclic — Kahn's algorithm over the edge set.
    let mut indegree: HashMap<&NodeId, usize> =
        graph.nodes.iter().map(|n| (&n.id, 0usize)).collect();
    for edge in &graph.edges {
        *indegree.entry(&edge.to.node).or_insert(0) += 1;
    }
    let mut queue: VecDeque<&NodeId> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut processed = 0usize;
    while let Some(id) = queue.pop_front() {
        processed += 1;
        for edge in graph.edges.iter().filter(|e| &e.from.node == id) {
            let d = indegree.entry(&edge.to.node).or_insert(0);
            *d = d.saturating_sub(1);
            if *d == 0 {
                queue.push_back(&edge.to.node);
            }
        }
    }
    if processed != graph.nodes.len() {
        let path = find_cycle_path(graph);
        let message = if path.is_empty() {
            "graph contains a cycle".to_string()
        } else {
            format!("graph contains a cycle: {}", path.join(" → "))
        };
        // Drop the trailing repeat of the entry node for the `nodes` set.
        let mut nodes = path;
        if nodes.len() > 1 && nodes.first() == nodes.last() {
            nodes.pop();
        }
        errors.push(ValidationError::new("graph_cycle", message).with_nodes(nodes));
    }

    // Every non-trigger node is reachable from the trigger.
    if let Some(trigger_id) = trigger_id {
        let mut reachable: HashSet<&NodeId> = HashSet::new();
        let mut frontier = vec![trigger_id];
        while let Some(id) = frontier.pop() {
            if !reachable.insert(id) {
                continue;
            }
            for edge in graph.edges.iter().filter(|e| &e.from.node == id) {
                frontier.push(&edge.to.node);
            }
        }
        for node in &graph.nodes {
            if !reachable.contains(&node.id) {
                errors.push(
                    ValidationError::new(
                        "unreachable_node",
                        format!("node `{}` is unreachable from the trigger", node.id.0),
                    )
                    .at_node(&node.id),
                );
            }
        }
    }

    // Advisory — a named `branch` case port with no outgoing edge is a dead
    // route. The implicit `default` port is deliberately not flagged: an
    // unwired `default` is the idiomatic "drop on no match".
    for node in &graph.nodes {
        if let NodeKind::Branch { cases } = &node.kind {
            for case in cases {
                let wired = graph
                    .edges
                    .iter()
                    .any(|e| e.from.node == node.id && e.from.port == case.label);
                if !wired {
                    warnings.push(ValidationWarning::at_node(
                        "unconnected_case",
                        format!(
                            "branch node `{}` case `{}` has no outgoing edge",
                            node.id.0, case.label
                        ),
                        &node.id,
                    ));
                }
            }
        }
    }

    StructuralReport { errors, warnings }
}

/// Write-time expression / duration validation (`http-api.md` §3.1 —
/// `bad_expression`, `bad_duration`). Kept apart from [`validate_graph`] so
/// the engine's pre-run [`validate`] gate stays purely structural and never
/// rejects a graph the v1.1 engine would have run.
pub fn validate_expressions(graph: &FlowGraph) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    // Returns the error rather than pushing it, so it captures nothing
    // mutably — the arms below also push to `errors` directly, and a
    // closure holding a live `&mut errors` would conflict with that.
    let bad_expr = |node: &NodeId, label: &str, src: &str| -> Option<ValidationError> {
        expr::parse_check(src).err().map(|e| {
            ValidationError::new("bad_expression", format!("node `{}` {label}: {e}", node.0))
                .at_node(node)
        })
    };
    for node in &graph.nodes {
        match &node.kind {
            NodeKind::Branch { cases } => {
                for case in cases {
                    errors.extend(bad_expr(
                        &node.id,
                        &format!("case `{}` `when`", case.label),
                        &case.when,
                    ));
                }
            }
            NodeKind::Transform { output } => match output {
                TransformOutput::Expr(e) => {
                    errors.extend(bad_expr(&node.id, "transform expression", e))
                }
                TransformOutput::Object(map) => {
                    for (field, e) in map {
                        errors.extend(bad_expr(&node.id, &format!("transform field `{field}`"), e));
                    }
                }
            },
            NodeKind::Parallel { collection, .. } => {
                errors.extend(bad_expr(&node.id, "parallel collection", collection));
            }
            NodeKind::Delay { r#for } => match parse_duration(r#for) {
                Err(m) => errors.push(
                    ValidationError::new("bad_duration", format!("node `{}`: {m}", node.id.0))
                        .at_node(&node.id),
                ),
                Ok(d) if d > DELAY_CAP => errors.push(
                    ValidationError::new(
                        "bad_duration",
                        format!(
                            "node `{}`: delay `{}` exceeds the 15-minute cap",
                            node.id.0, r#for
                        ),
                    )
                    .at_node(&node.id),
                ),
                Ok(_) => {}
            },
            NodeKind::Action { config } => {
                for tmpl in action_templates(config) {
                    if let Err(e) = expr::template_check(&tmpl) {
                        errors.push(
                            ValidationError::new(
                                "bad_expression",
                                format!("node `{}` action template: {e}", node.id.0),
                            )
                            .at_node(&node.id),
                        );
                    }
                }
            }
            NodeKind::Trigger { .. } | NodeKind::Subflow { .. } => {}
        }
    }
    errors
}

/// Every `{{ … }}`-templated string field of an [`Action`] — the surfaces
/// [`render_action`] interpolates at run time.
fn action_templates(action: &Action) -> Vec<String> {
    match action {
        Action::LogLine { message } => vec![message.clone()],
        Action::Ts6Command { args, .. } | Action::MusicBotCommand { args, .. } => args
            .values()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Action::WebhookOut { url, headers } => {
            let mut t = vec![url.clone()];
            t.extend(headers.iter().flat_map(|(k, v)| [k.clone(), v.clone()]));
            t
        }
        Action::Moderate {
            reason_template, ..
        } => vec![reason_template.clone()],
    }
}

/// Depth-first search for one cycle, returned as the node-id path with the
/// entry node repeated at the end (`a → b → a`). Empty when acyclic.
fn find_cycle_path(graph: &FlowGraph) -> Vec<String> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for n in &graph.nodes {
        adj.entry(n.id.0.clone()).or_default();
    }
    for e in &graph.edges {
        adj.entry(e.from.node.0.clone())
            .or_default()
            .push(e.to.node.0.clone());
    }
    // 1 = on the current DFS stack, 2 = fully explored.
    let mut color: HashMap<String, u8> = HashMap::new();
    let mut path: Vec<String> = Vec::new();
    for n in &graph.nodes {
        if dfs_cycle(&n.id.0, &adj, &mut color, &mut path) {
            return path;
        }
    }
    Vec::new()
}

fn dfs_cycle(
    node: &str,
    adj: &HashMap<String, Vec<String>>,
    color: &mut HashMap<String, u8>,
    path: &mut Vec<String>,
) -> bool {
    if color.get(node).copied().unwrap_or(0) != 0 {
        return false;
    }
    color.insert(node.to_string(), 1);
    path.push(node.to_string());
    for next in adj.get(node).map(Vec::as_slice).unwrap_or(&[]) {
        match color.get(next).copied().unwrap_or(0) {
            1 => {
                // Back-edge — trim `path` to start at the cycle entry.
                if let Some(pos) = path.iter().position(|p| p == next) {
                    path.drain(..pos);
                }
                path.push(next.clone());
                return true;
            }
            2 => {}
            _ => {
                if dfs_cycle(next, adj, color, path) {
                    return true;
                }
            }
        }
    }
    path.pop();
    color.insert(node.to_string(), 2);
    false
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Internal run result — adds the final blackboard `nodes` map so a
/// `subflow`/`parallel` parent can read its child's terminal output.
struct InnerOutcome {
    status: FlowRunStatus,
    error: Option<String>,
    node_results: Vec<NodeResult>,
    blackboard_nodes: Map<String, Value>,
}

/// Edge control signal (`architecture.md` §5.2). A `skipped` signal still
/// *satisfies* an edge — that is the rule that keeps branch+merge from
/// deadlocking a join on the pruned side (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Active,
    Skipped,
}

/// A node fault — the typed shape behind an `errored` settle. Lowered into
/// the §6.2 error document by [`error_document`].
struct NodeFault {
    code: String,
    message: String,
    /// Extra object fields merged into the error document (`parallel`
    /// records `failedIndices` here).
    detail: Option<Value>,
}

impl NodeFault {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            detail: None,
        }
    }

    fn expr(e: expr::ExprError) -> Self {
        Self::new("expr_error", e.0)
    }
}

/// The settled outcome of one node task.
struct NodeExec {
    status: NodeStatus,
    /// `out` document for an `ok` settle (a `branch` carries its
    /// pass-through input here).
    out_data: Value,
    /// Matched output port — `Some` only for a `branch`.
    branch_port: Option<String>,
    /// Fault — `Some` only for an `errored` settle.
    fault: Option<NodeFault>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

impl NodeExec {
    fn ok(out_data: Value, started: DateTime<Utc>) -> Self {
        Self {
            status: NodeStatus::Ok,
            out_data,
            branch_port: None,
            fault: None,
            started_at: started,
            finished_at: Utc::now(),
        }
    }

    fn errored(fault: NodeFault, started: DateTime<Utc>) -> Self {
        Self {
            status: NodeStatus::Errored,
            out_data: Value::Null,
            branch_port: None,
            fault: Some(fault),
            started_at: started,
            finished_at: Utc::now(),
        }
    }

    fn skipped() -> Self {
        let now = Utc::now();
        Self {
            status: NodeStatus::Skipped,
            out_data: Value::Null,
            branch_port: None,
            fault: None,
            started_at: now,
            finished_at: now,
        }
    }
}

/// Run a graph to completion. Boxed so `subflow`/`parallel` node handlers
/// can recurse into it without an infinitely-sized future.
fn execute(
    graph: FlowGraph,
    trigger_doc: Value,
    deps: GraphDeps,
    depth: usize,
) -> Pin<Box<dyn Future<Output = InnerOutcome> + Send>> {
    Box::pin(async move {
        if let Err(reason) = validate(&graph) {
            return InnerOutcome {
                status: FlowRunStatus::Errored,
                error: Some(format!("graph validation failed: {reason}")),
                node_results: Vec::new(),
                blackboard_nodes: Map::new(),
            };
        }

        let by_id: HashMap<&NodeId, &Node> = graph.nodes.iter().map(|n| (&n.id, n)).collect();
        let inbound: HashMap<&NodeId, Vec<&Edge>> = {
            let mut m: HashMap<&NodeId, Vec<&Edge>> = HashMap::new();
            for node in &graph.nodes {
                m.insert(&node.id, Vec::new());
            }
            for edge in &graph.edges {
                m.entry(&edge.to.node).or_default().push(edge);
            }
            m
        };

        // Run state.
        let mut edge_sig: HashMap<EdgeId, Signal> = HashMap::new();
        let mut port_out: HashMap<NodeId, HashMap<String, Value>> = HashMap::new();
        let mut bb_nodes: Map<String, Value> = Map::new();
        let mut results: Vec<NodeResult> = Vec::new();
        let mut dispatched: HashSet<NodeId> = HashSet::new();
        let mut unhandled: Option<String> = None;
        let mut running = 0usize;

        let node_sem = Arc::new(Semaphore::new(NODE_SEMAPHORE));
        let (tx, mut rx) = mpsc::channel::<(NodeId, NodeExec)>(64);

        loop {
            // Scan for ready, not-yet-dispatched nodes.
            let ready: Vec<NodeId> = graph
                .nodes
                .iter()
                .filter(|n| !dispatched.contains(&n.id))
                .filter(|n| node_ready(n, &inbound, &edge_sig))
                .map(|n| n.id.clone())
                .collect();

            if !ready.is_empty() {
                for nid in ready {
                    dispatched.insert(nid.clone());
                    let node = by_id[&nid];
                    let in_edges = inbound.get(&nid).cloned().unwrap_or_default();
                    let active: Vec<&Edge> = in_edges
                        .iter()
                        .filter(|e| edge_sig.get(&e.id) == Some(&Signal::Active))
                        .copied()
                        .collect();

                    if matches!(node.kind, NodeKind::Trigger { .. }) {
                        // The unique entry node — settles immediately with
                        // the trigger document on its `out` port.
                        let exec = NodeExec::ok(trigger_doc.clone(), Utc::now());
                        settle(
                            &nid,
                            node,
                            exec,
                            &graph,
                            &mut edge_sig,
                            &mut port_out,
                            &mut bb_nodes,
                            &mut results,
                            &mut unhandled,
                        );
                    } else if active.is_empty() {
                        // Every inbound edge is `skipped` — the whole
                        // upstream was pruned; settle `skipped` without
                        // running (`architecture.md` §5.3).
                        settle(
                            &nid,
                            node,
                            NodeExec::skipped(),
                            &graph,
                            &mut edge_sig,
                            &mut port_out,
                            &mut bb_nodes,
                            &mut results,
                            &mut unhandled,
                        );
                    } else {
                        // Dispatch as a task. `input` is the single
                        // active-inbound-edge document; a join with
                        // multiple inbound edges leaves it undefined (§7.1).
                        let input = if active.len() == 1 {
                            port_out
                                .get(&active[0].from.node)
                                .and_then(|m| m.get(&active[0].from.port))
                                .cloned()
                        } else {
                            None
                        };
                        let bb = Blackboard::new(trigger_doc.clone(), bb_nodes.clone(), input);
                        let permit = node_sem
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("per-run node semaphore is never closed");
                        let node_owned = node.clone();
                        let deps2 = deps.clone();
                        let tx2 = tx.clone();
                        let nid2 = nid.clone();
                        running += 1;
                        tokio::spawn(async move {
                            let _permit = permit;
                            // Inner spawn isolates a node-handler panic at
                            // the task boundary (`architecture.md` §6.6).
                            let exec = match tokio::spawn(run_node(node_owned, bb, deps2, depth))
                                .await
                            {
                                Ok(e) => e,
                                Err(je) => NodeExec::errored(
                                    NodeFault::new("panic", format!("node task panicked: {je}")),
                                    Utc::now(),
                                ),
                            };
                            let _ = tx2.send((nid2, exec)).await;
                        });
                    }
                }
                continue;
            }

            if running == 0 {
                break;
            }

            let (nid, exec) = rx
                .recv()
                .await
                .expect("at least one task sender is alive while running > 0");
            running -= 1;
            let node = by_id[&nid];
            settle(
                &nid,
                node,
                exec,
                &graph,
                &mut edge_sig,
                &mut port_out,
                &mut bb_nodes,
                &mut results,
                &mut unhandled,
            );
        }

        results.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.node_id.0.cmp(&b.node_id.0))
        });

        let (status, error) = match unhandled {
            Some(msg) => (FlowRunStatus::Errored, Some(msg)),
            None => (FlowRunStatus::Ok, None),
        };
        InnerOutcome {
            status,
            error,
            node_results: results,
            blackboard_nodes: bb_nodes,
        }
    })
}

/// A node is ready when its inbound edges have settled per its join policy
/// (`architecture.md` §5.3). The trigger node (no inbound) is ready at once.
fn node_ready(
    node: &Node,
    inbound: &HashMap<&NodeId, Vec<&Edge>>,
    edge_sig: &HashMap<EdgeId, Signal>,
) -> bool {
    if matches!(node.kind, NodeKind::Trigger { .. }) {
        return true;
    }
    let edges = match inbound.get(&node.id) {
        Some(e) if !e.is_empty() => e,
        // Defensive — validation rejects a non-trigger node with no
        // inbound edge; treat it as ready so the run still terminates.
        _ => return true,
    };
    let policy = edges[0].join_policy;
    match policy {
        JoinPolicy::All => edges.iter().all(|e| edge_sig.contains_key(&e.id)),
        JoinPolicy::Any => {
            edges
                .iter()
                .any(|e| edge_sig.get(&e.id) == Some(&Signal::Active))
                || edges.iter().all(|e| edge_sig.contains_key(&e.id))
        }
    }
}

/// Record a settled node: push its [`NodeResult`], update the blackboard,
/// resolve each output port's control signal, and stamp the leaving edges.
#[allow(clippy::too_many_arguments)]
fn settle(
    nid: &NodeId,
    node: &Node,
    exec: NodeExec,
    graph: &FlowGraph,
    edge_sig: &mut HashMap<EdgeId, Signal>,
    port_out: &mut HashMap<NodeId, HashMap<String, Value>>,
    bb_nodes: &mut Map<String, Value>,
    results: &mut Vec<NodeResult>,
    unhandled: &mut Option<String>,
) {
    let kind = kind_str(&node.kind);
    let duration_ms = (exec.finished_at - exec.started_at)
        .num_milliseconds()
        .max(0) as u64;

    // The §6.2 error document, when this node errored.
    let error_doc = exec.fault.as_ref().map(|f| error_document(nid, kind, f));

    // Blackboard `nodes.<id>` entry: the node's primary output (§7.1).
    let primary = match exec.status {
        NodeStatus::Ok => exec.out_data.clone(),
        NodeStatus::Errored => error_doc.clone().unwrap_or(Value::Null),
        NodeStatus::Skipped | NodeStatus::Interrupted => Value::Null,
    };
    bb_nodes.insert(nid.0.clone(), primary);

    // Per-node run record (§6.4).
    let output = match exec.status {
        NodeStatus::Ok => Some(cap_output(exec.out_data.clone())),
        _ => None,
    };
    results.push(NodeResult {
        node_id: nid.clone(),
        kind: kind.to_string(),
        status: exec.status,
        started_at: exec.started_at,
        finished_at: Some(exec.finished_at),
        duration_ms: Some(duration_ms),
        error: exec.fault.as_ref().map(|f| f.message.clone()),
        output,
    });

    // Resolve each output port → a control signal + (for active) its data.
    let mut active_ports: HashMap<String, Value> = HashMap::new();
    for port in output_ports(&node.kind) {
        let (sig, data) = port_signal(node, &exec, &port, error_doc.as_ref());
        if sig == Signal::Active {
            active_ports.insert(port.clone(), data);
        }
        for edge in graph
            .edges
            .iter()
            .filter(|e| &e.from.node == nid && e.from.port == port)
        {
            edge_sig.insert(edge.id.clone(), sig);
        }
    }
    port_out.insert(nid.clone(), active_ports);

    // An `errored` node with an *unwired* `err` port is an unhandled
    // failure — the run terminates `errored` (`architecture.md` §6.3).
    if exec.status == NodeStatus::Errored {
        let err_wired = has_err_port(&node.kind)
            && graph
                .edges
                .iter()
                .any(|e| &e.from.node == nid && e.from.port == "err");
        if !err_wired {
            let msg = exec
                .fault
                .as_ref()
                .map(|f| format!("node `{}`: {}", nid.0, f.message))
                .unwrap_or_else(|| format!("node `{}` errored", nid.0));
            unhandled.get_or_insert(msg);
        }
    }
}

/// Resolve one output port to its control signal (`architecture.md` §5.2).
fn port_signal(
    node: &Node,
    exec: &NodeExec,
    port: &str,
    error_doc: Option<&Value>,
) -> (Signal, Value) {
    match exec.status {
        NodeStatus::Skipped | NodeStatus::Interrupted => (Signal::Skipped, Value::Null),
        NodeStatus::Errored => {
            if port == "err" {
                (Signal::Active, error_doc.cloned().unwrap_or(Value::Null))
            } else {
                (Signal::Skipped, Value::Null)
            }
        }
        NodeStatus::Ok => {
            if matches!(node.kind, NodeKind::Branch { .. }) {
                // The matched case port fires; every other port is pruned.
                if exec.branch_port.as_deref() == Some(port) {
                    (Signal::Active, exec.out_data.clone())
                } else {
                    (Signal::Skipped, Value::Null)
                }
            } else if port == "out" {
                (Signal::Active, exec.out_data.clone())
            } else {
                // The `err` port of a node that settled `ok`.
                (Signal::Skipped, Value::Null)
            }
        }
    }
}

/// The §6.2 error document an `err` port carries.
fn error_document(nid: &NodeId, kind: &str, fault: &NodeFault) -> Value {
    let mut doc = json!({
        "node": nid.0,
        "kind": kind,
        "code": fault.code,
        "message": fault.message,
        "at": Utc::now().to_rfc3339(),
    });
    if let (Some(Value::Object(extra)), Value::Object(target)) = (&fault.detail, &mut doc) {
        for (k, v) in extra {
            target.insert(k.clone(), v.clone());
        }
    }
    doc
}

/// Enforce the 8 kB `output` cap (`architecture.md` §6.4).
fn cap_output(v: Value) -> Value {
    let len = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
    if len > OUTPUT_CAP {
        json!({ "_truncated": true })
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// Node handlers
// ---------------------------------------------------------------------------

/// Run one non-trigger node. Spawned as a task by [`execute`].
async fn run_node(node: Node, bb: Blackboard, deps: GraphDeps, depth: usize) -> NodeExec {
    let started = Utc::now();
    match &node.kind {
        NodeKind::Trigger { .. } => NodeExec::ok(bb.trigger().clone(), started),
        NodeKind::Action { config } => match run_action(config, &bb, &deps).await {
            Ok(v) => NodeExec::ok(v, started),
            Err(f) => NodeExec::errored(f, started),
        },
        NodeKind::Branch { cases } => {
            let (port, data) = run_branch(cases, &bb);
            NodeExec {
                status: NodeStatus::Ok,
                out_data: data,
                branch_port: Some(port),
                fault: None,
                started_at: started,
                finished_at: Utc::now(),
            }
        }
        NodeKind::Delay { r#for } => match run_delay(r#for, &bb).await {
            Ok(v) => NodeExec::ok(v, started),
            Err(f) => NodeExec::errored(f, started),
        },
        NodeKind::Transform { output } => match run_transform(output, &bb) {
            Ok(v) => NodeExec::ok(v, started),
            Err(f) => NodeExec::errored(f, started),
        },
        NodeKind::Parallel {
            collection,
            sub_flow_id,
            max_concurrency,
        } => match run_parallel(
            collection,
            *sub_flow_id,
            *max_concurrency,
            &bb,
            &deps,
            depth,
        )
        .await
        {
            Ok(v) => NodeExec::ok(v, started),
            Err(f) => NodeExec::errored(f, started),
        },
        NodeKind::Subflow { sub_flow_id } => {
            match run_subflow(*sub_flow_id, &bb, &deps, depth).await {
                Ok(v) => NodeExec::ok(v, started),
                Err(f) => NodeExec::errored(f, started),
            }
        }
    }
}

/// `action` — render `{{ }}` templates, then lower onto the dispatcher.
async fn run_action(
    action: &Action,
    bb: &Blackboard,
    deps: &GraphDeps,
) -> Result<Value, NodeFault> {
    let rendered = render_action(action, bb).map_err(NodeFault::expr)?;
    let ctx = ActionContext {
        flow_id: deps.flow_id,
        run_id: deps.run_id,
        flow_name: deps.flow_name.clone(),
        server_config_id: deps.server_config_id,
        virtual_server_id: deps.virtual_server_id,
        action_index: 0,
        trigger: bb.trigger().clone(),
    };
    match deps.dispatcher.dispatch(&ctx, &rendered).await {
        ActionOutcome::Ok => Ok(json!({ "ok": true })),
        ActionOutcome::Errored(message) => Err(NodeFault::new("action_error", message)),
    }
}

/// Interpolate every string-valued field of an [`Action`] through the v2
/// expression dialect (`architecture.md` §4.2 / §7.2).
fn render_action(action: &Action, bb: &Blackboard) -> Result<Action, expr::ExprError> {
    Ok(match action {
        Action::LogLine { message } => Action::LogLine {
            message: expr::interpolate(message, bb)?,
        },
        Action::Ts6Command { command, args } => Action::Ts6Command {
            command: command.clone(),
            args: render_args(args, bb)?,
        },
        Action::MusicBotCommand {
            bot_id,
            command,
            args,
        } => Action::MusicBotCommand {
            bot_id: *bot_id,
            command: command.clone(),
            args: render_args(args, bb)?,
        },
        Action::WebhookOut { url, headers } => Action::WebhookOut {
            url: expr::interpolate(url, bb)?,
            headers: headers
                .iter()
                .map(|(k, v)| Ok((k.clone(), expr::interpolate(v, bb)?)))
                .collect::<Result<Vec<_>, expr::ExprError>>()?,
        },
        Action::Moderate {
            effect,
            duration_secs,
            reason_template,
            rule_key,
        } => Action::Moderate {
            effect: *effect,
            duration_secs: *duration_secs,
            reason_template: expr::interpolate(reason_template, bb)?,
            rule_key: rule_key.clone(),
        },
    })
}

fn render_args(
    args: &Map<String, Value>,
    bb: &Blackboard,
) -> Result<Map<String, Value>, expr::ExprError> {
    let mut out = Map::with_capacity(args.len());
    for (key, value) in args {
        let rendered = match value {
            Value::String(s) => Value::String(expr::interpolate(s, bb)?),
            other => other.clone(),
        };
        out.insert(key.clone(), rendered);
    }
    Ok(out)
}

/// `branch` — the first case whose `when` is true fires; else `default`.
/// A case expression that fails to evaluate is logged and treated as false
/// (`architecture.md` §4.3).
fn run_branch(cases: &[BranchCase], bb: &Blackboard) -> (String, Value) {
    let data = bb.input().clone();
    for case in cases {
        match expr::eval_bool(&case.when, bb) {
            Ok(true) => return (case.label.clone(), data),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                case = %case.label,
                error = %e,
                "branch case expression failed to evaluate; treating as false"
            ),
        }
    }
    ("default".to_string(), data)
}

/// `delay` — park the path for a bounded duration, then pass data through.
async fn run_delay(spec: &str, bb: &Blackboard) -> Result<Value, NodeFault> {
    let dur = parse_duration(spec).map_err(|m| NodeFault::new("bad_duration", m))?;
    if dur > DELAY_CAP {
        return Err(NodeFault::new(
            "delay_too_long",
            format!("delay `{spec}` exceeds the 15-minute cap"),
        ));
    }
    tokio::time::sleep(dur).await;
    Ok(bb.input().clone())
}

/// Parse a `delay` duration string: a non-negative integer with an `ms`,
/// `s`, or `m` suffix (`30s`, `500ms`, `5m`).
fn parse_duration(spec: &str) -> Result<Duration, String> {
    let s = spec.trim();
    let split = s
        .find(|c: char| c.is_alphabetic())
        .ok_or_else(|| format!("duration `{spec}` has no unit suffix"))?;
    let (num, unit) = s.split_at(split);
    let value: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("duration `{spec}` has a non-integer value"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(value)),
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        other => Err(format!("duration `{spec}` has unknown unit `{other}`")),
    }
}

/// `transform` — pure data reshaping via the expression dialect.
fn run_transform(output: &TransformOutput, bb: &Blackboard) -> Result<Value, NodeFault> {
    match output {
        TransformOutput::Expr(e) => expr::eval(e, bb).map_err(NodeFault::expr),
        TransformOutput::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, e) in map {
                out.insert(key.clone(), expr::eval(e, bb).map_err(NodeFault::expr)?);
            }
            Ok(Value::Object(out))
        }
    }
}

/// `parallel` — dynamic fan-out: run a sub-flow once per collection element
/// with bounded concurrency (`architecture.md` §4.4).
async fn run_parallel(
    collection_expr: &str,
    sub_flow_id: FlowId,
    max_concurrency: u8,
    bb: &Blackboard,
    deps: &GraphDeps,
    depth: usize,
) -> Result<Value, NodeFault> {
    if depth >= MAX_DEPTH {
        return Err(NodeFault::new(
            "subflow_depth",
            format!("sub-flow nesting exceeds depth {MAX_DEPTH}"),
        ));
    }
    let collection = expr::eval(collection_expr, bb).map_err(NodeFault::expr)?;
    let Value::Array(elements) = collection else {
        return Err(NodeFault::new(
            "collection_not_array",
            "`parallel` collection expression did not evaluate to an array",
        ));
    };
    if elements.len() > COLLECTION_CAP {
        return Err(NodeFault::new(
            "collection_too_large",
            format!(
                "`parallel` collection has {} elements (cap {COLLECTION_CAP})",
                elements.len()
            ),
        ));
    }
    let (graph, name, server_config_id, virtual_server_id) =
        load_subflow(&deps.db, sub_flow_id).await?;

    let element_sem = Arc::new(Semaphore::new((max_concurrency.clamp(1, 16)) as usize));
    let mut handles = Vec::with_capacity(elements.len());
    for element in elements {
        let permit = element_sem
            .clone()
            .acquire_owned()
            .await
            .expect("parallel element semaphore is never closed");
        let element_graph = graph.clone();
        let subdeps = GraphDeps {
            db: deps.db.clone(),
            dispatcher: deps.dispatcher.clone(),
            flow_id: sub_flow_id,
            run_id: deps.run_id,
            flow_name: name.clone(),
            server_config_id,
            virtual_server_id,
        };
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            execute(element_graph, element, subdeps, depth + 1).await
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    let mut failed = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(outcome) => {
                if outcome.status == FlowRunStatus::Errored {
                    failed.push(index);
                }
                results.push(Value::Object(outcome.blackboard_nodes));
            }
            Err(join_err) => {
                failed.push(index);
                results.push(json!({ "_panicked": join_err.to_string() }));
            }
        }
    }

    if !failed.is_empty() {
        return Err(NodeFault {
            code: "parallel_error".to_string(),
            message: format!(
                "{} of {} `parallel` element runs errored",
                failed.len(),
                results.len()
            ),
            detail: Some(json!({ "failedIndices": failed, "results": results })),
        });
    }
    Ok(Value::Array(results))
}

/// `subflow` — run another flow as a nested run, emitting its terminal
/// blackboard (`architecture.md` §4.7).
async fn run_subflow(
    sub_flow_id: FlowId,
    bb: &Blackboard,
    deps: &GraphDeps,
    depth: usize,
) -> Result<Value, NodeFault> {
    if depth >= MAX_DEPTH {
        return Err(NodeFault::new(
            "subflow_depth",
            format!("sub-flow nesting exceeds depth {MAX_DEPTH}"),
        ));
    }
    let (graph, name, server_config_id, virtual_server_id) =
        load_subflow(&deps.db, sub_flow_id).await?;
    let subdeps = GraphDeps {
        db: deps.db.clone(),
        dispatcher: deps.dispatcher.clone(),
        flow_id: sub_flow_id,
        run_id: deps.run_id,
        flow_name: name,
        server_config_id,
        virtual_server_id,
    };
    let outcome = execute(graph, bb.input().clone(), subdeps, depth + 1).await;
    if outcome.status == FlowRunStatus::Errored {
        return Err(NodeFault {
            code: "subflow_error".to_string(),
            message: outcome
                .error
                .unwrap_or_else(|| "sub-flow run errored".to_string()),
            detail: Some(json!({ "subFlowId": sub_flow_id.0 })),
        });
    }
    Ok(Value::Object(outcome.blackboard_nodes))
}

/// Load and decode a sub-flow row. A legacy `flowData` blob comes back as a
/// projected path graph via [`decode_flow_data`].
async fn load_subflow(
    db: &Database,
    sub_flow_id: FlowId,
) -> Result<(FlowGraph, String, i64, i64), NodeFault> {
    let row = bot_flows::find_by_id(db, sub_flow_id.0)
        .await
        .map_err(|e| {
            NodeFault::new(
                "subflow_error",
                format!("loading sub-flow {}: {e}", sub_flow_id.0),
            )
        })?
        .ok_or_else(|| {
            NodeFault::new(
                "subflow_missing",
                format!("sub-flow {} not found", sub_flow_id.0),
            )
        })?;
    let graph = decode_flow_data(&row.flowData).map_err(|e| {
        NodeFault::new(
            "subflow_error",
            format!("sub-flow {} flowData: {e}", sub_flow_id.0),
        )
    })?;
    Ok((graph, row.name, row.serverConfigId, row.virtualServerId))
}

// ---------------------------------------------------------------------------
// Port catalogue (architecture.md §4)
// ---------------------------------------------------------------------------

fn output_ports(kind: &NodeKind) -> Vec<String> {
    match kind {
        NodeKind::Trigger { .. } | NodeKind::Delay { .. } => vec!["out".to_string()],
        NodeKind::Action { .. }
        | NodeKind::Parallel { .. }
        | NodeKind::Transform { .. }
        | NodeKind::Subflow { .. } => vec!["out".to_string(), "err".to_string()],
        NodeKind::Branch { cases } => {
            let mut ports: Vec<String> = cases.iter().map(|c| c.label.clone()).collect();
            ports.push("default".to_string());
            ports
        }
    }
}

fn has_err_port(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Action { .. }
            | NodeKind::Parallel { .. }
            | NodeKind::Transform { .. }
            | NodeKind::Subflow { .. }
    )
}

fn is_input_port(kind: &NodeKind, port: &str) -> bool {
    !matches!(kind, NodeKind::Trigger { .. }) && port == "in"
}

fn kind_str(kind: &NodeKind) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ts6_manager_shared::flows::Trigger;
    use ts6_manager_shared::flows::v2::{EdgeId, PortRef, Position};

    fn trigger_node(id: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: None,
            position: Position { x: 0.0, y: 0.0 },
            kind: NodeKind::Trigger {
                config: Trigger::ManualFire,
            },
        }
    }

    fn log_node(id: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: None,
            position: Position { x: 0.0, y: 0.0 },
            kind: NodeKind::Action {
                config: Action::LogLine {
                    message: "x".into(),
                },
            },
        }
    }

    fn edge(id: &str, from: (&str, &str), to: (&str, &str)) -> Edge {
        Edge {
            id: EdgeId(id.into()),
            from: PortRef {
                node: NodeId(from.0.into()),
                port: from.1.into(),
            },
            to: PortRef {
                node: NodeId(to.0.into()),
                port: to.1.into(),
            },
            join_policy: JoinPolicy::All,
        }
    }

    #[test]
    fn validate_accepts_a_path_graph() {
        let graph = FlowGraph {
            nodes: vec![trigger_node("t"), log_node("a")],
            edges: vec![edge("e0", ("t", "out"), ("a", "in"))],
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn validate_rejects_a_cycle() {
        // a -> b -> a, plus a trigger feeding a.
        let graph = FlowGraph {
            nodes: vec![trigger_node("t"), log_node("a"), log_node("b")],
            edges: vec![
                edge("e0", ("t", "out"), ("a", "in")),
                edge("e1", ("a", "out"), ("b", "in")),
                edge("e2", ("b", "out"), ("a", "in")),
            ],
        };
        assert!(validate(&graph).unwrap_err().contains("cycle"));
    }

    #[test]
    fn validate_rejects_two_triggers() {
        let graph = FlowGraph {
            nodes: vec![trigger_node("t"), trigger_node("t2"), log_node("a")],
            edges: vec![edge("e0", ("t", "out"), ("a", "in"))],
        };
        assert!(
            validate(&graph)
                .unwrap_err()
                .contains("exactly one trigger")
        );
    }

    #[test]
    fn validate_rejects_an_unknown_port() {
        let graph = FlowGraph {
            nodes: vec![trigger_node("t"), log_node("a")],
            edges: vec![edge("e0", ("t", "nope"), ("a", "in"))],
        };
        assert!(validate(&graph).unwrap_err().contains("output port"));
    }

    #[test]
    fn validate_rejects_an_unreachable_node() {
        let graph = FlowGraph {
            nodes: vec![trigger_node("t"), log_node("a"), log_node("orphan")],
            edges: vec![
                edge("e0", ("t", "out"), ("a", "in")),
                // `orphan` only has a self-feeding inbound from `a`'s err,
                // but nothing reaches it from the trigger after pruning.
                edge("e1", ("a", "err"), ("orphan", "in")),
            ],
        };
        // `orphan` *is* reachable here (a.err -> orphan), so flip to a
        // genuinely stranded node by removing that edge's source path.
        let stranded = FlowGraph {
            nodes: vec![trigger_node("t"), log_node("a"), log_node("orphan")],
            edges: vec![
                edge("e0", ("t", "out"), ("a", "in")),
                edge("e1", ("orphan", "out"), ("orphan", "in")),
            ],
        };
        assert!(validate(&graph).is_ok());
        assert!(validate(&stranded).is_err());
    }

    #[test]
    fn parse_duration_covers_supported_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10h").is_err());
    }
}
