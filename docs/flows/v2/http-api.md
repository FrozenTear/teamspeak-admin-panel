# Flow engine v2 — HTTP API & persistence spec

- **Status:** draft, pending board ratification — ratify gate under [PURA-259](/PURA/issues/PURA-259), authored by [PURA-260](/PURA/issues/PURA-260).
- **Companion docs:** [`architecture.md`](./architecture.md), [`ui-brief.md`](./ui-brief.md), [`gate.md`](./gate.md).
- **Supersedes:** [`../http-api.md`](../http-api.md) (v1.1 wire spec, shipped `v1.1` tag — **not** edited by this work).
- **Wire types:** new `crates/shared/src/flows/v2.rs`, re-exported as `ts6_manager_shared::flows::v2`. The v1.1 `flows.rs` module is **kept** — the projection shim ([`architecture.md`](./architecture.md) §9) deserialises legacy rows through it.

All v2 wire types are `#[serde(rename_all = "camelCase")]`, matching v1.1 and `music_bots`. Non-2xx responses keep the v1.1 `flows::ErrorBody` envelope `{ "error": <discriminant>, "message": <human> }` — reused, not redefined.

## 1. Endpoints

| Method | Path | Auth | Purpose |
| ------ | ---- | ---- | ------- |
| `GET`    | `/api/flows`                       | `RequireAuth`  | List flows. `?virtualServerId=`, `?enabled=` filters. |
| `POST`   | `/api/flows`                       | `RequireAdmin` | Create a flow (graph or — back-compat — a v1.1 definition). |
| `GET`    | `/api/flows/{id}`                  | `RequireAuth`  | Fetch one flow. |
| `PATCH`  | `/api/flows/{id}`                  | `RequireAdmin` | Partial update. |
| `DELETE` | `/api/flows/{id}`                  | `RequireAdmin` | Delete. `?force=true` interrupts live runs first. |
| `POST`   | `/api/flows/{id}/fire`             | `RequireAdmin` | Manual fire. Returns the `runId`. Always allowed. |
| `GET`    | `/api/flows/{id}/runs`             | `RequireAuth`  | Run history (summaries). `?limit=`, `?cursor=`. |
| `GET`    | `/api/flows/{id}/runs/{runId}`     | `RequireAuth`  | **New.** One run with full `nodeResults` — the run-overlay source. |
| `POST`   | `/api/flows/validate`              | `RequireAdmin` | **New.** Validate a graph without persisting; returns structured errors for the canvas. |
| `POST`   | `/api/flows/{id}/convert`          | `RequireAdmin` | **New.** Convert a legacy v1.1 flow to a v2 graph in place. |

The five v1.1 endpoints keep their paths, methods, auth, and the global per-IP rate bucket. `POST /fire` keeps the per-flow soft cap of 1 fire / 2 s (`429`). Three endpoints are added — the run-detail read, graph validation, and legacy conversion — the rest is unchanged.

## 2. Wire types (`crates/shared/src/flows/v2.rs`)

```rust
//! v2 graph wire types — flow-engine redesign (PURA-259 / PURA-260).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts6_manager_shared::flows::{FlowId, FlowRunId, FlowRunStatus, ErrorBody}; // reused from v1.1

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);   // slug — referenced by expressions and run records

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeId(pub String);

/// The persisted graph. Stored inside the versioned `flowData` envelope (§5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowGraph {
    pub nodes: Vec<Node>,       // 1..=64
    pub edges: Vec<Edge>,       // 0..=128
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: NodeId,
    #[serde(default)]
    pub label: Option<String>,
    pub position: Position,     // canvas coords; ignored by the engine
    #[serde(flatten)]
    pub kind: NodeKind,         // tag = "kind" — see below
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position { pub x: f64, pub y: f64 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeKind {
    Trigger   { config: Trigger },                 // Trigger reused from flows.rs (v1.1)
    Action    { config: Action },                  // Action  reused from flows.rs (v1.1)
    Branch    { cases: Vec<BranchCase> },
    Parallel  { collection: String, sub_flow_id: FlowId, #[serde(default = "mc_default")] max_concurrency: u8 },
    Delay     { r#for: String },                   // duration string, <= 15m
    Transform { output: TransformOutput },
    Subflow   { sub_flow_id: FlowId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchCase { pub label: String, pub when: String }   // `when` is a boolean expression

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransformOutput {
    Object(std::collections::BTreeMap<String, String>),  // field -> expression
    Expr(String),                                        // single expression
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub id: EdgeId,
    pub from: PortRef,
    pub to: PortRef,
    #[serde(default)]
    pub join_policy: JoinPolicy,   // read off the *target* node; carried per-edge for wire simplicity
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortRef { pub node: NodeId, pub port: String }

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy { #[default] All, Any }

fn mc_default() -> u8 { 4 }
```

