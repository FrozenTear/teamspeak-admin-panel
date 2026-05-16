# Flow engine v2 — architecture brief (graph/node)

- **Status:** draft, pending board ratification — ratify gate under [PURA-259](/PURA/issues/PURA-259), authored by [PURA-260](/PURA/issues/PURA-260).
- **Greenlight:** board accepted the v1.2 flow-engine redesign `request_confirmation` on [PURA-227](/PURA/issues/PURA-227), 2026-05-16.
- **Supersedes:** the v1.1 linear engine documented in [`../architecture.md`](../architecture.md) (shipped at the `v1.1` tag — **not** edited by this work).
- **Companion docs:** [`http-api.md`](./http-api.md), [`ui-brief.md`](./ui-brief.md), [`gate.md`](./gate.md).

## 1. Purpose

v1.1 shipped a flow as **one trigger → an ordered list of 1–8 actions**, executed strictly serially, aborting on the first error. That model proved the wedge (V6 back on the gate) but it cannot express the three things operators asked for next:

- **Conditional work** — "send a *different* message depending on which channel the client joined".
- **Concurrent work** — "kick the client *and* post a webhook, don't make one wait for the other".
- **Composed work** — "reuse my welcome sub-routine from three different flows".

v2 reshapes a flow from a **list** into a **directed acyclic graph** of typed nodes. This document specifies the graph model and the execution engine. The wire/persistence surface, the canvas builder, and the gate plan live in the sibling docs.

This is a **topology and engine** redesign. It does **not** add new *action kinds* or (per §11) new *trigger sources* — those are orthogonal widenings that the graph model makes cheaper to land later.

## 2. v2 scope vs deferred

### 2.1 In scope (v2)

- **Graph model** — node set + typed edges, DAG-only (§3, §4).
- **Execution engine** — topological scheduling, branch/merge, bounded parallelism, per-node run records, structured error propagation (§5, §6).
- **Data flow** — a per-run blackboard + an expression dialect; replaces v1.1's single-pass `${…}` substitution (§7).
- **Persistence** — versioned `flowData` envelope, `nodeResults` on `bot_flow_run`; **zero schema migration** (§8, and [`http-api.md`](./http-api.md) §5).
- **v1.1 coexistence** — the v2 engine runs legacy linear flows via a path-graph projection shim; the v1.1 linear executor is deleted (§9).
- **Canvas UI** — visual node builder with a live run-overlay; replaces the v1.1 form pages (see [`ui-brief.md`](./ui-brief.md)).
- **Gate** — a multi-node (branch + parallel) probe in `scripts/ws-gate/` (see [`gate.md`](./gate.md)).

### 2.2 Out of scope (defer to v2.x+)

- **New trigger sources** — WebQuery-event and sidecar-telemetry triggers stay deferred (§11; board to confirm).
- **Multiple trigger nodes per graph** — v2 graphs have exactly one `trigger` node (§3.1).
- **Loop edges / unbounded iteration** — v2 is DAG-only. Bounded iteration over a collection is the `parallel` node (§4.4). Cycles are rejected at validation time.
- **Auto-retry / backoff** — a node that errors does not retry. The per-node run record (§6.4) makes retry a tractable v2.x add.
- **Durable long-running flows** — `delay` is bounded to ≤ 15 min and in-memory; a restart interrupts parked runs (§4.5). Hour/day-scale scheduled resumption is a future item.
- **Multi-manager** — single-manager only, unchanged from v1.1.
- **Per-server-group permission grain** — admin-only writes, unchanged from v1.1.

## 3. The graph model

A v2 flow is a **`FlowGraph`**: a set of **nodes** and a set of directed **edges** between their **ports**. The graph is acyclic (§5.1).

```
FlowGraph {
  nodes:  [ Node ... ]   // 1..=64
  edges:  [ Edge ... ]   // 0..=128
}

Node {
  id:        NodeId      // stable, unique, human-readable slug — "fetch_user", "welcome_msg"
  kind:      NodeKind    // one of the 7 kinds in §4
  label:     String?     // operator-facing display name; defaults to id
  position:  { x, y }    // canvas coordinates; ignored by the engine
  config:    <kind-specific>   // see §4
}

Edge {
  id:   EdgeId
  from: { node: NodeId, port: PortId }   // an output port
  to:   { node: NodeId, port: PortId }   // an input port
}
```

