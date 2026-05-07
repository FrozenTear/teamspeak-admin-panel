# Bot-Flow Canvas — UX Brief (Preliminary)

**Status:** Preliminary brief for the future Canvas/Editor Engineer (Wave 3, post-FOUNDATION/REST/WS). Hands them a head start so the canvas is not a blank-page problem when they pick it up. Detailed component spec arrives in a follow-up after the engineer is hired and we can co-design.

**Spec basis:** Chapter 31 (Bot Editor — Visual Flow Canvas) and the underlying flow document model (Chapters 12–17).

**Why this is the most demanding UX work in the project.** The canvas is *both* a programming environment *and* a configuration UI for a TS-specific automation domain. It must feel discoverable to a hobbyist gaming-community admin who has never seen a node-graph editor, while still scaling to the operator who wants to wire 30 nodes into a "support ticket flow." Both of these people exist and both must succeed.

---

## 1. Mental model

A flow is a **directed graph** of:

| Node kind | Visual | Required next |
|---|---|---|
| Trigger | Top of graph, no inbound | One outbound, leads to first action/condition |
| Action | One in, one out | Optional; chain ends at no-next |
| Condition | One in, **two outs** (`true`, `false`) | Two distinct outbound branches |
| Variable | One in, one out | Optional |
| Delay | One in, one out | Optional |
| Log | One in, one out (or terminal) | Optional |

Operators describe what they want in event-and-reaction terms: "**when** a client joins #Lobby **and** they're in the new-users group, **send** them a welcome message **and** log it." The canvas should reflect that grammar.

---

## 2. Anatomy

```
┌── Top bar ─────────────────────────────────────────────────────────────────┐
│ ←  Welcome flow  ·  My Community             [Disabled] [▶ Test] [Save]  │
├── Palette ────┬── Canvas ─────────────────────────────────┬── Drawer ─────┤
│  Triggers     │                                            │ Trigger:      │
│  ─────        │   ┌─────────┐                              │ event         │
│  ▷ Event      │   │ Trigger │ event                        │ ─────         │
│  ▷ Cron       │   │ join    │                              │ Event name    │
│  ▷ Webhook    │   └────┬────┘                              │ [notifyclient│
│  ▷ Command    │        ▼                                   │  enterview ▾] │
│               │   ┌──────────┐                             │               │
│  Actions      │   │ Cond     │  client_servergroups        │ Filters       │
│  ─────        │   │ contains │  contains "newuser"         │ + Add filter  │
│  ▷ Send msg   │   └──┬────┬──┘                             │               │
│  ▷ Move       │      │true│false                           │ ─────         │
│  ▷ Kick       │      ▼    └─────► ┌────────┐               │ Variables     │
│  ▷ Ban        │   ┌──────────┐    │ Log    │ "no welcome"  │ flow-scoped:  │
│  ▷ ...        │   │ Action   │    └────────┘               │   counter:0   │
│               │   │ sendmsg  │ "Welcome!"                  │               │
│  Conditions   │   └──────────┘                             │               │
│  ─────        │                                            │               │
│  ▷ if         │                                            │               │
│  ▷ contains   │                                            │               │
│               │                                            │               │
│  Variables    │                                            │               │
│  ─────        │                                            │               │
│  ▷ Set        │                                            │               │
│  ▷ Increment  │                                            │               │
│               │                                            │               │
│  Delays       │                                            │               │
│  ─────        │                                            │               │
│  ▷ Wait       │                                            │               │
│               │                                            │               │
│  Logs         │                                            │               │
│  ─────        │                                            │               │
│  ▷ Log        │                                            │               │
└───────────────┴───────────────────────────┬────────────────┴───────────────┘
                                            │
                ┌── Live execution log (collapsible) ─────┐
                │ ▼ [12:34:01] Trigger fired: client 5    │
                │   [12:34:01] Cond evaluated: false      │
                │   [12:34:01] Log: "no welcome"          │
                └─────────────────────────────────────────┘
```

### 2.1 Sub-surfaces

| Region | Purpose |
|---|---|
| **Top bar** | Flow metadata (name, server), state pill (Enabled/Disabled), Test, Save. |
| **Palette** (left, 200px) | Categorised list of node kinds. Drag-source. |
| **Canvas** (centre, fluid) | Pan/zoom workspace with the node graph. |
| **Drawer** (right, 360px) | Inspector for the selected node. Empty state when nothing selected. |
| **Live execution log** (bottom, collapsible) | Tail of the most recent test/run, streamed over WS. |

### 2.2 Node card spec

```
┌─────────────────────────────┐
│  ▎ TRIGGER                  │  ← category bar (color per category)
│  event                       │
│  notifycliententerview       │
│ ─────────────────────────── │
│  filters: 0                 │
└─────────────────────────────┘
                ●               ← outbound handle (centred bottom)
```

