//! v2 graph wire types + persistence helpers — flow-engine redesign
//! ([PURA-259](/PURA/issues/PURA-259) / [PURA-260](/PURA/issues/PURA-260)).
//!
//! Mirrors `docs/flows/v2/architecture.md` §3–§9 and `docs/flows/v2/http-api.md`
//! §2 / §5. The v1.1 [`super`] module (`flows.rs`) is **kept untouched**: a
//! legacy linear flow is loaded through [`project_legacy`] into a degenerate
//! path graph, so there is exactly one graph model downstream.
//!
//! Stays WASM-clean (pure Rust + `serde_json`) so the Dioxus canvas and the
//! axum routes share these shapes verbatim. Every struct is
//! `#[serde(rename_all = "camelCase")]`, matching v1.1 and `music_bots`.
//!
//! ## Persistence — zero schema migration (`http-api.md` §5)
//!
//! - `bot_flow.flowData` keeps its opaque-`string` column. v2 writes the
//!   versioned envelope `{ "version": 2, "graph": { … } }`; a row with no
//!   `version` key is, by definition, a legacy v1.1 [`FlowDefinition`].
//!   [`decode_flow_data`] is the deserializer — it tries the v2 envelope,
//!   falls back to v1.1 + [`project_legacy`].
//! - `bot_flow_run` gains `nodeResults` ([`NodeResult`]) without a new
//!   column: the repo packs it into the existing opaque-`string`
//!   `actionResults` column as a JSON envelope (see `repos::bot_flow_runs`).

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Action, ActionResult, FlowDefinition, FlowId, FlowRunId, FlowRunStatus, Trigger};

/// The persistence envelope version this module writes and the highest it
/// reads. A `flowData` blob with no `version` key is a legacy v1.1 flow.
pub const FLOW_DATA_VERSION: u8 = 2;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// A node identifier — a stable, human-readable slug (`"fetch_user"`),
/// **not** an integer. It is referenced by expressions (`architecture.md` §7)
/// and by the per-node run record, so it must stay legible across edits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

/// An edge identifier — unique within a [`FlowGraph`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeId(pub String);

// ---------------------------------------------------------------------------
// Graph model (architecture.md §3, §4)
// ---------------------------------------------------------------------------

/// The persisted graph. Stored inside the versioned `flowData` envelope
/// ([`FlowDataEnvelope`]). Structural invariants (`architecture.md` §3.1) are
/// validated by the graph-engine child, not by `serde`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowGraph {
    /// 1..=64 nodes; exactly one is a [`NodeKind::Trigger`].
    pub nodes: Vec<Node>,
    /// 0..=128 directed edges between node ports.
    pub edges: Vec<Edge>,
}

/// One graph node. `kind` is flattened so the wire is a flat object carrying
/// the `kind` discriminant alongside `id` / `label` / `position`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: NodeId,
    /// Operator-facing display name; defaults to `id` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Canvas coordinates — ignored by the engine.
    pub position: Position,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Canvas coordinates. The engine never reads these; the canvas owns layout.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

/// The seven v2 node kinds (`architecture.md` §4). Internally tagged on
/// `kind` so it flattens into [`Node`] as a flat object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NodeKind {
    /// Graph entry — the unique source node. `config` reuses the v1.1
    /// [`Trigger`] catalogue unchanged (no widening in v2).
    Trigger { config: Trigger },
    /// Performs one effect. `config` reuses the v1.1 [`Action`] catalogue.
    Action { config: Action },
    /// Routes control to exactly one output port — one per case label plus
    /// an implicit `default`.
    Branch { cases: Vec<BranchCase> },
    /// Dynamic fan-out: runs `subFlowId` once per element of `collection`
    /// with bounded concurrency.
    Parallel {
        collection: String,
        sub_flow_id: FlowId,
        #[serde(default = "mc_default")]
        max_concurrency: u8,
    },
    /// Parks the path for a bounded duration (≤ 15 min), then passes data
    /// through unchanged.
    Delay {
        #[serde(rename = "for")]
        r#for: String,
    },
    /// Pure, side-effect-free data reshaping via an expression.
    Transform { output: TransformOutput },
    /// Runs another flow as a nested run.
    Subflow { sub_flow_id: FlowId },
}