`NodeId` is a slug, not an integer: it is referenced by expressions (§7) and by the run record, so it must be stable and legible. The HTTP/persistence shape is pinned in [`http-api.md`](./http-api.md) §2.

### 3.1 Structural invariants (validated on every write)

1. **Exactly one `trigger` node.** It is the unique source — 0 inbound edges. Multi-trigger graphs are deferred (§2.2).
2. **Acyclic.** Kahn's algorithm over the edge set; a non-empty residue ⇒ reject (`error: "graph_cycle"`).
3. **Every non-trigger node is reachable** from the trigger node. Unreachable nodes ⇒ reject (`error: "unreachable_node"`) — a stranded node is always an authoring mistake.
4. **Ports exist and directions match.** An edge's `from.port` must be an output port of `from.node`'s kind; `to.port` an input port. Unknown port ⇒ reject (`error: "unknown_port"`).
5. **Required input ports are connected.** Every node's `in` port must have ≥ 1 inbound edge (except `trigger`).
6. **Sub-flow references resolve and do not recurse.** A `subflow`/`parallel` node's target flow id must exist, and the static sub-flow reference graph must itself be acyclic (§4.6, `error: "subflow_cycle"`).
7. **Size caps.** ≤ 64 nodes, ≤ 128 edges. These bound the run record (§6.4) and keep the canvas legible.

Validation runs server-side on `POST`/`PATCH` and is also exposed standalone at `POST /api/flows/validate` so the canvas can lint before save (see [`http-api.md`](./http-api.md) §3).

## 4. Node-type catalogue

Seven node kinds. Each has a fixed input/output **port** set (except `branch`, whose output ports are configured). A port carries both a **control signal** (§5.2) and a **data document** (§7).

| Kind        | Inputs | Outputs                       | Side effects | Summary |
| ----------- | ------ | ----------------------------- | ------------ | ------- |
| `trigger`   | —      | `out`                         | none         | Graph entry; emits the trigger event document. |
| `action`    | `in`   | `out`, `err`                  | yes          | Performs one effect; the four v1.1 action kinds. |
| `branch`    | `in`   | one per case + `default`      | none         | Routes control to exactly one output port. |
| `parallel`  | `in`   | `out`, `err`                  | via sub-flow | Fan-out: runs a sub-flow once per collection element, concurrently. |
| `delay`     | `in`   | `out`                         | none         | Parks the path for a bounded duration, then passes data through. |
| `transform` | `in`   | `out`, `err`                  | none         | Pure data reshaping via an expression. |
| `subflow`   | `in`   | `out`, `err`                  | via sub-flow | Runs another flow as a nested run. |

**Static concurrency is topological, not a node.** "Run B and C at the same time" is simply a node with two outgoing edges — the engine schedules both as soon as the source settles (§5). "Wait for B and C, then run E" is a node with two inbound edges — a *join*, governed by `joinPolicy` (§5.3). No dedicated fan-out/join node is needed for the static case; the `parallel` node (§4.4) exists only for **dynamic** fan-out over runtime data.

### 4.1 `trigger`

The unique entry node. Config is the trigger discriminant — **identical catalogue to v1.1** (`cron`, `manualFire`, `ts6ClientJoined`); no widening in v2 (§11).

```json
{ "id": "start", "kind": "trigger",
  "config": { "kind": "ts6ClientJoined", "channelId": 5 } }
```

- Ports: no inputs; one output `out`.
- `out` emits the resolved trigger event document — the same shape v1.1 wrote to `bot_flow_run.trigger`. Idempotency key is unchanged from v1.1 (`(flowId, triggerInstance)`).

### 4.2 `action`

Performs exactly one effect. Config is **the v1.1 `Action` enum, unchanged** — `ts6Command`, `musicBotCommand`, `webhookOut`, `logLine`. The graph redesign deliberately reuses the action catalogue and its dispatch code (`flow/dispatch.rs`); the only change is that arg templating uses the v2 expression dialect (§7) instead of `${…}`.

