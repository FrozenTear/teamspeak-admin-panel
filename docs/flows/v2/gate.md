# Flow engine v2 — gate plan (graph WS-Gate probe)

- **Status:** draft, pending board ratification — ratify gate under [PURA-259](/PURA/issues/PURA-259), authored by [PURA-260](/PURA/issues/PURA-260).
- **Companion docs:** [`architecture.md`](./architecture.md), [`http-api.md`](./http-api.md), [`ui-brief.md`](./ui-brief.md).
- **Builds on:** the v1.1 gate [`../v1.1-gate.md`](../v1.1-gate.md) and the existing `scripts/ws-gate/` tree (`v6-probe.sh`, `admin-probe.sh`, `run-all.sh`).
- **Owner (gate harness):** QAEngineer / [QA](/PURA/agents/qa).

## 1. Goal

v1.1's `v6-probe.sh` proves the *linear* flow path: create → enable → fire → run `ok`. It cannot prove the v2 engine, because a single `logLine` action exercises no topology — no branch, no join, no parallelism, no per-node record.

v2 adds **V6g — Graph flow** to the Chapter 1 verification matrix: a probe that builds a **multi-node graph with both a branch and a parallel fan-out**, fires it, and asserts the **per-node** outcome — that the taken branch ran, the pruned branch was skipped, the parallel paths both ran, and the run settled `ok`. Against a fresh **rootless Podman deploy of the v2 release image**, the same posture v1.0/v1.1 hold for the rest of the matrix.

The v1.1 `v6-probe.sh` is **kept** — a v2 manager still runs legacy linear flows ([`architecture.md`](./architecture.md) §9), so the linear path stays gated. v2 adds a probe; it does not replace one.

## 2. `v6-graph-probe.sh`

Path: `scripts/ws-gate/v6-graph-probe.sh` (new — joins `v6-probe.sh` in the existing `ws-gate/` tree).

### 2.1 Inputs

```
BASE_URL    (positional 1, required)    e.g. https://manager.scuffedcrew.no
ADMIN_TOKEN (env, required)             admin JWT for the manager
SERVER_CONFIG_ID  (env, default 1)
VIRTUAL_SERVER_ID (env, default 1)
OBS_WINDOW_S      (env, default 30)
```

Same input contract as `v6-probe.sh` — the umbrella runner passes them uniformly.

### 2.2 The probe graph

The probe builds **one graph that exercises every topology feature v2 claims** and whose outcome is fully deterministic — all leaf nodes are `logLine` actions, so the probe needs no live TS6 server, only the manager:

```
            trigger (manualFire)
                  │
              transform        set { route: "a", n: 2 }
                  │
               branch          case "a": input.route == "a"   → fires
              ╱   │   ╲                 case "b": …            → skipped
        (a)  ╱  (b)│    ╲ (default)
       logA      logB     logDefault          logB, logDefault → skipped
         │
      fan_out  (parallel over a 2-element list, sub-flow = a logLine flow)
         │
       join    (logLine, joinPolicy = all — one inbound edge, trivially ready)
```

Concretely the probe `POST`s, in order:

1. A tiny **sub-flow** — a one-node-after-trigger `logLine` graph — captured as `SUB_FLOW_ID`. The `parallel` node references it.
2. The **main graph**: `trigger(manualFire)` → `transform` (emits `{route:"a"}`) → `branch` (cases `a`,`b` + `default`) → `logA` on the `a` port, `logB` on `b`, `logDefault` on `default` → `logA` → `parallel`(`collection` = a 2-element literal, `subFlowId` = `SUB_FLOW_ID`) → `join` (`logLine`).

This graph asserts, in one fire: a `transform` produces data, a `branch` routes on it, the **taken** path runs, **both not-taken** paths are `skipped`, a `parallel` node fans out and rejoins, a `subflow` runs nested, and a join settles. That is the full v2 feature surface ([`architecture.md`](./architecture.md) §4).

### 2.3 Sequence

```text
1. Health pre-check        curl -fsS $BASE_URL/api/health           — abort non-2xx.

2. Validate (negative)     POST /api/flows/validate with a hand-built CYCLIC graph
                           assert 200, body.valid == false,
                           body.errors[].code contains "graph_cycle".
                           — proves the validator is wired, not just create-path.

3. Create sub-flow         POST /api/flows  (logLine sub-flow graph)
                           assert 201; SUB_FLOW_ID = body.id.

4. Create main graph       POST /api/flows  (the §2.2 graph, subFlowId=SUB_FLOW_ID)
                           assert 201; FLOW_ID = body.id; assert body.flowVersion == 2.

5. Enable                  PATCH /api/flows/$FLOW_ID  { enabled: true }
                           assert 200, body.enabled == true.

6. Fire                    POST /api/flows/$FLOW_ID/fire
                           assert 202; RUN_ID = body.runId.

7. Observe (deadline = now + OBS_WINDOW_S, poll 500 ms)
   GET /api/flows/$FLOW_ID/runs/$RUN_ID
     when body.status is terminal:
       assert body.status == "ok"
       index body.nodeResults by nodeId:
         assert nr["transform"].status == "ok"
         assert nr["branch"].status    == "ok"
         assert nr["logA"].status      == "ok"
         assert nr["logB"].status      == "skipped"        — not-taken branch
         assert nr["logDefault"].status== "skipped"        — not-taken branch
         assert nr["fan_out"].status   == "ok"
         assert nr["join"].status      == "ok"
       break
     on timeout: fail.

8. Cleanup                 DELETE /api/flows/$FLOW_ID?force=true   assert 204
                           DELETE /api/flows/$SUB_FLOW_ID?force=true assert 204

9. Exit 0 with a green log line.
```