/// One ordered case of a [`NodeKind::Branch`]. `when` is a boolean
/// expression in the v2 dialect (`architecture.md` §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchCase {
    pub label: String,
    pub when: String,
}

/// The `output` of a [`NodeKind::Transform`] — either object construction
/// (field → expression) or a single expression producing any JSON value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransformOutput {
    /// Object construction: each value is an expression.
    Object(BTreeMap<String, String>),
    /// A single expression.
    Expr(String),
}

/// A directed edge from one node's output port to another's input port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub id: EdgeId,
    pub from: PortRef,
    pub to: PortRef,
    /// The join policy applied at the *target* node; carried per-edge for
    /// wire simplicity (`http-api.md` §2).
    #[serde(default)]
    pub join_policy: JoinPolicy,
}

/// An endpoint of an [`Edge`] — a `port` on a `node`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortRef {
    pub node: NodeId,
    pub port: String,
}

/// How a join node (a node with multiple inbound edges) becomes ready
/// (`architecture.md` §5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    /// Ready when *all* inbound edges have settled. The default.
    #[default]
    All,
    /// Ready as soon as the first inbound edge settles `active`.
    Any,
}

/// `parallel.maxConcurrency` default (`architecture.md` §4.4).
fn mc_default() -> u8 {
    4
}

// ---------------------------------------------------------------------------
// Run record additions (http-api.md §2.1)
// ---------------------------------------------------------------------------

/// One per-node run record. v2 runs populate a `Vec<NodeResult>`; v1.1 runs
/// leave it empty and keep their `actionResults` (`http-api.md` §5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeResult {
    pub node_id: NodeId,
    /// Wire discriminant of the node kind (`"action"`, `"branch"`, …).
    pub kind: String,
    pub status: NodeStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    /// The node's `out` document — capped at 8 kB by the engine; over-cap
    /// stores `{ "_truncated": true }` (`architecture.md` §6.4).
    pub output: Option<serde_json::Value>,
}

/// Per-node lifecycle marker. `skipped` is a *node* status only — never a
/// *run* status (`architecture.md` §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Ok,
    Errored,
    Skipped,
    Interrupted,
}

// ---------------------------------------------------------------------------
// Persistence envelope (http-api.md §5.1)
// ---------------------------------------------------------------------------

/// The versioned `flowData` envelope a v2 flow row stores:
/// `{ "version": 2, "graph": { nodes, edges } }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowDataEnvelope {
    pub version: u8,
    pub graph: FlowGraph,
}

impl FlowDataEnvelope {
    /// Wrap a graph in the current-version envelope.
    pub fn new(graph: FlowGraph) -> Self {
        Self {
            version: FLOW_DATA_VERSION,
            graph,
        }
    }
}

/// Failure decoding a `flowData` blob.
#[derive(Debug)]
pub enum FlowDataError {
    /// The blob was not valid JSON, or did not match either the v2 envelope
    /// or the v1.1 [`FlowDefinition`] shape.
    Json(serde_json::Error),
    /// Top-level JSON was not an object.
    NotAnObject,
    /// A `version` key was present but its value was not a positive integer.
    BadVersion,
    /// A `version` key was present with an unsupported value.
    UnsupportedVersion(u64),
}

impl fmt::Display for FlowDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "flowData is not valid JSON: {e}"),
            Self::NotAnObject => write!(f, "flowData top-level JSON is not an object"),
            Self::BadVersion => write!(f, "flowData `version` is not a positive integer"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "flowData envelope version {v} is unsupported (expected {FLOW_DATA_VERSION})"
                )
            }
        }
    }
}

