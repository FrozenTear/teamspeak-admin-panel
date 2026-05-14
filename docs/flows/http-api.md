# Flow engine — v1.1 HTTP API spec

- **Status:** draft, pending board ratification ([PURA-198](/PURA/issues/PURA-198)).
- **Companion docs:** [`architecture.md`](./architecture.md), [`ui-brief.md`](./ui-brief.md), [`v1.1-gate.md`](./v1.1-gate.md).
- **Wire types:** new `crates/shared/src/flows.rs`, re-exported via `crates/shared/src/lib.rs` as `ts6_manager_shared::flows`.

All wire types are `#[serde(rename_all = "camelCase")]` (mirrors `music_bots.rs` convention). Server source uses snake_case; the wire is `botFlowId` / `createdAt`. All non-2xx responses serialise the shared `flows::ErrorBody` envelope:

```json
{ "error": "validation", "message": "trigger.kind: unknown variant `webhookIn`" }
```

The router is mounted at absolute paths (mirroring `music_bots_router`); composition in `main.rs`:

```rust
let flows_router = flow::routes::router().with_state(state.clone());
// …
.merge(flows_router)
```

## 1. Endpoints

| Method | Path                                | Auth          | Purpose                                                  |
| ------ | ----------------------------------- | ------------- | -------------------------------------------------------- |
| `GET`  | `/api/flows`                        | `RequireAuth` | List flows. Optional `?virtualServerId=` filter.         |
| `POST` | `/api/flows`                        | `RequireAdmin`| Create a flow.                                           |
| `GET`  | `/api/flows/{id}`                   | `RequireAuth` | Fetch one flow.                                          |
| `PATCH`| `/api/flows/{id}`                   | `RequireAdmin`| Partial update (any subset of mutable fields).           |
| `DELETE`| `/api/flows/{id}`                  | `RequireAdmin`| Delete. `?force=true` interrupts in-flight runs first.   |
| `POST` | `/api/flows/{id}/fire`              | `RequireAdmin`| Manual fire. Returns the created `runId`. **Always allowed**, even if `enabled = false`. |
| `GET`  | `/api/flows/{id}/runs`              | `RequireAuth` | Run history. Optional `?limit=`, `?cursor=` (run id).    |

The `RequireSetupAuth` extractor is **not** in scope — flow engine sits above setup.

Rate-limiting reuses the global per-IP bucket (no dedicated flow bucket in v1.1). `POST /fire` adds a per-flow soft cap of 1 fire / 2 s; excess returns `429`.

## 2. Wire types (`crates/shared/src/flows.rs`)

```rust
//! Wire-format types for the flow engine ([PURA-198](/PURA/issues/PURA-198)).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FlowId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FlowRunId(pub i64);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Trigger {
    Cron { expression: String },
    ManualFire,
    Ts6ClientJoined { channel_id: Option<i64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Action {
    Ts6Command {
        command: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
    },
    MusicBotCommand {
        bot_id: u64,
        command: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
    },
    WebhookOut {
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    LogLine { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowDefinition {
    pub trigger: Trigger,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flow {
    pub id: FlowId,
    pub name: String,
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub enabled: bool,
    pub definition: FlowDefinition,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<FlowRunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRunSummary {
    pub id: FlowRunId,
    pub status: FlowRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRun {
    #[serde(flatten)]
    pub summary: FlowRunSummary,
    pub flow_id: FlowId,
    pub trigger: serde_json::Value,        // resolved trigger event document
    pub error: Option<String>,
    pub action_results: Vec<ActionResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowRunStatus {
    InFlight,
    Ok,
    Errored,
    Interrupted,
    SkippedDisabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub index: u32,
    pub kind: String,           // e.g. "ts6Command"
    pub status: ActionStatus,
    pub duration_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus { Ok, Errored, Skipped }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFlowRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    #[serde(default)]
    pub enabled: bool,
    pub definition: FlowDefinition,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFlowRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default)]
    pub virtual_server_id: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub definition: Option<FlowDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FireFlowResponse {
    pub run_id: FlowRunId,
    pub flow_id: FlowId,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: String,          // discriminant, e.g. "validation"
    pub message: String,
}
```

`double_option` (helper) — same trick as elsewhere in `shared` — distinguishes "field absent" from "field present and null" so a PATCH can clear `description`.

## 3. Endpoint detail

### 3.1 `POST /api/flows`

Request:

```json
{
  "name": "welcome-on-join",
  "description": "send a welcome text when a new client joins channel 5",
  "serverConfigId": 1,
  "virtualServerId": 1,
  "enabled": true,
  "definition": {
    "trigger": { "kind": "ts6ClientJoined", "channelId": 5 },
    "actions": [
      { "kind": "ts6Command", "command": "sendtextmessage",
        "args": { "targetmode": 2, "target": "${trigger.channelId}",
                  "msg": "Welcome ${trigger.clientNickname}." } }
    ]
  }
}
```

Validation:

- `name` non-empty, ≤120 chars, unique per `(serverConfigId, virtualServerId)`.
- `definition.actions` non-empty, length ≤ 8.
- Each `action.command` is one of the engine's whitelisted commands (whitelist in `flow::engine::commands::mod.rs`).
- `Trigger::Cron.expression` parses via the chosen cron crate.