```json
{ "id": "welcome_msg", "kind": "action",
  "config": { "kind": "ts6Command", "command": "sendtextmessage",
              "args": { "targetmode": 2, "target": "{{ trigger.channelId }}",
                        "msg": "Welcome {{ trigger.clientNickname }}." } } }
```

- Ports: input `in`; outputs `out` (data: the action's result document) and `err` (data: the error document — see §6.2).
- The `err` port is the **try/catch seam**: wire it to handle failures down a recovery path; leave it unwired and an error becomes the run's terminal failure (§6.3).

### 4.3 `branch`

Routes control to **exactly one** outgoing path. Config is an ordered case list; each case has a label and a boolean expression. The first case whose expression is true fires; if none match, `default` fires.

```json
{ "id": "by_channel", "kind": "branch",
  "config": { "cases": [
    { "label": "lobby",   "when": "trigger.channelId == 1" },
    { "label": "support", "when": "trigger.channelId == 7" }
  ] } }
```

- Ports: input `in`; one output port **per case label** plus `default`. Here: `lobby`, `support`, `default`.
- The matched port's edges carry `active`; **every other output port's edges carry `skipped`** (§5.2). This is how a branch prunes a subgraph: the not-taken side propagates `skipped` and its nodes settle as `skipped`, not `errored`.
- A `branch` performs no side effect and cannot itself error; an expression that fails to evaluate is a *validation* failure caught before save, or — defensively — falls through to `default` at run time with a logged warning.

### 4.4 `parallel` — dynamic fan-out

The **bounded-iteration** construct. Takes a collection from its input, runs a referenced **sub-flow** once per element with bounded concurrency, and emits the array of per-element results. This is how v2 expresses "do this for each X" *without* loop edges.

```json
{ "id": "greet_each", "kind": "parallel",
  "config": { "collection": "trigger.newClients",
              "subFlowId": 42,
              "maxConcurrency": 4 } }
```

- Ports: input `in`; outputs `out` (data: `[ <sub-flow result>, … ]`, element order preserved) and `err`.
- `collection` is an expression that must evaluate to a JSON array (≤ 256 elements, validated at run time; over-cap ⇒ node errors).
- Each element is passed as the sub-flow's trigger payload. `maxConcurrency` (1–16, default 4) bounds in-flight element runs; it composes with the per-run node semaphore (§6.5).
- If **any** element run errors, the node errors (the `err` document lists the failed indices); successful element results are still in the error document for inspection. A future `failurePolicy: continue` knob is noted but not in v2.
- Folding fan-out into "a node that delegates to a sub-flow" means the canvas needs **no nested-region UI** — the body is just another flow the operator already knows how to build.

### 4.5 `delay`

Parks the path for a bounded duration, then passes its input data through **unchanged**.

```json
{ "id": "wait_30s", "kind": "delay",
  "config": { "for": "30s" } }
```

- Ports: input `in`; output `out` (data == input data).
- `for` is a duration string; **bounded to ≤ 15 minutes** (validated). Long-horizon scheduling is deferred (§2.2).
- Implemented as an in-memory `tokio::time::sleep` inside the run task. A manager restart **interrupts** a parked run (consistent with v1.1's in-flight → `interrupted` rule, [`../architecture.md`](../architecture.md) §6.4). The UI surfaces this footgun.

### 4.6 `transform`

Pure, side-effect-free data reshaping. Produces a new data document from an expression over the blackboard (§7). This is the node that **replaces complex inline templating** — instead of stuffing expressions into action args, an operator puts a `transform` upstream and feeds a clean document into the action.

```json
{ "id": "build_payload", "kind": "transform",
  "config": { "output": {
      "userId":  "trigger.clientDatabaseId",
      "joinedAt": "trigger.ts",
      "channel":  "trigger.channelId" } } }
```

- Ports: input `in`; outputs `out` (data: the reshaped document) and `err` (an expression that fails — e.g. references a missing key with strict mode — errors the node).
- `output` is either a map of field → expression (object construction) or a single expression producing any JSON value.

### 4.7 `subflow`

Runs **another flow** as a nested run, passing its input as the sub-flow's trigger payload and emitting the sub-flow's terminal output.

```json
{ "id": "run_welcome", "kind": "subflow",
  "config": { "subFlowId": 42 } }
```

- Ports: input `in`; outputs `out` (data: the sub-flow's terminal node output) and `err` (the sub-flow ran to an unhandled error).
- The target flow's own `trigger` node is bypassed — when invoked as a sub-flow it emits the passed-in payload instead of subscribing to its configured source.
- **Recursion guard:** the static sub-flow reference graph (across both `subflow` and `parallel` nodes) must be acyclic — validated on write (§3.1.6). A runtime **nesting-depth cap of 5** is a defensive backstop.

## 5. Execution engine

The v2 engine replaces `flow/engine.rs`'s linear loop with a **topological scheduler**. One flow run executes one graph instance.

### 5.1 Topological readiness

A run holds, per node, a settled-state and, per edge, a settled control signal. Scheduling is event-driven, not a precomputed order:

1. The `trigger` node settles first, with the trigger document on its `out` port.
2. A node becomes **ready** when **every inbound edge has settled** (to `active`, `skipped`, or `errored`).
3. A ready node is dispatched as a `tokio` task (bounded by the per-run semaphore, §6.5).
4. When a node settles, its outbound edges' control signals are computed (§5.2) and any newly-ready downstream node is dispatched.
5. The run finishes when no node is ready or running.

Because the graph is a validated DAG (§3.1.2), this terminates and every reachable node settles exactly once.

### 5.2 Edge control signals

When a node settles, each of its **output ports** is resolved to one signal, which all edges leaving that port carry:

| Node settled as | `out` / case ports                              | `err` port |
| --------------- | ----------------------------------------------- | ---------- |
| `ok`            | `active`                                        | `skipped`  |
| `ok` (`branch`) | matched case → `active`; all others → `skipped` | n/a        |
| `errored`       | `skipped`                                       | `active` (carries the error document) |
| `skipped`       | `skipped`                                       | `skipped`  |

This is the whole error/branch propagation model: a not-taken branch and a failed `out` port both emit `skipped`; a failed `err` port emits `active`.

### 5.3 Join semantics

A node with multiple inbound edges is a **join**. Its `joinPolicy` config field decides when it is ready and how it settles:

- **`all`** (default) — ready when *all* inbound edges have settled. The node *runs* if ≥ 1 inbound edge is `active`; if *every* inbound edge is `skipped`, the node settles as `skipped` without running (its whole upstream was pruned). If any inbound edge is `errored` *and the node is not on that edge's `err` path*, the node settles `skipped` with reason `upstream_error`.
- **`any`** — ready as soon as the *first* inbound edge settles `active`; remaining edges are ignored. Useful for "whichever recovery path finished first".

Because a `skipped` signal *satisfies* an edge, a join after a `branch` never deadlocks waiting on the pruned side — it sees `skipped` and proceeds. This is the single rule that makes branch + merge composable.

### 5.4 How v2 supersedes the v1.1 linear executor

The v1.1 executor (`flow/engine.rs` `run_one`, the `for action in flow.actions` loop) is **deleted**. There is one engine. A legacy linear flow is loaded through the projection shim (§9) into a degenerate **path graph** — `trigger → action → action → …`, each node joined `all`, no branches — and the v2 scheduler runs it. A path graph under the topological scheduler is observably identical to the old serial loop (one node ready at a time, abort-on-first-error via unwired `err` ports). The v1.1 `engine_tests.rs` serial-execution cases are ported as path-graph assertions.

## 6. Run model

### 6.1 Run lifecycle

Unchanged at the flow level from v1.1: per-flow serial with **drop-on-busy** (a trigger that arrives while the flow has a live run is dropped with a counter), cross-flow parallelism bounded by the global engine semaphore. A run gets one `bot_flow_run` row.

### 6.2 The error document

When an `action`/`transform`/`parallel`/`subflow` node errors, its `err` port emits:

```json
{ "node": "fetch_user", "kind": "action", "code": "ts6_error",
  "message": "client not on server", "at": "2026-05-20T14:02:11Z" }
```

A downstream node wired from that `err` port receives this as its input data — it is the catch handler.

### 6.3 Run terminal status

| Status            | Meaning |
| ----------------- | ------- |
| `ok`              | Every reachable node settled; no *unhandled* error. A run with pruned branches or *handled* errors is still `ok`. |
| `errored`         | ≥ 1 node settled `errored` with its `err` port **unwired** — the failure had nowhere to go. |
| `interrupted`     | Manager restart or forced delete killed a live/parked run. |
| `skipped_disabled`| The trigger fired while the flow was disabled (audit row, no execution). |

`skipped` is a *node* status, never a *run* status. The status enum is otherwise the v1.1 set (`http-api.md` §2).

### 6.4 Per-node run records

`bot_flow_run` gains a `nodeResults` array — one entry per node that settled:

```json
{ "nodeId": "welcome_msg", "kind": "action", "status": "ok",
  "startedAt": "…", "finishedAt": "…", "durationMs": 318,
  "error": null, "output": { "…": "…" } }
```

- `status` ∈ `ok | errored | skipped | interrupted`.
- `output` is the node's `out` document, **capped at 8 kB**; over-cap stores `{ "_truncated": true }` and sets a flag. This bounds the run row (≤ 64 nodes ⇒ ≤ ~512 kB worst case) while keeping the canvas run-overlay (see [`ui-brief.md`](./ui-brief.md)) data-driven.
- The v1.1 `actionResults` column is **retained** for legacy runs (historical rows already written by the v1.1 engine); v2 runs populate `nodeResults`. Both coexist; see §8 and [`http-api.md`](./http-api.md) §5.

### 6.5 Concurrency

- **Per-flow:** single-slot, drop-on-busy — unchanged from v1.1.
- **Per-run node semaphore:** bounds concurrent node tasks *within one run* (default 8). This caps the blast radius of a wide static fan-out.
- **`parallel` element concurrency:** the node's own `maxConcurrency` (§4.4), composed under the per-run semaphore.
- **Cross-flow:** the existing global engine semaphore (`max(4, num_cpus)`) is unchanged.

### 6.6 Failure & restart matrix

| Scenario                                   | Behaviour |
| ------------------------------------------ | --------- |
| Node errors, `err` port wired              | Recovery path runs; run can still finish `ok`. |
| Node errors, `err` port unwired            | Node settles `errored`; downstream `out` consumers skip; run terminal `errored`. |
| `branch` prunes a subgraph                 | Not-taken nodes settle `skipped`; joins see `skipped` and proceed; run can be `ok`. |
| Join with all inbound edges `skipped`      | Join node settles `skipped` without running. |
| Manager restart with a live or parked run  | Run row rewritten `interrupted` on boot; no resume. |
| Sub-flow nesting exceeds depth 5           | The offending `subflow`/`parallel` node errors (`code: "subflow_depth"`). |
| Engine task panic at a node                | Caught at the per-node task boundary; node settles `errored`; engine survives. |

### 6.7 Observability

Extends the v1.1 metric set ([`../architecture.md`](../architecture.md) §6.5):

- `ts6_flow_runs_total{flowId,status}`, `ts6_flow_runs_dropped_total{flowId,reason}` — unchanged.
- `ts6_flow_node_runs_total{flowId,nodeKind,status}` — **new** counter.
- `ts6_flow_run_duration_seconds` — unchanged histogram.
- `ts6_flow_engine_active_runs`, `ts6_flow_engine_active_nodes` — gauges (the second is new).

Structured log lines on run start/finish (as v1.1) plus one per node settle: `flow.id`, `run.id`, `node.id`, `node.kind`, `status`, `duration_ms`, `error?`.

## 7. Data flow

v1.1 passed data exactly one way: single-pass `${trigger.key}` string substitution into action args, against a fixed per-trigger key whitelist. v2 generalises this.

### 7.1 The blackboard

A run carries a **blackboard** — a JSON document that grows as nodes settle:

```json
{ "trigger": { …trigger event… },
  "nodes": { "fetch_user": { …its out document… },
             "build_payload": { … } },
  "input": { …the current node's inbound data… } }
```

- `trigger` — the trigger event, written once.
- `nodes.<nodeId>` — each settled node's `out` document, addressable by id.
- `input` — convenience binding for the current node's single-inbound-edge data; for a join node with multiple inbound edges, `input` is undefined and the node must reference `nodes.<id>` explicitly.

### 7.2 Expression dialect

Every config field that takes a value can instead take an **expression**: a string `{{ … }}` for interpolation, or a bare boolean expression in a `branch` `when`. Expressions read the blackboard:

- `{{ trigger.channelId }}` — interpolation (replaces v1.1 `${trigger.channelId}`).
- `trigger.channelId == 1 and nodes.fetch_user.tier == "vip"` — a `branch` condition.
- `transform.output` field expressions — object construction.

The dialect is **deliberately small** — accessors, comparisons, boolean logic, a handful of string/number helpers. No user-defined functions, no loops (iteration is the `parallel` node). The concrete library is an **implementation-time pick, not gating** — recommendation: `minijinja` for `{{ }}` interpolation and a sandboxed `evalexpr`/`jmespath`-class crate for boolean conditions. This mirrors how v1.1 left the cron-crate choice to impl time.

### 7.3 Port typing — structural and advisory

Each port declares a **shape hint**: `object | array | string | number | boolean | any`. The canvas (see [`ui-brief.md`](./ui-brief.md)) **warns** on a hint mismatch when the operator draws an edge, but does **not** block it. The engine is dynamically typed and does the real check at run time.

This is a deliberate honesty call: a visual builder that advertised a strong static type system over arbitrary TS6 / webhook JSON payloads would be lying — the payloads are not statically known. Advisory hints catch the common mistake (wiring an array into a scalar-expecting port) without promising a guarantee the engine cannot keep. The `transform` node is the explicit place to *make* a document the right shape.

## 8. Persistence

**Zero schema migration.** Both new fields ride existing opaque-string columns:

- `bot_flow.flowData` — currently a JSON string of the v1.1 `FlowDefinition`. v2 stores a **versioned envelope** `{ "version": 2, "graph": { nodes, edges } }`. A row whose `flowData` has no `version` key is, by definition, a legacy v1.1 flow. The repo deserializer tries the v2 envelope first and falls back to the bare v1.1 `FlowDefinition` (§9). No `ALTER`, no new column.
- `bot_flow_run` — `nodeResults` is added to the run document; legacy runs keep `actionResults`. The run row already serialises its result payload as a JSON string, so this is additive.

Repos (`bot_flows.rs`, `bot_flow_runs.rs`), table names, sequence ids, and the per-flow 200-run cap + 30-day TTL janitor are **unchanged**. The full wire shape is in [`http-api.md`](./http-api.md) §2 / §5.

## 9. v1.1 → v2 compatibility — coexist with lazy upgrade

A v1.1 linear flow **is** a degenerate path graph. v2 does **not** force a bulk migration:

1. **Coexistence via projection.** On load, a legacy `flowData` (no `version` key) is parsed as a v1.1 `FlowDefinition` and **projected** into a `FlowGraph`: `trigger` node → one `action` node per list entry, chained `out → in`, all joins `all`, no branches. Legacy `${…}` arg templating is preserved by the shim for projected nodes. The v2 engine then runs it. Legacy flows keep working untouched, forever, with no operator action.
2. **Opt-in conversion.** The UI offers an explicit per-flow **"Convert to graph"** action (`POST /api/flows/{id}/convert`) that writes the projected graph back as a v2 envelope. One-way, operator-initiated, never automatic — the operator chooses when a flow graduates to the canvas.
3. **No automatic rewrite.** Untouched legacy flows are never silently rewritten; the projection is computed at load time and is cheap.

This is the lowest-risk path: no migration script to get wrong, no big-bang rewrite of live rows, and exactly one execution engine to maintain.

## 10. Risks resolved by this design

| Risk | Resolution |
| ---- | ---------- |
| A graph could deadlock or run forever | DAG-only, validated by Kahn's algorithm on every write (§3.1). Iteration is the bounded `parallel` node, not loop edges. |
| Branch + merge could deadlock a join on the pruned side | `skipped` is a satisfying edge signal; `joinPolicy=all` waits for *settled*, not *active* (§5.3). |
| The run record could grow unbounded on a wide graph | ≤ 64 nodes, per-node `output` capped at 8 kB (§6.4). The 200-run cap + 30-day TTL are unchanged. |
| A migration could corrupt live v1.1 flows | No migration. Coexistence via load-time projection; conversion is opt-in (§9). |
| `parallel` could fan out to thousands of element runs | Collection capped at 256 elements; `maxConcurrency` ≤ 16; per-run node semaphore caps total in-flight (§4.4, §6.5). |
| Sub-flow recursion (a flow calling itself) | Static sub-flow-reference acyclicity check on write + runtime depth cap of 5 (§3.1.6, §4.7). |
| The canvas claims a type safety it cannot keep | Port typing is explicitly advisory; the engine validates dynamically (§7.3). |
| The engine ships silent like v0.1.0-rc1 (router never merged) | The v2 gate ([`gate.md`](./gate.md)) probes a multi-node graph end-to-end on a fresh deploy; the `check-router.sh` build guard stays. |

## 11. Trigger widening — recommendation: **hold**

The issue asks whether v2 lands the v1.1-deferred WebQuery-event and sidecar-telemetry triggers.

**Recommendation: hold them.** v2 ships the graph engine with the **same three trigger sources** as v1.1 (`cron`, `manualFire`, `ts6ClientJoined`). Rationale:

- v2 is already a large surface — engine rewrite, new wire types, the canvas builder, the compatibility shim. New *trigger plumbing* (WebQuery subscription, a sidecar telemetry channel) is **orthogonal** scope; bundling it dilutes the increment and risks the timeline.
- The graph model makes a trigger "just another node kind". Once v2 lands, widening the trigger catalogue is a clean, low-risk follow-up — no engine change, only a new `trigger` config variant and its subscriber.
- Sidecar telemetry has no surface to consume until Phase 7 media-QoS work lands; widening now would build against a moving target.

This is a **scope recommendation for the board to confirm** at the ratify gate. If the board wants a trigger widening in v2, it is a clean addition to scope and produces one extra implementation child.

## 12. Implementation children (filed only after board ratification)

Per [PURA-260](/PURA/issues/PURA-260)'s process: **no implementation children are filed until this brief is ratified.** On ratification, the expected breakdown — filed under [PURA-259](/PURA/issues/PURA-259) — is:

1. **Canvas-tech spike** (DioxusLead) — the open question in [`ui-brief.md`](./ui-brief.md) §3; 2–3 day timebox; bespoke SVG/CSS vs. a JS-interop island. Blocks child 4.
2. **Wire types + persistence** (RustPlatform) — `crates/shared` v2 types, the versioned envelope, the projection shim (§8, §9).
3. **Graph engine** (RustPlatform) — the topological scheduler, node dispatch, run records (§5, §6). Blocked by child 2.
4. **Canvas UI** (DioxusLead + UXDesigner) — per [`ui-brief.md`](./ui-brief.md). Blocked by children 1 and 2.
5. **Gate harness** (QAEngineer) — per [`gate.md`](./gate.md). Blocked by child 3.

Sequencing and exact specialty assignment are finalised when the children are filed.

## 13. References

- [PURA-259](/PURA/issues/PURA-259) — Phase 8 epic (flow-engine v2 redesign).
- [PURA-260](/PURA/issues/PURA-260) — this design brief.
- [PURA-227](/PURA/issues/PURA-227) — board greenlight (accepted `request_confirmation`, 2026-05-16).
- [`../architecture.md`](../architecture.md) — v1.1 linear-engine architecture (shipped `v1.1` tag).
- `crates/shared/src/flows.rs` — v1.1 wire types (reused by the projection shim).
- `crates/ts6-manager-server/src/flow/` — engine, routes, dispatch, trigger (v2 rewrites `engine.rs`).
- `crates/ts6-manager-server/src/repos/bot_flows.rs`, `bot_flow_runs.rs` — persistence (unchanged shape).