impl std::error::Error for FlowDataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for FlowDataError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// The persisted-flow version a `flowData` blob represents — `2` if it
/// carries a v2 envelope, `1` if it is a bare v1.1 [`FlowDefinition`].
/// This is the value the API returns as `flowVersion` (`http-api.md` §2.2).
pub fn flow_version(flow_data: &str) -> Result<u8, FlowDataError> {
    let value: serde_json::Value = serde_json::from_str(flow_data)?;
    let obj = value.as_object().ok_or(FlowDataError::NotAnObject)?;
    match obj.get("version") {
        None => Ok(1),
        Some(v) => {
            let ver = v.as_u64().ok_or(FlowDataError::BadVersion)?;
            u8::try_from(ver).map_err(|_| FlowDataError::UnsupportedVersion(ver))
        }
    }
}

/// The repo deserializer (`http-api.md` §5.1): decode a `flowData` blob into
/// a [`FlowGraph`].
///
/// - A blob with `version == 2` is read as a [`FlowDataEnvelope`].
/// - A blob with **no** `version` key is a legacy v1.1 [`FlowDefinition`] —
///   it is parsed and run through the projection shim ([`project_legacy`]).
///
/// This is how the v2 engine runs legacy linear flows untouched, with no
/// bulk migration (`architecture.md` §9).
pub fn decode_flow_data(flow_data: &str) -> Result<FlowGraph, FlowDataError> {
    let value: serde_json::Value = serde_json::from_str(flow_data)?;
    let obj = value.as_object().ok_or(FlowDataError::NotAnObject)?;
    match obj.get("version") {
        Some(v) => {
            let ver = v.as_u64().ok_or(FlowDataError::BadVersion)?;
            if ver != u64::from(FLOW_DATA_VERSION) {
                return Err(FlowDataError::UnsupportedVersion(ver));
            }
            let envelope: FlowDataEnvelope = serde_json::from_value(value)?;
            Ok(envelope.graph)
        }
        None => {
            let definition: FlowDefinition = serde_json::from_value(value)?;
            Ok(project_legacy(&definition))
        }
    }
}

/// Encode a graph into the canonical v2 `flowData` blob — the versioned
/// envelope `{ "version": 2, "graph": { … } }`.
pub fn encode_flow_data(graph: &FlowGraph) -> String {
    // Serialising a `FlowDataEnvelope` of owned data cannot fail.
    serde_json::to_string(&FlowDataEnvelope::new(graph.clone()))
        .expect("FlowDataEnvelope serialisation is infallible")
}

// ---------------------------------------------------------------------------
// Projection shim (architecture.md §9.1)
// ---------------------------------------------------------------------------

/// Canvas y-spacing between projected nodes — purely cosmetic (the engine
/// ignores [`Position`]); gives `POST /convert` a sane top-to-bottom layout.
const PROJECTION_ROW_GAP: f64 = 120.0;

/// Project a legacy v1.1 [`FlowDefinition`] into a degenerate **path graph**:
/// a `trigger` node followed by one `action` node per list entry, chained
/// `out → in`, every join `all`, no branches (`architecture.md` §9.1).
///
/// Run under the v2 topological scheduler a path graph is observably
/// identical to the old serial loop — one node ready at a time,
/// abort-on-first-error via unwired `err` ports. This is the single
/// mechanism by which legacy linear flows keep working with no operator
/// action and no migration.
pub fn project_legacy(definition: &FlowDefinition) -> FlowGraph {
    let mut nodes = Vec::with_capacity(definition.actions.len() + 1);
    let mut edges = Vec::with_capacity(definition.actions.len());

    let trigger_id = NodeId("trigger".to_string());
    nodes.push(Node {
        id: trigger_id.clone(),
        label: None,
        position: Position { x: 0.0, y: 0.0 },
        kind: NodeKind::Trigger {
            config: definition.trigger.clone(),
        },
    });

    let mut prev = PortRef {
        node: trigger_id,
        port: "out".to_string(),
    };

    for (index, action) in definition.actions.iter().enumerate() {
        let node_id = NodeId(format!("action_{index}"));
        nodes.push(Node {
            id: node_id.clone(),
            label: None,
            position: Position {
                x: 0.0,
                y: ((index + 1) as f64) * PROJECTION_ROW_GAP,
            },
            kind: NodeKind::Action {
                config: action.clone(),
            },
        });
        edges.push(Edge {
            id: EdgeId(format!("e{index}")),
            from: prev,
            to: PortRef {
                node: node_id.clone(),
                port: "in".to_string(),
            },
            join_policy: JoinPolicy::All,
        });
        prev = PortRef {
            node: node_id,
            port: "out".to_string(),
        };
    }

    FlowGraph { nodes, edges }
}