Responses:

- `201 Created` → `Flow` (with `lastRun = null`).
- `400 Bad Request` → `ErrorBody{ error: "validation", message }`.
- `409 Conflict` → `ErrorBody{ error: "name_taken" }`.
- `403 Forbidden` → not admin.

### 3.2 `PATCH /api/flows/{id}`

Body is `UpdateFlowRequest` — any subset of fields. Setting `enabled: true` on a flow registers its trigger with the engine; `enabled: false` deregisters and lets in-flight runs finish. Replacing `definition` is allowed only when `enabled = false` (the API rejects a definition swap on a live flow with `409`).

Responses: `200 OK → Flow`, `400`, `404`, `409`, `403`.

### 3.3 `DELETE /api/flows/{id}[?force=true]`

- Default: rejects with `409 Conflict` if a run is in-flight. Error body: `{ "error": "run_in_flight", "message": "run {id} is in-flight; pass ?force=true to interrupt" }`.
- `?force=true`: marks all in-flight runs `interrupted`, then deletes the flow row. Returns `204 No Content`.
- Run rows for the flow are deleted along with it (FK-style cascade implemented at repo level; SurrealDB schemaless table → an explicit `DELETE bot_flow_run WHERE flowId = $id` precedes the flow delete).

### 3.4 `POST /api/flows/{id}/fire`

Body: optional `{ "context": { ... } }` — operator-provided JSON merged into the run's `trigger` document under the `manualFire` discriminant. v1.1 does not substitute it into action args.

Responses:

- `202 Accepted` → `FireFlowResponse{ runId, flowId, startedAt }`. Run is in-flight; poll `GET /api/flows/{id}/runs` for status.
- `404 Not Found` → flow id unknown.
- `429 Too Many Requests` → per-flow fire rate-limit hit.
- `503 Service Unavailable` → engine semaphore saturated and the request waited `>5s`. Operator can retry.

`POST /fire` works whether or not the flow is `enabled` — it is the testing endpoint. Run row carries `trigger.kind = "manualFire"` regardless of the flow's configured trigger.

### 3.5 `GET /api/flows/{id}/runs`

Query: `?limit=` (default 25, max 200), `?cursor=` (last seen `runId` for keyset pagination).

Response:

```json
{
  "runs": [ /* FlowRun */ ],
  "nextCursor": 1234   // null if no more
}
```

Ordering: `startedAt DESC`, `id DESC` as tiebreaker.

### 3.6 `GET /api/flows`

Query: `?virtualServerId=` (optional), `?enabled=` (optional bool). No pagination in v1.1 — flow counts are expected to be small (≤ a few dozen per manager). Response: `{ "flows": [Flow, ...] }`. `lastRun` is populated by a single join-style read against `bot_flow_run` (`MAX(startedAt)` per `flowId`).

## 4. Error envelope catalogue

| `error` discriminant     | HTTP | Used when                                                                                       |
| ------------------------ | ---- | ----------------------------------------------------------------------------------------------- |
| `validation`             | 400  | Body parse failed; field shape rejected.                                                        |
| `name_taken`             | 409  | A flow with the same `(name, serverConfigId, virtualServerId)` exists.                          |
| `run_in_flight`          | 409  | Delete attempted while a run is in-flight without `?force=true`.                                |
| `definition_swap_locked` | 409  | PATCH tried to replace `definition` on an enabled flow.                                         |
| `not_found`              | 404  | Flow id unknown.                                                                                |
| `forbidden`              | 403  | Auth ok but not admin (write routes).                                                           |
| `unauthorised`           | 401  | No / invalid JWT.                                                                               |
| `rate_limited`           | 429  | Per-flow `POST /fire` cap hit.                                                                  |
| `engine_saturated`       | 503  | Global engine semaphore wait timed out.                                                         |
| `internal`               | 500  | Unhandled — should be impossible. Body intentionally vague.                                     |

The discriminant is stable for clients to branch on; `message` is human-readable and may shift between versions.

## 5. OpenAPI / client-gen

Not in v1.1 — wire types in `shared::flows` are the contract. The Dioxus client imports the same types. A `paperclip-api-spec` pass can be filed in v1.2 when a third party needs to call the API.

## 6. Test surface

The implementation child opens `crates/ts6-manager-server/src/flow/routes_tests.rs` covering:

1. `POST` round-trips: 201, 400 (bad cron), 409 (duplicate name).
2. `PATCH` definition swap blocked while `enabled = true`.
3. `DELETE` blocked when run in-flight; `?force=true` succeeds.
4. `POST /fire` produces a row visible on `GET /runs` within 1 s for a `logLine`-only flow.
5. `RequireAdmin` enforcement on every write route.
6. `ErrorBody` envelope on every non-2xx.

## 7. References

- [PURA-198](/PURA/issues/PURA-198) — design issue.
- [`architecture.md`](./architecture.md) — engine internals.
- [`v1.1-gate.md`](./v1.1-gate.md) — gate probe sequence.
- `crates/shared/src/music_bots.rs` — wire-style reference.
- `crates/ts6-manager-server/src/routes/music_bots/mod.rs` — router style reference.