| Slot | Tokens |
|---|---|
| Width | 220px (default), grows to fit text up to 320px |
| Padding | `--space-5` |
| Background | `--bg-surface-raised` |
| Border | `1px solid --border-subtle` |
| Border (selected) | `2px solid --accent-fg` |
| Border (error) | `2px solid --danger-fg` |
| Radius | `--radius-md` |
| Category bar | `4px` left strip in category color |
| Type — category label | `--text-2xs --weight-semibold`, uppercase, letter-spacing 0.05em |
| Type — kind label | `--text-sm --weight-medium --text-muted` |
| Type — title | `--text-base --text-primary` |
| Footer (meta) | `--text-xs --text-muted` |
| Handle | 12px circle, `--bg-surface` border 2px in handle color |

**Category colors** (semantic, not arbitrary):

| Category | Color |
|---|---|
| Trigger | `--info-500` (blue — "this is where it starts") |
| Action | `--accent-bg` (cyan — primary action) |
| Condition | `--warning-bg` (amber — branching, decision) |
| Variable | `--c-neutral-500` (neutral — bookkeeping) |
| Delay | `--c-neutral-500` (neutral) |
| Log | `--success-fg` (green — observation) |

Condition's two handles (`true`/`false`) are colored individually:

- `true` handle: `--success-bg`
- `false` handle: `--danger-bg`

**Lens:** non-color-dependent — handles are also labelled with a tiny "T" / "F" inside the circle for color-blind operators.

---

## 3. Edge creation

- Drag from a source handle (filled) to a target handle (filled). Preview line follows the cursor.
- Snap to target when within 12px.
- Invalid connections (same-node loop, two-target conflict on an action) display the line in `--danger-fg` and the cursor as "not-allowed."
- Multi-edge: an action can only have one outbound; trying to draw a second prompts "Replace existing connection?" inline at cursor.
- Edges are 2px lines with a 1px outline (so they read on any node background). Curve uses bezier with control points pulled toward the source/target handle directions.

---

## 4. Configuration drawer

Opens on node selection. Empty-state message when no node selected: "Select a node to configure."

The drawer **must support every field** for the node kinds in Chapters 13–15. Field-rendering rules:

| Spec field type | UI control |
|---|---|
| Free-text string | `<TextInput>` |
| Numeric | `<NumberInput>` |
| Boolean | toggle switch |
| Enum (small) | segmented control |
| Enum (large) | dropdown |
| `channelId` | **Channel picker** (per Ch. 31 — searchable dropdown with channel tree, fetches `channellist`) |
| `serverGroupId` | Server-group picker (analogous) |
| `clientId` (offline-permitted) | Client search (typeahead from `clientdblist`) |
| Cron expression | Cron expression input + human-readable preview ("Every Monday at 9 AM") |
| Webhook secret | `<PasswordInput>` |
| Filter map (event triggers) | `<TriggerEventFilter>` — repeatable key/value pairs |
| Variable reference | `<VariablePicker>` — dropdown of in-scope vars + `+ New variable` |
| `{{placeholder}}` body | `<TemplateBodyInput>` — autocompletes vars as you type `{{`; preview pane shows resolved sample |

Long fields stack vertically in single-column. Required fields marked per `components.md` §2.5.

### 4.1 Inline validation

- Cron: parse on blur; invalid → red helper "Not a valid cron expression."
- Webhook secret: at least 8 chars; warning if it's a common dictionary word.
- Channel picker: validates the channel still exists on the current server when the drawer mounts; if not, banner inside the drawer "Channel #1234 no longer exists. Pick another."
- WebQuery passthrough action: command field warns red if not in the whitelist (per spec Ch. 16) — links to "see allowed commands" inline.

### 4.2 Drawer chrome

```
┌── Drawer header ────────────────────────┐
│ Send message                  [✕ close] │
│ Action · sendtextmessage                │
├─────────────────────────────────────────┤
│ ...form fields...                       │
├─────────────────────────────────────────┤
│            [Reset]    [Apply]           │
└─────────────────────────────────────────┘
```

- Apply commits the change to the working flow document. Save (top bar) persists to backend.
- Reset reverts the drawer fields to the saved-document state for this node.

---

## 5. Validation before deploy

Save / Enable does **not** persist a malformed flow. Pre-deploy checks:

1. **Trigger present** — exactly one trigger node.
2. **No orphans** — every action/condition/variable/delay/log node is reachable from the trigger.
3. **No cycles** — unless the operator opted in via "Allow cycles" toggle in the top bar (with a confirm-destructive style banner: "Cycles can run forever and cost CPU. Make sure your flow has a clear exit.").
4. **Condition handles wired** — both `true` and `false` lead somewhere (either to nodes or to no-op terminators).
5. **WebQuery whitelist** — every `webquery` action's command in the whitelist.
6. **Cron parseable** — every cron trigger's expression validates.
7. **Channel picker references resolve** — every `channelId` field points at an existing channel.

Save with errors: button is disabled, hover-tooltip explains "3 errors must be fixed first." Each error is also shown as a list in the top bar (clickable — selects the offending node).

---

## 6. Test mode (dry-run)

Top bar `▶ Test` button:

- Asks the operator to provide a **trigger payload** (modal with editable JSON sample matching the trigger kind).
- Calls `POST /api/bots/:id/test` (TODO: backend ticket — does this endpoint exist? If not, propose).
- Streams the resulting execution log live via WS into the bottom panel (the `Live execution log` region).
- During test, nodes that fired highlight in `--accent-bg/0.2` for 1.2s; node currently executing pulses.

This is the operator's main debugging tool. It must work fast (Doherty <400ms to first log line) and clearly.

---

## 7. Template gallery

`+ Templates` in the top bar (or in the empty-state when a flow has no nodes) opens a modal:

- Grid of template cards (illustration + name + description).
- 17 built-in templates from spec Ch. 17.
- Click → preview in a side panel (read-only) → "Use this template" instantiates the flow document into the canvas without enabling.

Template categories (suggested grouping):
- Welcome & onboarding
- Moderation
- Channel automation
- Stats & widgets
- Webhooks (3 templates)

---

## 8. Pan / zoom / minimap

- Drag empty canvas: pan.
- Scroll: zoom (cmd-scroll on Mac for trackpad pinch-equivalent).
- Zoom range: 25%–200%.
- Minimap (bottom-right corner of canvas, 160×120) shows node positions; viewport rectangle indicates current view; click to jump.
- "Fit" button (bottom-left of canvas) recentres and zooms to fit all nodes.

---

## 9. Keyboard

| Key | Behavior |
|---|---|
| `Delete` / `Backspace` | Remove selected node(s) and their edges. |
| `Ctrl+A` / `⌘A` | Select all. |
| `Ctrl+D` / `⌘D` | Duplicate selected node(s). |
| `Ctrl+Z` / `⌘Z` | Undo. (Local history; bounded to 50 ops.) |
| `Ctrl+Shift+Z` / `⌘⇧Z` | Redo. |
| `Ctrl+S` / `⌘S` | Save. |
| `Space (hold) + drag` | Pan (figma-style, prevents accidental node drag). |
| `+` / `-` | Zoom in / out. |
| `0` | Fit. |
| `F` | Focus selected node. |
| `?` | Open keyboard shortcuts overlay. |

---

## 10. Mobile / tablet

The canvas is **out of scope** below `--bp-lg` (1024px). The `/bots/:botId` route on small viewports renders an `EmptyState`:

- Title: "Open this flow on a larger screen."
- Description: "The visual editor needs at least 1024px wide. Switch to a tablet or laptop to edit."
- Secondary action: link to `/bots` (flow list — fully usable on mobile).

The flow list and Enable/Disable controls are mobile-friendly; only the canvas is gated.

---

## 11. Performance budget

- Pan/zoom must stay >50 fps with 100 nodes (extrapolated; test in Wave 3).
- Save: <300ms server round-trip on a 50-node flow (Doherty).
- Test mode log stream: <100ms first frame after WS message.

---

## 12. Implementation suggestion (for the canvas engineer)

Dioxus does not have a mature flow-canvas crate as of 2026-05. Pragmatic options:

| Option | Pros | Cons |
|---|---|---|
| **Hand-written SVG + pointer events** | Pure Dioxus, full control, zero JS | Months of plumbing for pan/zoom/edge routing |
| **Wrap a JS lib via `wasm-bindgen` + `web-sys`** (e.g., `litegraph.js`, `rete.js`) | Faster start, mature canvas | JS interop boundary; bundle size |
| **Hybrid: Dioxus chrome + JS canvas via interop, share state via signals** | Best of both | Two render boundaries to debug |

Recommendation (subject to Wave 3 engineer's call): **Option 3** for v1 — operators care about flows working, not about purity. v1.1 can revisit if a Rust-native canvas crate matures.

This decision should be confirmed with `[DioxusLead](/PURA/agents/dioxuslead)` before the Wave 3 engineer starts.

---

## 13. Cross-references

- Field rendering rules consume the `NodeKindCatalog` JSON contract (impl plan §3.13). Editor and runtime share this catalog as the **single source of truth** — drift between them is a release-blocker.
- Live execution log streams over the WS hub category `flow:execution:log:<botId>` (per spec Ch. 8 + impl plan §3.7).
- Channel picker hits `GET /api/servers/:configId/vs/:sid/channels` (per spec Ch. 7).
- Pre-deploy validations duplicate (intentionally) what the runtime enforces — failing fast in the editor avoids a "saved but won't run" trap.

---

## 14. Open questions for follow-up

1. Does the spec / runtime support a "test fixture" payload that the editor can submit dry? If not, propose `POST /api/bots/:id/test`. Owner: CTO/RustPlatform.
2. Should the canvas support **subflows** (a node that runs another flow)? Not in spec. Out of scope for v1; flag for v1.1 consideration.
3. Undo history: **local-only** (browser session) is the recommendation. Server-side flow versioning is a separate (and large) feature; not Phase 1 / 3 scope.
4. Multi-operator concurrent editing: out of scope for v1 (last-write-wins). Document this in the editor with a "loaded N seconds ago" indicator + "Reload" if remote changes detected.