// ---------------------------------------------------------------------------
// HTTP API surface — request / response shapes (http-api.md §2-§3)
// ---------------------------------------------------------------------------

/// The `flowData`-envelope version a flow row represents on the wire — `1`
/// for a legacy linear flow, `2` for a graph flow (`http-api.md` §2.2). The
/// API now always *stores* v2 envelopes; a `1` is only ever a row written
/// by the pre-v2 engine, upgraded by an explicit `POST /convert`.
pub type FlowVersion = u8;

/// The mutually-exclusive create / update body shape (`http-api.md` §2.2).
/// A v2 client sends `{ "graph": … }`; a v1.1-era client (or a script) may
/// still send `{ "definition": … }` — a one-release back-compat courtesy,
/// projected to a graph on write. The two are surfaced on the wire as two
/// optional, mutually-exclusive keys ([`CreateFlowBody`] / [`UpdateFlowBody`])
/// and resolved into this enum server-side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlowSpec {
    /// v2 — the canonical graph shape.
    Graph { graph: FlowGraph },
    /// v1.1 — a legacy linear definition, projected via [`project_legacy`].
    Legacy { definition: FlowDefinition },
}

/// Resolve a `graph` / `definition` optional pair into a [`FlowSpec`].
/// `graph` wins if (wrongly) both are present is *not* allowed — the doc's
/// "mutually-exclusive" rule is enforced as an error.
fn resolve_spec(
    graph: &Option<FlowGraph>,
    definition: &Option<FlowDefinition>,
) -> Result<FlowSpec, &'static str> {
    match (graph, definition) {
        (Some(_), Some(_)) => Err("send either `graph` or `definition`, not both"),
        (Some(g), None) => Ok(FlowSpec::Graph { graph: g.clone() }),
        (None, Some(d)) => Ok(FlowSpec::Legacy {
            definition: d.clone(),
        }),
        (None, None) => Err("a flow needs a `graph` (v2) or a `definition` (v1.1)"),
    }
}

/// `POST /api/flows` body (`http-api.md` §2.2). Keeps every v1.1 field; the
/// definition is the untagged `graph` / `definition` pair — exactly one is
/// required. Resolve it with [`CreateFlowBody::spec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFlowBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<FlowGraph>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<FlowDefinition>,
}

impl CreateFlowBody {
    /// Resolve the create body to its [`FlowSpec`] — `Err` if neither or
    /// both of `graph` / `definition` were sent.
    pub fn spec(&self) -> Result<FlowSpec, &'static str> {
        resolve_spec(&self.graph, &self.definition)
    }
}

/// `PATCH /api/flows/{id}` body (`http-api.md` §2.2 / §4). Every field is
/// optional, including the graph swap (a present `graph` *or* `definition`).
/// Use [`UpdateFlowBody::spec`] to resolve the swap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFlowBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::double_option"
    )]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_server_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<FlowGraph>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<FlowDefinition>,
}

impl UpdateFlowBody {
    /// Resolve the optional graph swap. `Ok(None)` — no spec change;
    /// `Ok(Some(spec))` — replace the definition; `Err` — both shapes sent.
    pub fn spec(&self) -> Result<Option<FlowSpec>, &'static str> {
        match (&self.graph, &self.definition) {
            (None, None) => Ok(None),
            (g, d) => resolve_spec(g, d).map(Some),
        }
    }
}

/// One flow as the v2 API returns it (`http-api.md` §2.2): every v1.1
/// [`super::Flow`] field, an explicit [`FlowVersion`], and the stored spec
/// shape — `graph` for a v2 flow, `definition` for a legacy one (exactly
/// one is ever present). PURA-267's canvas consumes this verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowView {
    pub id: FlowId,
    pub name: String,
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub enabled: bool,
    pub flow_version: FlowVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<FlowGraph>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<FlowDefinition>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<FlowRunSummaryView>,
}

