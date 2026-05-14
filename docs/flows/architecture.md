# Flow engine — v1.1 architecture brief

- **Status:** draft, pending board ratification ([PURA-198](/PURA/issues/PURA-198)).
- **Spec refs:** §1.5 V6, Chapter 31.
- **Parent epic:** [PURA-155](/PURA/issues/PURA-155) (Phase 6) → v1.1 scope.
- **Predecessor deviation:** [`docs/deviations/v6-flow-engine-cut.md`](../deviations/v6-flow-engine-cut.md) — to be reclassified `resolved` when the v1.1 implementation children land and V6 is back on the gate.

## 1. Purpose

The flow engine lets an operator wire **a trigger → a small ordered list of actions** against a TeamSpeak virtual server managed by the ts6-manager. v1.1 ships the smallest engine that proves the spec — design, persistence, HTTP surface, UI, and a green gate row — without committing the operator-facing wire to anything we cannot maintain.

This document is the **architecture brief**. HTTP wire, UI brief, and gate plan live in sibling files.

## 2. v1.1 scope vs deferred

### 2.1 In scope (v1.1)

- **Triggers** — three sources only (see §3).
- **Actions** — four kinds (see §4).
- **Persistence** — extend `bot_flows.rs`, add `bot_flow_runs`. Survives manager restart.
- **HTTP surface** — `POST/GET/PATCH/DELETE /api/flows`, `GET /api/flows/{id}/runs`. Spec'd in [`http-api.md`](./http-api.md).
- **UI** — list, create form, enable toggle, run history pane. Spec'd in [`ui-brief.md`](./ui-brief.md).
- **Gate** — `scripts/ws-gate/v6-probe.sh` reinstated. Spec'd in [`v1.1-gate.md`](./v1.1-gate.md).
- **Deployment shape** — **single-manager** only. Multi-manager flow-state coordination is out of scope.

### 2.2 Out of scope (defer to v1.2+)

- WebQuery → manager event triggers (the existing WS hub already covers the only TS6 event we need; WebQuery surfaces are a v1.2 widening).
- Sidecar telemetry → bot triggers (no telemetry surface lands until media QoS work in Phase 7).
- Complex flow control — branches, retries, schedules with timezones, parallel actions. v1.1 is **strict serial, one-shot**.
- Multi-manager state visibility, leader election, distributed run logs.
- Per-server-group permission grain. v1.1 is admin-only writes (see §8).
- A user-facing "flow library" / marketplace. v1.1 only renders the operator's own flows.

The minimal cut is deliberate: we want a flow to be **easy to reason about**, easy to gate, and easy to extend. v1.2 widens after we know what operators actually wire.

## 3. Trigger taxonomy (v1.1)

A flow has exactly **one trigger**. The trigger document is stored as JSON in `bot_flow.flowData.trigger`. The engine's trigger module is the only place that knows how to subscribe / poll for each source.

| Discriminant       | Source                                                                                  | Fires when                                                                                                | Idempotency key                                       | v1.1 notes |
| ------------------ | --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- | ---------- |
| `cron`             | Tokio interval driven from a parsed cron expression (single-process, UTC).              | The next tick after the manager registers the schedule.                                                   | `(flowId, tickInstant)`                              | Bounded to 1-minute minimum granularity. Catch-up is **skipped** — if a tick was missed (manager off), we do not replay. |
| `manualFire`      | Operator-driven via `POST /api/flows/{id}/fire`.                                        | An admin posts the fire endpoint.                                                                         | `(flowId, requestId)` where `requestId` is server-minted | Always allowed even if `enabled = false` — manual fire is the test/debug path. |
| `ts6ClientJoined` | Subscribes to the existing WS hub topic `ts:client:connected` (see `ws/server_notify.rs`). | A `notifycliententerview` from the upstream TS6 server flows through the manager's notify pipeline.       | `(flowId, virtualServerId, clientUniqueIdentifier, ts)` | Filter narrows by `virtualServerId` (already on the flow row) and optionally by `channelId` in the trigger config. |

**Why these three.** Cron + manual cover the "scheduled chore" and "run-it-now" wedge. `ts6ClientJoined` is the single most-asked-for community-server flow (welcome-message, auto-group-assign) and reuses an event stream that already exists in `v0.1.0-rc1`. No new TS6 plumbing required.

**Trigger-document shape (JSON, persisted in `bot_flow.flowData`):**

```json
{ "kind": "cron", "expression": "0 */5 * * * *" }
{ "kind": "manualFire" }
{ "kind": "ts6ClientJoined", "channelId": null }
```

The wire-format types live in `crates/shared/src/flows.rs` (new). See [`http-api.md`](./http-api.md) §2.

## 4. Action taxonomy (v1.1)

