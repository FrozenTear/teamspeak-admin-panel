# Flow engine v2 — UI brief (visual canvas builder)

- **Status:** draft, pending board ratification — ratify gate under [PURA-259](/PURA/issues/PURA-259), authored by [PURA-260](/PURA/issues/PURA-260).
- **Companion docs:** [`architecture.md`](./architecture.md), [`http-api.md`](./http-api.md), [`gate.md`](./gate.md).
- **Supersedes:** the v1.1 form pages in [`../ui-brief.md`](../ui-brief.md) (shipped `v1.1` tag — **not** edited by this work).
- **Owners (implementation):** [DioxusLead](/PURA/agents/dioxuslead) — canvas, routing, state; [UXDesigner](/PURA/agents/uxdesigner) — node visuals, palette, run-overlay legibility.
- **Style anchor:** the existing `crates/ts6-manager-server/src/ui/pages/flows/` v1.1 pages — list / detail / runs tabs are **kept**; only the *definition editor* changes.

## 1. Goal

v1.1's create-flow form was a top-to-bottom list of cards: a trigger card, then 1–8 action cards. That maps a list cleanly — but a *graph* does not fit a vertical form. v2 replaces the definition editor with a **visual canvas**: nodes the operator drags onto a surface and wires together by dragging between ports.

The wedge is unchanged from v1.1 — *a community operator gets from intent to a working flow without reading API docs* — but v2 must keep that true for a flow that **branches** and **runs work in parallel**, which a form cannot show.

## 2. What changes, what stays

| Surface | v1.1 | v2 |
| ------- | ---- | -- |
| `/flows` list | table, one row per flow | **kept**; adds a `v1`/`graph` version badge and a "Convert to graph" item in the `…` menu for legacy flows |
| `/flows/{id}` detail — Runs tab | run history table + action-results drawer | **kept**; the drawer shows `nodeResults` instead of `actionResults` |
| `/flows/{id}` detail — Definition tab | read-only card render | **replaced** by a read-only canvas render |
| `/flows/new`, `/flows/{id}/edit` | vertical card form | **replaced** by the canvas editor |
| run-overlay | none (v1.1 had no live status) | **new** — live per-node status on the canvas |

The list page, the runs history, toast/dialog primitives, the nav entry, and the `RequireAuth`/`RequireAdmin` posture all carry over. The new build is the **canvas** and nothing else — scope the implementation accordingly.

## 3. Canvas technology — the open question

**This is a genuine open question the brief does not pre-decide.** Dioxus has no mature node-graph component the way the React ecosystem has `xyflow`/React Flow. The realistic options:

| Option | What it is | Pro | Con |
| ------ | ---------- | --- | --- |
| **A — bespoke SVG/CSS** | Absolutely-positioned node `div`s, SVG `<path>` bezier edges, pointer-event drag, all in Dioxus/Rust | One language, one stack; no interop; full control of styling; testable with the existing headless probe | Real build effort — drag, connect, pan/zoom, hit-testing are all hand-rolled |
| **B — JS-interop island** | Embed a JS canvas lib (`drawflow`, `rete.js`, `litegraph.js`) as an interop island inside the SPA | Rich editor fast | A JS dependency in a Rust SPA; a state-sync boundary between the JS canvas and Dioxus signals; the headless probe must reach into JS state |
| **C — Rust/WASM graph crate** | An existing Rust graph editor | Single stack | The mature ones (`egui`-based) render to a `<canvas>` bitmap, not the DOM — they do not compose with a Dioxus DOM SPA, lose our CSS/a11y story |

**Recommendation — and it is a recommendation, not a decree:** lean toward **Option A (bespoke SVG/CSS)**. v2 graphs are bounded at ≤ 64 nodes / ≤ 128 edges ([`architecture.md`](./architecture.md) §3.1) — a scale a hand-rolled SVG canvas handles comfortably — and Option A avoids the JS-interop state-sync tax that would otherwise dog every edit, every undo, and every headless test. Option C is effectively ruled out (wrong rendering model).

**The first implementation child filed after ratification is a 2–3 day canvas-tech spike** ([`architecture.md`](./architecture.md) §12, child 1): DioxusLead prototypes the drag-place-connect core in Option A, timeboxed, and either confirms it or escalates for an Option B fallback. The brief commits to the *spike*, not to the outcome — building the wrong canvas is the single most expensive mistake available here.

## 4. The canvas editor — `/flows/new`, `/flows/{id}/edit`

Three-pane layout:

```
┌──────────┬───────────────────────────────┬──────────────┐
│ palette  │            canvas             │  inspector   │
│ (nodes)  │     (drag, connect, pan)      │ (node config)│
└──────────┴───────────────────────────────┴──────────────┘
```

### 4.1 Palette (left)

One entry per node kind ([`architecture.md`](./architecture.md) §4): **Trigger, Action, Branch, Parallel, Delay, Transform, Sub-flow**. Each is a draggable chip with the kind's glyph (§7) and a one-line description. Dragging a chip onto the canvas creates a node at the drop point; `Trigger` is disabled in the palette once the graph already has one (exactly-one-trigger invariant).

### 4.2 Canvas (centre)

- **Nodes** render as cards: a title bar (label + kind glyph), input ports on the left edge, output ports on the right edge. `branch` nodes render one output port per case plus `default`; `action`/`transform`/`parallel`/`subflow` render `out` and `err` ports (the `err` port styled distinctly — see §6).
- **Edges** are SVG bezier curves port-to-port. Drag from an output port to an input port to connect; drop on empty space to cancel. An input port accepts multiple inbound edges (joins).
- **Pan** by dragging empty canvas; **zoom** with the wheel (clamped). No infinite canvas — bounded to the size caps.
- **Select** a node to load it into the inspector; `Delete` removes the node and its incident edges.
- **Layout helper:** a "Tidy" button runs a simple layered (Sugiyama-style) auto-layout — optional, never automatic, so operator-placed nodes are never moved without intent.

### 4.3 Inspector (right)

Shows the selected node's config form — kind-specific, reusing the v1.1 action-card field components for `action` nodes (`command` dropdown, args grid) so that surface is not rebuilt. `branch` gets a case-list editor (label + `when` expression, reorderable); `parallel`/`subflow` get a flow picker; `delay` a duration field; `transform` an output-field grid. Every value field accepts a `{{ … }}` expression (§5 of [`architecture.md`](./architecture.md)); the inspector shows an inline expression-validity hint.

### 4.4 Validation — inline, continuous

The editor calls `POST /api/flows/validate` ([`http-api.md`](./http-api.md) §3.1) on a short debounce after every structural edit. Results render **in place**:

- A **cycle** highlights the offending edges in red with a banner "This graph has a loop — flows must be acyclic."
- An **unreachable node** dims with a tooltip "Not connected to the trigger."
- An **unconnected required port** shows a red port dot.
- A **type-hint mismatch** ([`architecture.md`](./architecture.md) §7.3) shows the edge **amber, not red** — a warning, not a block. Hover explains "This port expects an array; the source emits a string." Save is **not** blocked on warnings.

`[Save]` is disabled while any `error` (not `warning`) is outstanding, with the count surfaced ("3 problems"). This is the canvas earning its keep — the operator never has to read [`http-api.md`](./http-api.md) to learn the rules.

## 5. Run-overlay — live per-node status

New to v2. After `[Fire]`, or when viewing a run from the Runs tab, the canvas enters **overlay mode**: it polls `GET /api/flows/{id}/runs/{runId}` ([`http-api.md`](./http-api.md) §3.2) at ~1 s and paints each node with its `NodeResult.status`:

| Node status   | Canvas treatment |
| ------------- | ---------------- |
| running       | pulsing border, `⟳` glyph |
| `ok`          | green border, `✓` |
| `errored`     | red border, `✕` |
| `skipped`     | dimmed, `↷` (a pruned branch or an upstream error) |
| `interrupted` | grey hatched, `‖` |

Clicking a node in overlay mode opens the inspector in **read-only run mode**: the node's `output` document (or, on error, the error document), `durationMs`, and timestamps. This is the v2 debugging surface — the operator sees *which* path the flow took and *where* it stopped, directly on the graph they built. Polling stops when the run reaches a terminal status.

## 6. Operator-facing footguns surfaced in copy

Carries forward the v1.1 footgun-copy discipline ([`../ui-brief.md`](../ui-brief.md) §6) and adds the graph-specific ones:

1. **`delay` does not survive a restart.** On the `delay` inspector: "If the manager restarts while this is waiting, the run is interrupted. Keep waits short." (Bound is ≤ 15 min — the field rejects longer.)
2. **A `branch` runs exactly one path.** Inspector helper: "Only the first matching case runs. The others — and everything after them — are skipped."
3. **An unwired `err` port fails the run.** When an `action`/`transform`/`subflow`/`parallel` node's `err` port is unconnected, a subtle hint on the port: "If this errors, the run stops here. Wire this port to handle the error instead."
4. **`parallel` fan-out is capped.** Inspector note on `collection`: "At most 256 items; at most 16 run at once."
5. **Sub-flows cannot recurse.** If the operator wires a `subflow` that would form a reference cycle, validation blocks it with "A flow cannot call itself, directly or indirectly."
6. **Single-manager.** The `/flows` footer line is unchanged from v1.1.