### 2.1 Run record additions

`FlowRun` (v1.1, [`../http-api.md`](../http-api.md) §2) gains one field — `nodeResults`. v1.1 runs leave it empty and populate `actionResults`; v2 runs do the inverse.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeResult {
    pub node_id: NodeId,
    pub kind: String,                       // "action", "branch", …
    pub status: NodeStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub output: Option<serde_json::Value>,  // capped 8 kB; { "_truncated": true } past the cap
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus { Ok, Errored, Skipped, Interrupted }
```

`FlowRunStatus` is the **unchanged** v1.1 enum (`in_flight | ok | errored | interrupted | skipped_disabled`) — `skipped` is a *node* status only ([`architecture.md`](./architecture.md) §6.3).

### 2.2 Create / update requests

`CreateFlowRequest` / `UpdateFlowRequest` keep every v1.1 field. The `definition: FlowDefinition` field becomes **one of two mutually-exclusive shapes**, decided by an untagged `flowSpec`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlowSpec {
    Graph { graph: FlowGraph },                         // v2 — canonical
    Legacy { definition: ts6_manager_shared::flows::FlowDefinition },  // v1.1 — accepted for back-compat
}
```

- A v2 client sends `{ "graph": { nodes, edges } }`.
- A v1.1-era client (or a script) may still send `{ "definition": { trigger, actions } }`; the server projects it ([`architecture.md`](./architecture.md) §9) and stores a v2 envelope. Accepting the legacy shape is a **one-release courtesy** so the v1.1 gate probe and any external callers keep working; it is removed in v2.1.
- Responses always return the stored shape plus an explicit `flowVersion` field (`1` or `2`).

## 3. New endpoints

### 3.1 `POST /api/flows/validate`

Validates a graph **without persisting** — the canvas calls this on every meaningful edit so the operator sees structural errors before saving.

Request: `{ "graph": { nodes, edges } }`.

Response `200 OK`:

```json
{ "valid": false,
  "errors": [
    { "code": "graph_cycle",     "nodes": ["a","b","c"], "message": "cycle: a → b → c → a" },
    { "code": "unknown_port",    "edge": "e7",           "message": "node 'fetch' has no output port 'result'" }
  ],
  "warnings": [
    { "code": "type_hint_mismatch", "edge": "e3", "message": "array wired into a string-hinted port" }
  ] }
```

- `errors` block a save; `warnings` (e.g. advisory type-hint mismatches, [`architecture.md`](./architecture.md) §7.3) do not.
- Error `code`s: `graph_cycle`, `unreachable_node`, `unknown_port`, `port_unconnected`, `multiple_triggers`, `no_trigger`, `subflow_cycle`, `subflow_missing`, `size_exceeded`, `bad_expression`, `bad_duration`.
- The same validation runs inside `POST`/`PATCH`; this endpoint just exposes it ahead of a write.

### 3.2 `GET /api/flows/{id}/runs/{runId}`

Returns one `FlowRun` with the full `nodeResults` array — the data source for the canvas **run-overlay** ([`ui-brief.md`](./ui-brief.md) §5). The list endpoint `GET /runs` returns summaries only (no `nodeResults`/`output`) to keep the history page light; the detail endpoint carries the heavy payload.

`404` if the run id is unknown or not owned by `{id}`.

The canvas polls this endpoint at ~1 s while a run is in flight. A push channel (SSE) is noted as a v2.x enhancement, not in v2 — polling a bounded (≤ 64-node) run is adequate.

### 3.3 `POST /api/flows/{id}/convert`

Converts a **legacy** (`flowVersion: 1`) flow to a v2 graph in place: computes the path-graph projection ([`architecture.md`](./architecture.md) §9), assigns node positions in a simple top-to-bottom layout, and rewrites `flowData` as a v2 envelope.

- `200 OK` → the converted `Flow` (`flowVersion: 2`).
- `409 Conflict` `{ "error": "already_graph" }` — the flow is already v2.
- `409 Conflict` `{ "error": "definition_swap_locked" }` — the flow is enabled (disable to convert; conversion changes the definition).

Conversion is **opt-in and operator-initiated** — there is no bulk/automatic migration ([`architecture.md`](./architecture.md) §9).

## 4. Changed endpoint behaviour

- **`POST /api/flows`** — accepts `FlowSpec` (§2.2). Validation (§3.1) runs before insert; a structural error returns `400` with the `errors` array embedded in `ErrorBody.message` is insufficient, so the body is `{ "error": "graph_invalid", "errors": [ … ] }` (the validate-style payload). Name uniqueness `409 name_taken` is unchanged.
- **`PATCH /api/flows/{id}`** — a graph swap obeys the v1.1 rule: replacing the graph is rejected `409 definition_swap_locked` while the flow is `enabled`. Patching `name`/`description`/`virtualServerId`/`enabled` on a live flow is fine.
- **`DELETE`**, **`POST /fire`**, **`GET /runs`** — unchanged from v1.1, except `GET /runs` summaries now carry `flowVersion` for the row badge.

## 5. Persistence

**No SurrealDB schema migration.** Both tables keep their v1.1 shape (table names, columns, sequence ids, `PROJECTION` strings) — see [`../architecture.md`](../architecture.md) §5.

### 5.1 `bot_flow.flowData` — versioned envelope

`flowData` is an opaque JSON `String` column. v2 writes:

```json
{ "version": 2, "graph": { "nodes": [ … ], "edges": [ … ] } }
```

A row whose decoded `flowData` has **no `version` key** is a legacy v1.1 flow — its content is the bare `FlowDefinition` `{ "trigger": …, "actions": [ … ] }`. The repo deserializer:

```text
parse flowData as JSON
  if object has "version" == 2  -> FlowGraph from .graph
  else                          -> v1.1 FlowDefinition -> project to FlowGraph (shim)
```

No `ALTER TABLE`, no new column, no backfill job. Legacy rows are upgraded only by an explicit `POST /convert` (§3.3).

### 5.2 `bot_flow_run` — `nodeResults`

The run row already stores its result payload as a JSON string. v2 adds `nodeResults` (a JSON array of `NodeResult`, §2.1) alongside the existing `actionResults`:

- A run produced by the v2 engine writes `nodeResults`, leaves `actionResults` `[]`.
- Historical v1.1 runs keep their `actionResults`; `nodeResults` reads back `[]`.
- The per-flow 200-run cap and 30-day TTL janitor are **unchanged**. The 8 kB per-node `output` cap ([`architecture.md`](./architecture.md) §6.4) keeps a v2 run row bounded (≤ 64 nodes).

## 6. Error envelope catalogue

The v1.1 catalogue ([`../http-api.md`](../http-api.md) §4) is reused. Additions:

| `error` discriminant | HTTP | Used when |
| -------------------- | ---- | --------- |
| `graph_invalid`      | 400  | Create/update graph failed structural validation; body carries an `errors` array. |
| `already_graph`      | 409  | `POST /convert` on a flow that is already v2. |
| `bad_expression`     | 400  | An expression field failed to parse at write time. |
| `subflow_missing`    | 400  | A `subflow`/`parallel` node references a non-existent flow id. |
| `subflow_cycle`      | 400  | The static sub-flow reference graph is cyclic. |

`definition_swap_locked`, `name_taken`, `run_in_flight`, `not_found`, `forbidden`, `rate_limited`, `engine_saturated`, `internal` carry over unchanged.

## 7. Test surface

The wire/persistence implementation child opens cases in `flow/routes_tests.rs` (extended) covering:

1. `POST` round-trip of a multi-node graph: `201`, plus `400 graph_invalid` for a cycle, an unreachable node, and an unknown port.
2. `POST` with a legacy `definition` body → stored as a v2 envelope, `flowVersion: 2` on read-back.
3. `POST /validate` returns the expected `errors`/`warnings` for a hand-built bad graph.
4. `POST /convert` projects a legacy flow; `409 already_graph` on a second call; `409 definition_swap_locked` when enabled.
5. `GET /runs/{runId}` returns populated `nodeResults` for a v2 run and `[]` for a legacy run.
6. `PATCH` graph swap blocked while `enabled = true`.
7. `RequireAdmin` on every write route; `ErrorBody` on every non-2xx.

## 8. References

- [PURA-259](/PURA/issues/PURA-259) — Phase 8 epic.
- [PURA-260](/PURA/issues/PURA-260) — this design brief.
- [`architecture.md`](./architecture.md) — graph model & engine.
- [`../http-api.md`](../http-api.md) — v1.1 wire spec (reused `ErrorBody`, `FlowId`, `FlowRunStatus`).
- `crates/shared/src/flows.rs` — v1.1 types, kept for the projection shim.
- `crates/ts6-manager-server/src/flow/routes.rs` — router (extended with three routes).
- `crates/ts6-manager-server/src/repos/bot_flows.rs`, `bot_flow_runs.rs` — repos (shape unchanged).