A flow has an **ordered list of 1–8 actions**. Actions execute strictly serially. The first action that errors stops the run; subsequent actions in the list are marked `skipped`.

| Discriminant         | What it does                                                                                                                                                | Required scope                                                                                  | Idempotency notes                                                                            |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `ts6Command`         | Issues one mutating ServerQuery / TS6 command (e.g. `clientmove`, `sendtextmessage`, `servergroupaddclient`) against the flow's `virtualServerId`.          | Reuse `control_router` ClientCommand pipeline; admin token only — no operator session reuse.    | Idempotency is the **TS6 server's** problem (clientmove is naturally idempotent; sendtextmessage is not). v1.1 does not retry. |
| `musicBotCommand`    | Posts to the in-process music-bot API (`POST /music-bots/{botId}/...` internal call) — e.g. `enqueue`, `pause`, `join-channel`.                             | Bot must exist on the flow's `virtualServerId`; admin token.                                    | Same one-shot semantics — music-bot is responsible for queue de-duplication if it cares.     |
| `webhookOut`         | HTTP POST to a configured URL with a JSON body containing `{ flowId, runId, trigger, context }`.                                                            | URL must pass the SSRF allow-list (`crates/ts6-manager-server/src/web/ssrf.rs`, R6 from audit). | Outbound side. Caller's problem. We send once; failure → run errors.                          |
| `logLine`            | Writes a structured log line at `info` with `flow.id`, `flow.name`, `run.id`, `action.index`, `message`.                                                    | None — always allowed.                                                                          | The free-cost debugging action; useful as the only action in a "does my trigger fire?" flow. |

**Action-document shape:**

```json
[
  { "kind": "ts6Command", "command": "sendtextmessage", "args": { "targetmode": 2, "target": "${trigger.channelId}", "msg": "Welcome." } },
  { "kind": "logLine", "message": "welcome flow fired" }
]
```

Argument-templating (the `${trigger.channelId}` example) is **single-pass string substitution** against the trigger context — no expressions, no conditionals. If a referenced key is missing we error the action and abort the run. The whitelist of substitutable keys is fixed per trigger kind and documented in `flows.rs`.

## 5. Persistence model

### 5.1 Existing surface

`crates/ts6-manager-server/src/repos/bot_flows.rs` already defines:

- Table `bot_flow` with columns `id, name, description, flowData (String), serverConfigId, virtualServerId, enabled, createdAt, updatedAt`.
- CRUD: `insert`, `find_by_id`, `list`, `list_for_server`, `update`, `delete`.
- A `BotFlowUpdate` struct supporting partial patch of `name, description, flowData, virtualServerId, enabled`.

The repo is unit-tested by `repos/tests_chapter4.rs` but is unreferenced by any HTTP router. v1.1 keeps the table shape and adds run history; we do **not** rename the table or break the existing repo tests.

### 5.2 What v1.1 adds

**Schema (SurrealDB, mirroring the `bot_flow` style):**

- Table `bot_flow_run`:
  - `id i64` (`record::id`).
  - `flowId i64` (FK to `bot_flow.id`).
  - `trigger SurrealValue` (JSON document — the resolved trigger event, including idempotency-key fields).
  - `startedAt DateTime<Utc>`.
  - `finishedAt Option<DateTime<Utc>>` — `NULL` while in-flight.
  - `status: "in_flight" | "ok" | "errored" | "interrupted" | "skipped_disabled"`.
  - `error Option<String>` (truncated to 2 kB).
  - `actionResults Vec<ActionResult>` — one entry per planned action with `index, status, durationMs, error?`.

Index: `(flowId, startedAt DESC)` for the run-history list.

**Repo file:** `crates/ts6-manager-server/src/repos/bot_flow_runs.rs` (new). Mirrors `bot_flows.rs` style — `PROJECTION` string, async `insert/find/list_for_flow/update_status` functions, `tests_chapter4.rs` row added.

### 5.3 Bounded-storage policy (resolved from the open question)

Two limits enforced together; whichever first:

1. **Per-flow row cap of 200 runs.** On `insert`, after the new row lands, delete the oldest rows where `id NOT IN (top 200 by startedAt DESC)` for that `flowId`.
2. **Global TTL of 30 days.** A periodic Tokio task (every 1 h) deletes rows with `finishedAt < now - 30 days`.

200 × N flows × ≤2 kB per row is bounded for the single-manager target. The cap is conservative for v1.1; a future config knob lives behind the API but is not exposed in v1.1.

### 5.4 Migration

`bot_flow_run` is a new SurrealDB table. We do **not** ship a one-shot migration script; the manager's `repos/init.rs`-equivalent table-creation pass already handles "create if not exists" semantics (mirror what `bot_flow` does today). No data migration is required — the table starts empty.

## 6. Engine surface

Per the issue request, the engine lives under `crates/ts6-manager-server/src/flow/`:

```
src/flow/
├── mod.rs       — re-exports + the `FlowEngine` handle stored in AppState
├── engine.rs    — run-execution loop, action dispatch, run-row writes
├── routes.rs    — axum sub-router; merged into main.rs as `flows_router`
└── trigger.rs   — trigger-source subscribers (cron, manualFire, ts6ClientJoined)
```

### 6.1 Boot

`main.rs` constructs `let flow_engine = flow::FlowEngine::start(state.clone()).await?;` before assembling the router. `FlowEngine::start`:

1. Loads all `bot_flow` rows with `enabled = true`.
2. For each, registers the appropriate trigger subscription with `trigger.rs`.
3. Spawns one Tokio task that owns the receive-channel for fired trigger events and dispatches them to per-flow run-execution tasks.
4. Spawns the TTL/cap janitor task (see §5.3).
5. Returns a `FlowEngineHandle` cloneable into `AppState` so route handlers can `enable/disable/fire` flows at runtime.

### 6.2 Run dispatch

```
trigger.rs (subscription)
    → mpsc::Sender<TriggerFire>
    → engine.rs receive loop
    → spawn task: run_one(flow, trigger_ctx)
        ├ create bot_flow_run row (status=in_flight)
        ├ for action in flow.actions { execute; update actionResults }
        ├ finalize row (status=ok|errored|skipped)
        └ emit ts6_flow_runs_total{flowId, status} metric
```

### 6.3 Concurrency / re-entrancy

- **Per-flow serial.** Each flow has a single-slot run mailbox; while a run is in-flight, additional triggers for that flow are **dropped** and a `dropped` log line emitted with a counter `ts6_flow_runs_dropped_total{flowId, reason="busy"}`. Why drop rather than queue: a queue invites unbounded backpressure for noisy triggers (cron 1m + slow webhook). Operators can re-fire manually if they care.
- **Cross-flow parallel.** Different flows run in their own tasks. No global lock.
- **Manager-level cap.** A global `Semaphore` with `permits = max(4, num_cpus)` bounds concurrent flow-run tasks across the whole engine. Excess waits, does not drop (cross-flow contention is expected to be low).

### 6.4 Failure model

- **An action errors** (HTTP non-2xx, TS6 error code, panic caught by `catch_unwind`-style wrapper) → that action's `actionResults[i].status = "errored"`, `error = "{kind}: {msg}"`. Remaining actions marked `skipped`. Run row's `status = "errored"`.
- **The flow stays enabled.** v1.1 does not auto-disable on N consecutive failures (defer to v1.2). The UI surfaces the most-recent run status so operators can react.
- **Engine panic** is caught at the per-run task boundary; the engine task itself does not die. Stack is logged with `flow.id` / `run.id` context.
- **In-flight runs on manager restart.** Any `bot_flow_run` with `status = "in_flight"` at boot is rewritten to `status = "interrupted"` with `error = "manager restart"` and `finishedAt = now`. We do **not** auto-resume. This keeps the persistence model honest: the run row matches reality.
- **Trigger backlog on restart** is intentionally lost. Cron does not catch up; TS6 events are only handled while the engine is live; manual fires that happened during downtime never reached us. This is documented operator-facing in the UI brief.

### 6.5 Observability

Add to the existing `routes/metrics.rs`:

- `ts6_flow_runs_total{flowId, status}` (counter).
- `ts6_flow_runs_dropped_total{flowId, reason}` (counter).
- `ts6_flow_run_duration_seconds_bucket{...}` (histogram, 5 buckets: 0.01/0.1/1/10/60s).
- `ts6_flow_engine_active_runs` (gauge — current in-flight runs across all flows).

Structured log line on every run start and finish, fields: `flow.id`, `flow.name`, `run.id`, `trigger.kind`, `status`, `duration_ms`, `error?`.

## 7. Concurrency / restart matrix (summary)

| Scenario                                      | Behaviour                                                                                    |
| --------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Trigger fires while flow disabled             | Run row written with `status = "skipped_disabled"`, actions skipped. Audit trail preserved.  |
| Trigger fires while same flow is running      | Dropped, counter incremented, log line emitted. No queue.                                    |
| Action errors mid-run                         | Run marked `errored`, remaining actions `skipped`, flow stays enabled.                        |
| Manager restart with in-flight run            | Run rewritten to `interrupted` on boot. No resume.                                            |
| Manager restart                               | Trigger backlog is lost. Cron does not replay. TS6 events from offline period are not seen.  |
| Operator deletes flow with in-flight run      | Delete is **rejected with 409** unless `?force=true`. Force-delete marks the run interrupted then deletes the flow row. |

## 8. Permission model