Every step writes request/response bodies to `qa-evidence/ws-gate/v6-graph/<ISO8601-UTC>/step-N.{req,resp}.json` — same evidence convention as `v6-probe.sh`, so the matrix entry attaches them like the rest.

### 2.4 Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | All assertions passed inside the observation window. |
| 64   | Usage error (missing `BASE_URL`/`ADMIN_TOKEN`). |
| 65   | Health pre-check failed. |
| 66   | `POST /validate` missing or did not reject the cyclic graph (validator not wired). |
| 67   | Sub-flow or main-graph create failed. |
| 68   | Enable failed. |
| 69   | Fire failed. |
| 70   | Observation window expired without a terminal run status. |
| 71   | Run reached a terminal status but it was not `ok`. |
| 72   | **Per-node assertion failed** — a branch/parallel/skip outcome was wrong (the v2-specific regression code). |
| 73   | Cleanup failed (probe still counts green; emits a warning). |

Code **72** is the one that matters most: it distinguishes "the graph engine ran but got the *topology* wrong" (a real v2 regression — branch took the wrong path, a not-taken node ran, a join settled early) from a plain wire/engine outage (66–71).

## 3. Matrix entry

`docs/phase6/readiness-audit.md` §1 currently carries a **V6** row (the v1.1 linear flow). v2 adds a sibling row:

1. Add **V6g — Graph flow** below V6.
2. `Status` cell = the `v6-graph-probe.sh` result (`pass`/`fail`).
3. Rationale cell: *"Graph/node flow engine shipped per [PURA-259](/PURA/issues/PURA-259). Probe: `scripts/ws-gate/v6-graph-probe.sh` against the v2 image — branch + parallel + per-node assertions."*
4. Update the **Summary read** paragraph row count (`seven` → `eight`).

V6 (linear) stays — the v2 engine still runs legacy linear flows, so both rows are live and both probes run.

## 4. Umbrella runner integration

`scripts/ws-gate/run-all.sh` already fans out to per-verification probes and aggregates pass/fail. v2:

- Register `v6-graph-probe.sh` as an additional row alongside `v6-probe.sh`.
- `run-all.sh` runs **both** the linear probe and the graph probe; the matrix dump shows V6 and V6g independently.
- A v2 image must pass **both**: linear regression (legacy flows still work) and graph (the new engine is correct).

## 5. CI / build-image integration

- The `RUN scripts/check-router.sh flows_router` build guard in `Containerfile.fullstack` is **kept** — it still prevents the "router never merged" silent-404 regression class. v2 adds nothing here; the same router carries the new endpoints.
- Optionally extend `check-router.sh` to assert the three v2 routes (`/api/flows/validate`, `/api/flows/{id}/runs/{runId}`, `/api/flows/{id}/convert`) are mounted — a cheap guard against a partially-merged v2 router. The gate-harness child decides whether this is worth the grep.

## 6. Acceptance for the gate-harness implementation child

- `scripts/ws-gate/v6-graph-probe.sh` is committed, executable, `shellcheck`-clean.
- Run against a locally-deployed v2 image: exits 0, writes the evidence files, and the per-node assertions in step 7 all hold.
- Run against a **v1.1** image (no `/validate`, no graph support): fails fast — exit 66 (validator absent) or 67 (graph create rejected) — proving the probe actually distinguishes engine versions.
- `run-all.sh` runs both `v6-probe.sh` and `v6-graph-probe.sh`; the aggregated matrix dump shows V6 and V6g.
- `docs/phase6/readiness-audit.md` §1 has the V6g row and the updated summary count.

## 7. References

- [PURA-259](/PURA/issues/PURA-259) — Phase 8 epic.
- [PURA-260](/PURA/issues/PURA-260) — this design brief.
- [`architecture.md`](./architecture.md) — graph model & engine (the feature surface this probe exercises).
- [`http-api.md`](./http-api.md) — `POST /validate`, `GET /runs/{runId}` (the endpoints the probe drives).
- [`../v1.1-gate.md`](../v1.1-gate.md) — v1.1 gate plan (the `v6-probe.sh` this builds beside).
- `scripts/ws-gate/v6-probe.sh`, `scripts/ws-gate/run-all.sh` — existing harness.
- `docs/phase6/readiness-audit.md` — Chapter 1 verification matrix.