impl FlowView {
    /// Read the flow's spec back as a [`FlowSpec`] regardless of which key
    /// carried it.
    pub fn spec(&self) -> Result<FlowSpec, &'static str> {
        resolve_spec(&self.graph, &self.definition)
    }
}

/// Compact run row — the v1.1 summary plus a [`FlowVersion`] badge
/// (`http-api.md` §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRunSummaryView {
    pub id: FlowRunId,
    pub status: FlowRunStatus,
    pub flow_version: FlowVersion,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
}

/// One run as `GET /api/flows/{id}/runs/{runId}` returns it (`http-api.md`
/// §3.2): the summary flattened in, plus the trigger document, the v1.1
/// `actionResults`, and the v2 `nodeResults` array — the canvas run-overlay
/// source. The list endpoint reuses this type but emits `nodeResults: []`
/// to stay light; the detail endpoint populates it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRunView {
    #[serde(flatten)]
    pub summary: FlowRunSummaryView,
    pub flow_id: FlowId,
    pub trigger: serde_json::Value,
    pub error: Option<String>,
    pub action_results: Vec<ActionResult>,
    pub node_results: Vec<NodeResult>,
}

/// `GET /api/flows` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFlowsView {
    pub flows: Vec<FlowView>,
}

/// `GET /api/flows/{id}/runs` response. `nextCursor` is `None` on the last
/// page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRunsView {
    pub runs: Vec<FlowRunView>,
    pub next_cursor: Option<FlowRunId>,
}

/// One structural validation failure (`http-api.md` §3.1). `code` is the
/// stable discriminant the canvas branches on; the locator fields
/// (`nodes` / `node` / `edge`) are populated as the `code` warrants and
/// otherwise omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationError {
    /// `graph_cycle`, `unreachable_node`, `unknown_port`,
    /// `port_unconnected`, `multiple_triggers`, `no_trigger`,
    /// `subflow_cycle`, `subflow_missing`, `size_exceeded`,
    /// `bad_expression`, `bad_duration`, …
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
}

impl ValidationError {
    /// A bare `code` + `message` failure with no locator.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            nodes: Vec::new(),
            node: None,
            edge: None,
        }
    }

    /// Builder — attach the offending node id.
    pub fn at_node(mut self, node: &NodeId) -> Self {
        self.node = Some(node.0.clone());
        self
    }

    /// Builder — attach the offending edge id.
    pub fn at_edge(mut self, edge: &EdgeId) -> Self {
        self.edge = Some(edge.0.clone());
        self
    }

    /// Builder — attach a node-set (a cycle path).
    pub fn with_nodes(mut self, nodes: Vec<String>) -> Self {
        self.nodes = nodes;
        self
    }
}

/// One non-blocking validation advisory (`http-api.md` §3.1). Shares the
/// shape of [`ValidationError`]; warnings (e.g. type-hint mismatches, a
/// dead branch case) do not block a save.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationWarning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
}

impl ValidationWarning {
    /// A `code` + `message` advisory pinned to a node.
    pub fn at_node(code: &str, message: impl Into<String>, node: &NodeId) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            nodes: Vec::new(),
            node: Some(node.0.clone()),
            edge: None,
        }
    }
}

/// `POST /api/flows/validate` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateGraphRequest {
    pub graph: FlowGraph,
}

/// `POST /api/flows/validate` response (`http-api.md` §3.1). `valid` is
/// `errors.is_empty()`; warnings never flip it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateGraphResponse {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationWarning>,
}

/// The `400` body a graph-validation failure returns on `POST` / `PATCH`
/// (`http-api.md` §4). Unlike the bare `ErrorBody`, it carries the full
/// `errors` array so the canvas can render every failure inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphInvalidBody {
    /// Always the discriminant `"graph_invalid"`.
    pub error: String,
    pub errors: Vec<ValidationError>,
}