- **Writes** (`POST`, `PATCH`, `DELETE`, `POST /fire`) — `RequireAdmin` extractor. v1.1 is admin-only by design; operators delegating flow authoring to moderators is a v1.2 feature gated on per-server-group ACL work.
- **Reads** (`GET /api/flows`, `GET /api/flows/{id}`, `GET /api/flows/{id}/runs`) — `RequireAuth` extractor. Any authenticated user of the manager can read flow definitions and run history. Rationale: in v1.1 the manager is single-tenant per deployment; broader read-out helps community moderators see what's wired without granting them edit rights.

All endpoints emit the shared `ts6_manager_shared::flows::ErrorBody` envelope (new — mirrors `music_bots::ErrorBody`).

## 9. Risks resolved by this design

| Risk                                                                                                  | Resolution                                                                                                                                                                                              |
| ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Multi-manager flow-state visibility                                                                   | **Single-manager only** for v1.1. Documented as a deployment constraint in `ui-brief.md` and the gate plan. v1.2 revisits if the wedge needs HA.                                                        |
| Unbounded `bot_flow_runs` table                                                                       | Per-flow cap of 200 + global TTL of 30d. Janitor task. §5.3.                                                                                                                                            |
| Permission grain                                                                                      | Admin writes, authenticated reads. §8.                                                                                                                                                                  |
| `webhookOut` as SSRF vector                                                                           | Reuses the existing SSRF allow-list pipeline ([PURA-150](/PURA/issues/PURA-150) class). Same proxy / IP-pin as outbound music-bot URL fetch.                                                            |
| Operator wires an infinite-loop flow (action triggers the same trigger)                               | Per-flow serial + drop-on-busy already kills the most obvious self-trigger. We do **not** add a generic cycle-detector in v1.1; documented as an operator footgun in `ui-brief.md`.                     |
| Engine code path is silent like the v0.1.0-rc1 one was (router never merged → 404 surface)            | The gate plan ([`v1.1-gate.md`](./v1.1-gate.md)) probes the wire end-to-end; the matrix re-adds V6; CI fails on missing route. The deviation note is reclassified only when the probe passes. |

## 10. Open questions that **do not** need to be resolved at design time

- Exact cron-expression dialect — pick one library at implementation time (recommendation: `cron` crate, 6-field UTC). Documented but not gating.
- Exact `ActionResult` JSON shape — settled in `crates/shared/src/flows.rs` during impl. Documented but not gating.
- Whether `manualFire` should be allowed against a deleted flow id (no — `404`).

## 11. Implementation children (to file after board ratification & v1.0 tag green)

Filed against [PURA-155](/PURA/issues/PURA-155):

1. **Server engine + routes** (RustPlatform). Scope: §6 (engine surface), §3 (triggers), §4 (actions), §5 (persistence), §8 (auth). Acceptance: `GET /api/flows` returns 200, `POST` creates a row, `POST /fire` produces a `bot_flow_run` row visible on `GET /api/flows/{id}/runs`. Unit tests in `flow/engine_tests.rs`. Inherits workspace from this issue.
2. **UI** (DioxusLead + UXDesigner). Scope: list / create form / enable toggle / run history pane per [`ui-brief.md`](./ui-brief.md). Acceptance: navigating to `/flows` on a fresh manager shows the empty-state, creating a `logLine` flow and clicking "Fire" produces a run row visible in the history pane.
3. **Gate harness** (QAEngineer). Scope: `scripts/ws-gate/v6-probe.sh` per [`v1.1-gate.md`](./v1.1-gate.md). Acceptance: probe exits zero against `v0.1.x` release image; matrix re-rendered with the V6 row marked `pass`.
4. **Deviation reclassification** (CTO). Scope: edit `docs/deviations/v6-flow-engine-cut.md` to status `resolved`, add the link to the v1.1 release notes. Files-only.

Children are filed serially: (1) blocks (2) and (3); (4) blocked by (3). Implementation start is gated on **v1.0 tag green** (`PURA-164`) so RustPlatform/DioxusLead/QA aren't pulled off the release gate.

## 12. References

- [PURA-198](/PURA/issues/PURA-198) — this design issue.
- [PURA-195](/PURA/issues/PURA-195) — v1.0 V6 cut decision.
- [PURA-189](/PURA/issues/PURA-189) — WS-Gate B3 (V6 gap), closed-superseded.
- [PURA-155](/PURA/issues/PURA-155) — Phase 6 epic (parent).
- [PURA-164](/PURA/issues/PURA-164) — v1.0 release-gate (must tag green before children land).
- `crates/ts6-manager-server/src/repos/bot_flows.rs` — existing repo.
- `crates/ts6-manager-server/src/ws/server_notify.rs` — `ts:client:connected` event source.
- `docs/phase6/readiness-audit.md` — Chapter 1 verification context.
- `docs/deviations/v6-flow-engine-cut.md` — deviation record (reclassified `resolved` when impl lands).