## 7. Visual / icon tokens

Reuse the v1.1 flow icon token set ([`../ui-brief.md`](../ui-brief.md) §7.1) for action kinds and run/node statuses — do **not** invent a second icon language. New tokens needed for the three node kinds v1.1 had no glyph for:

| Node kind | Token | Glyph (first pick) | Rationale |
| --------- | ----- | ------------------ | --------- |
| Trigger   | `flow-icon-node-trigger`   | `⚡` | The spark that starts the graph. |
| Branch    | `flow-icon-node-branch`    | `⑂` | A fork in the path. |
| Parallel  | `flow-icon-node-parallel`  | `⇉` | Concurrent arrows. |
| Delay     | `flow-icon-node-delay`     | `⏱` | A timer. |
| Transform | `flow-icon-node-transform` | `⇄` | Reshape — in to out. |
| Sub-flow  | `flow-icon-node-subflow`   | `⧉` | A nested surface. |
| Action    | reuses the v1.1 per-action-kind glyphs (`» ♪ ↗ ≡`) | | |

Glyphs are first-pass picks from the same misc-symbol range the sidebar already ships; UXDesigner/DioxusLead swap any that render as tofu in the live font stack. Every glyph is decorative (`aria-hidden`) — the node card always shows the kind as text. The `err` port is distinguished by **shape and label** ("on error"), not colour alone (WCAG 1.4.1).

## 8. Accessibility

A drag-and-connect canvas is the hardest a11y surface in the product. v2 commits to:

- **Keyboard parity for the core path** — add a node from the palette, select a node, and connect two ports must all be reachable without a pointer (a "connect mode": pick a source port, then a target port). Pan/zoom and free-form repositioning may stay pointer-first in v2; that gap is documented, not hidden.
- Every node card and port has an `aria-label` (kind + label + port name).
- The run-overlay never relies on colour alone — status is also a glyph and a text label in the node card and the inspector.
- This is explicitly a UXDesigner + DioxusLead joint concern; the canvas-tech spike (§3) must include a keyboard-path feasibility check, because Option B (JS interop) makes keyboard parity materially harder.

## 9. Implementation pickup checklist (post-ratification)

For the canvas UI child ([`architecture.md`](./architecture.md) §12, child 4 — blocked by the spike and the wire-types child):

- [ ] Land the canvas-tech spike outcome (§3) before any editor code.
- [ ] Replace the Definition tab and `/flows/new` + `/edit` editors with the three-pane canvas; keep list + runs pages.
- [ ] Wire `POST /validate` debounced inline validation (§4.4).
- [ ] Wire the run-overlay poll against `GET /runs/{runId}` (§5).
- [ ] Add the "Convert to graph" affordance on legacy rows (`POST /convert`).
- [ ] Extend `scripts/headless-probe.sh` to cover the canvas editor and an overlay render.
- [ ] Keyboard-path coverage for add/select/connect (§8).

## 10. Acceptance for the canvas UI child

- A branch-plus-parallel graph can be built entirely on the canvas — drag nodes, connect ports — saved, and fired, with no API-doc reading.
- Inline validation blocks `[Save]` on a cycle and clears once the cycle is broken.
- Firing the flow drives the run-overlay: the taken branch shows `✓`, the pruned branch shows `↷`, within the observation window.
- A legacy v1.1 flow renders read-only on the canvas and can be converted via the `…` menu.
- Headless probe captures a non-blank canvas render and an overlay render (release-WASM build — debug WASM is too large for headless QA).

## 11. References

- [PURA-259](/PURA/issues/PURA-259) — Phase 8 epic.
- [PURA-260](/PURA/issues/PURA-260) — this design brief.
- [`architecture.md`](./architecture.md) — graph model, node catalogue, expression dialect.
- [`http-api.md`](./http-api.md) — `POST /validate`, `GET /runs/{runId}`, `POST /convert`.
- [`../ui-brief.md`](../ui-brief.md) — v1.1 UI brief (list/runs pages and icon tokens reused).
- `crates/ts6-manager-server/src/ui/pages/flows/` — v1.1 pages (list/detail kept, editor replaced).