impl GraphInvalidBody {
    /// Wrap a non-empty `errors` array in the `graph_invalid` envelope.
    pub fn new(errors: Vec<ValidationError>) -> Self {
        Self {
            error: "graph_invalid".to_string(),
            errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_graph() -> FlowGraph {
        FlowGraph {
            nodes: vec![
                Node {
                    id: NodeId("start".into()),
                    label: None,
                    position: Position { x: 0.0, y: 0.0 },
                    kind: NodeKind::Trigger {
                        config: Trigger::Ts6ClientJoined {
                            channel_id: Some(5),
                        },
                    },
                },
                Node {
                    id: NodeId("by_channel".into()),
                    label: Some("Route by channel".into()),
                    position: Position { x: 0.0, y: 120.0 },
                    kind: NodeKind::Branch {
                        cases: vec![BranchCase {
                            label: "lobby".into(),
                            when: "trigger.channelId == 1".into(),
                        }],
                    },
                },
                Node {
                    id: NodeId("welcome".into()),
                    label: None,
                    position: Position { x: 0.0, y: 240.0 },
                    kind: NodeKind::Action {
                        config: Action::LogLine {
                            message: "hi".into(),
                        },
                    },
                },
            ],
            edges: vec![
                Edge {
                    id: EdgeId("e0".into()),
                    from: PortRef {
                        node: NodeId("start".into()),
                        port: "out".into(),
                    },
                    to: PortRef {
                        node: NodeId("by_channel".into()),
                        port: "in".into(),
                    },
                    join_policy: JoinPolicy::All,
                },
                Edge {
                    id: EdgeId("e1".into()),
                    from: PortRef {
                        node: NodeId("by_channel".into()),
                        port: "lobby".into(),
                    },
                    to: PortRef {
                        node: NodeId("welcome".into()),
                        port: "in".into(),
                    },
                    join_policy: JoinPolicy::Any,
                },
            ],
        }
    }

    #[test]
    fn node_kind_flattens_with_kind_discriminant() {
        let node = Node {
            id: NodeId("greet_each".into()),
            label: None,
            position: Position { x: 1.0, y: 2.0 },
            kind: NodeKind::Parallel {
                collection: "trigger.newClients".into(),
                sub_flow_id: FlowId(42),
                max_concurrency: 4,
            },
        };
        let json = serde_json::to_string(&node).unwrap();
        // `kind` rides at the top level alongside `id` (flattened).
        assert!(json.contains(r#""kind":"parallel""#), "got: {json}");
        assert!(json.contains(r#""id":"greet_each""#), "got: {json}");
        assert!(json.contains(r#""subFlowId":42"#), "got: {json}");
        assert!(json.contains(r#""maxConcurrency":4"#), "got: {json}");
        // `label` is absent on the wire when `None`.
        assert!(!json.contains("label"), "got: {json}");
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn delay_for_keyword_is_renamed_on_the_wire() {
        let node_kind = NodeKind::Delay {
            r#for: "30s".into(),
        };
        let json = serde_json::to_string(&node_kind).unwrap();
        assert!(json.contains(r#""for":"30s""#), "got: {json}");
        let back: NodeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, node_kind);
    }

    #[test]
    fn transform_output_is_untagged() {
        let expr = TransformOutput::Expr("trigger.channelId".into());
        assert_eq!(
            serde_json::to_string(&expr).unwrap(),
            r#""trigger.channelId""#
        );
        let mut map = BTreeMap::new();
        map.insert("userId".to_string(), "trigger.clientDatabaseId".to_string());
        let obj = TransformOutput::Object(map);
        let json = serde_json::to_string(&obj).unwrap();
        assert!(json.starts_with('{'), "got: {json}");
        assert_eq!(serde_json::from_str::<TransformOutput>(&json).unwrap(), obj);
        assert_eq!(
            serde_json::from_str::<TransformOutput>(&serde_json::to_string(&expr).unwrap())
                .unwrap(),
            expr
        );
    }

    #[test]
    fn join_policy_defaults_to_all_when_absent() {
        // An edge wire blob with no `joinPolicy` key decodes to `All`.
        let edge: Edge = serde_json::from_str(
            r#"{"id":"e9","from":{"node":"a","port":"out"},"to":{"node":"b","port":"in"}}"#,
        )
        .unwrap();
        assert_eq!(edge.join_policy, JoinPolicy::All);
        let json = serde_json::to_string(&JoinPolicy::Any).unwrap();
        assert_eq!(json, r#""any""#);
    }

    #[test]
    fn node_status_uses_snake_case_on_the_wire() {
        for (status, expected) in [
            (NodeStatus::Ok, "\"ok\""),
            (NodeStatus::Errored, "\"errored\""),
            (NodeStatus::Skipped, "\"skipped\""),
            (NodeStatus::Interrupted, "\"interrupted\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }

    #[test]
    fn node_result_round_trips_with_camel_case_wire() {
        let result = NodeResult {
            node_id: NodeId("welcome_msg".into()),
            kind: "action".into(),
            status: NodeStatus::Ok,
            started_at: Utc::now(),
            finished_at: None,
            duration_ms: Some(318),
            error: None,
            output: Some(serde_json::json!({ "sent": true })),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains(r#""nodeId":"welcome_msg""#), "got: {json}");
        assert!(json.contains(r#""durationMs":318"#), "got: {json}");
        assert!(!json.contains("node_id"), "snake_case leaked: {json}");
        let back: NodeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn flow_graph_round_trips_through_the_v2_envelope() {
        let graph = sample_graph();
        let blob = encode_flow_data(&graph);
        // The envelope carries the version marker.
        assert!(blob.contains(r#""version":2"#), "got: {blob}");
        assert_eq!(flow_version(&blob).unwrap(), 2);
        // Decoding the blob yields the same graph.
        assert_eq!(decode_flow_data(&blob).unwrap(), graph);
    }

    #[test]
    fn decode_rejects_an_unsupported_envelope_version() {
        let blob = r#"{"version":99,"graph":{"nodes":[],"edges":[]}}"#;
        assert!(matches!(
            decode_flow_data(blob),
            Err(FlowDataError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn legacy_definition_projects_to_a_path_graph() {
        let definition = FlowDefinition {
            trigger: Trigger::ManualFire,
            actions: vec![
                Action::LogLine {
                    message: "one".into(),
                },
                Action::LogLine {
                    message: "two".into(),
                },
            ],
        };
        let graph = project_legacy(&definition);

        // trigger + one node per action.
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);

        // The trigger is the unique entry node, id `trigger`.
        assert_eq!(graph.nodes[0].id, NodeId("trigger".into()));
        assert!(matches!(graph.nodes[0].kind, NodeKind::Trigger { .. }));

        // Action nodes are a chained path: trigger.out → action_0.in,
        // action_0.out → action_1.in, every join `all`.
        assert_eq!(graph.edges[0].from.node, NodeId("trigger".into()));
        assert_eq!(graph.edges[0].from.port, "out");
        assert_eq!(graph.edges[0].to.node, NodeId("action_0".into()));
        assert_eq!(graph.edges[0].to.port, "in");
        assert_eq!(graph.edges[1].from.node, NodeId("action_0".into()));
        assert_eq!(graph.edges[1].to.node, NodeId("action_1".into()));
        assert!(graph.edges.iter().all(|e| e.join_policy == JoinPolicy::All));
    }

    #[test]
    fn decode_flow_data_falls_back_to_the_projection_shim() {
        // A legacy blob — bare `FlowDefinition`, no `version` key.
        let legacy =
            r#"{"trigger":{"kind":"manualFire"},"actions":[{"kind":"logLine","message":"hi"}]}"#;
        assert_eq!(flow_version(legacy).unwrap(), 1);
        let graph = decode_flow_data(legacy).unwrap();
        // Decoded via the shim into a 2-node path graph.
        assert_eq!(graph.nodes.len(), 2);
        assert!(matches!(graph.nodes[0].kind, NodeKind::Trigger { .. }));
        assert!(matches!(graph.nodes[1].kind, NodeKind::Action { .. }));
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn projection_of_a_triggerless_action_list_is_just_the_trigger() {
        // Defensive: an empty action list projects to a lone trigger node.
        let definition = FlowDefinition {
            trigger: Trigger::ManualFire,
            actions: vec![],
        };
        let graph = project_legacy(&definition);
        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.edges.is_empty());
    }
}
