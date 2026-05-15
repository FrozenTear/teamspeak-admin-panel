# Flow engine — v1.1 UI brief

- **Status:** draft, pending board ratification ([PURA-198](/PURA/issues/PURA-198)).
- **Companion docs:** [`architecture.md`](./architecture.md), [`http-api.md`](./http-api.md), [`v1.1-gate.md`](./v1.1-gate.md).
- **Owners (implementation):** [DioxusLead](/PURA/agents/dioxuslead) for component scaffolding + routing + state; [UXDesigner](/PURA/agents/uxdesigner) for the create-flow form layout, empty-states, and run-status iconography.
- **Style anchor:** `crates/ts6-manager-server/src/ui/pages/music_bots/` — matches the same `RequireAuth`-fronted, single-virtual-server-scope, table+detail pattern.

## 1. Goal

A community operator can, on a fresh manager, get from "I want a welcome message" to "it works" in **under five minutes** without leaving the web UI and without reading the API docs. That is the v1.1 wedge for flow automation.

## 2. Route map

| Route                    | Page             | Notes                                                                      |
| ------------------------ | ---------------- | -------------------------------------------------------------------------- |
| `/flows`                 | Flow list        | Default landing for the Flows nav item.                                     |
| `/flows/new`             | Create-flow form | Modal-style page; "Cancel" returns to `/flows`.                             |
| `/flows/{id}`            | Flow detail      | Tabs: **Definition**, **Runs**. Default tab = Runs (operator's "did it work" question dominates). |
| `/flows/{id}/edit`       | Edit form        | Same shape as `/flows/new`. Definition section is read-only while the flow is enabled (matches API constraint). |

Routing wires into the existing Dioxus router in `crates/ts6-manager-server/src/ui/pages/mod.rs`. Add a "Flows" entry to the nav bar between "Music bots" and "Widgets" — flow automation reads as "do something with the bot / the server", which is the cognitive neighbour of those features.

## 3. Page-by-page

### 3.1 `/flows` — list

**Empty state.** Centred card:

> No flows yet. Flows let you trigger an action when something happens — for example, send a welcome message when a client joins.
>
> **\[Create a flow]**

**Populated state.** One table, one row per flow. Columns:

| Name | Trigger | Status | Last run | Actions |
| ---- | ------- | ------ | -------- | ------- |
| `welcome-on-join` | `ts6ClientJoined` (channel 5) | ● Enabled | `5m ago — ok (320 ms)` | `[Fire]` `[Edit]` `[…]` |

- "Status" is a one-pill display: `Enabled` (green), `Disabled` (grey).
- "Last run" uses the `lastRun` summary on the `Flow` wire object — `started_at` relativised, `status` colourised. `null` renders as `—`.
- "Fire" is a button that calls `POST /api/flows/{id}/fire` and flashes a toast `Fired run #{runId}`; click again navigates to the run row in the detail page.
- "Edit" → `/flows/{id}/edit`.
- "…" overflow menu: **Enable / Disable** (toggle), **Delete**. Delete prompts: "Delete this flow? Run history will be removed." If the API returns `run_in_flight`, the dialog offers `[Force delete]` which re-posts with `?force=true`.

Empty-state and populated-state share the same nav header and a "**+ New flow**" primary button top-right.

### 3.2 `/flows/new` — create form (UXDesigner-led layout)

Sections, top-to-bottom:

1. **Identification**
   - `name` (text, required, validates against the 120-char + per-virtual-server uniqueness rule live).
   - `description` (textarea, optional, 280 char hint).

2. **Target server** (auto-populated from the active virtual server; collapsible "show more" reveals `serverConfigId` and `virtualServerId` for power users — these default to the currently viewed server).

3. **Trigger** — radio group of three cards:

   - **On a schedule** — a `cron` expression input with three preset chips ("every 5 min", "hourly", "daily at noon UTC") that pre-fill the input. Live validation message below. UXDesigner note: do **not** expose cron as the only input; chips are the wedge for non-power-users.
   - **Manually only** — no extra fields. Helper text: "Useful for testing or for actions you only want to run on demand."
   - **When a client joins** — optional `channelId` dropdown (populated from the existing channels API). Default: any channel.

4. **Actions** — drag-reorderable list of up to 8 cards. Each card is an action kind selector with kind-specific fields:

   - **Send TS6 command** — `command` dropdown (whitelisted), `args` JSON editor with key/value rows. Operators may type `${trigger.…}` placeholders into arg values by hand; they are resolved server-side. *(The substitution chip-picker is deferred — see the note below.)*
   - **Send music-bot command** — bot selector (active bots on this virtual server), `command` dropdown, args grid.
   - **Send a webhook** — `url` text (validated against the SSRF allow-list reflectively: a "validate" button calls a future `POST /api/flows/validate-webhook` — **out of scope for v1.1**, render as an inline note "URL is sent as-is; ensure it is on the manager's allow-list"), optional headers grid.
   - **Write a log line** — `message` textarea. Always-available; useful as a smoke action.

   Bottom of the list: `[+ Add action]`.

   > **Substitution chip-picker — deferred to v1.2 (PURA-253 M2).** This section originally sketched a `${trigger.…}` chip-picker that would insert templated trigger references into action args. Building it in v1.1 would race the engine-side substitution contract: the resolved key set differs per trigger kind and is not yet pinned, so a picker built today risks offering keys the engine will not resolve — a signifier that lies is worse than no signifier. v1.1 operators type `${trigger.…}` placeholders by hand; they are still resolved server-side. Revisit in v1.2 once the substitution contract is pinned.

5. **Save** — primary button `[Create flow]` + a checkbox **Enable on save** (default off; the create-then-fire-from-list-page flow is intentional so the operator can sanity-check before going live).

Form-level validation surfaces inline; server-side validation errors land in a banner above the **Save** button with the `ErrorBody.message`.

### 3.3 `/flows/{id}` — detail (tabs)

**Runs tab (default).** Reverse-chronological list, one row per `FlowRun`:

| When | Trigger | Status | Duration | Error |
| ---- | ------- | ------ | -------- | ----- |
| `2026-05-21 14:02:11 UTC` | `manualFire` | ● Ok | 318 ms | — |
| `2026-05-21 13:55:00 UTC` | `cron (0 */5 * * * *)` | ● Errored | 1.2 s | `ts6Command: client not on server` |

Click-through opens a slide-out panel with `actionResults[]` — one row per planned action, kind, status, durationMs, error if any. This is the **operator's debugging surface** and the reason the run history is the default tab.

Status dot legend lives in a small footer toggle so it doesn't dominate the page.

Pagination is a `[Load more]` button at the bottom (uses `?cursor=`). v1.1 does not auto-poll the runs list — refresh is a manual `[Refresh]` button next to `[Load more]`. Auto-refresh is a v1.2 nice-to-have.

**Definition tab.** Read-only render of the flow's trigger + actions, using the same card components as the create form but in display mode. A `[Edit definition]` button at the top is enabled only when `enabled = false` (matches API `definition_swap_locked`); when disabled, hover tooltip explains "Disable the flow to change its definition."

Header bar of the page: `[Fire]` `[Disable / Enable]` `[Delete]`, mirroring the row actions on `/flows`.

### 3.4 `/flows/{id}/edit`

Same shape as `/flows/new`. The "Trigger" and "Actions" sections render disabled with an inline notice "This flow is enabled. Disable it to edit the trigger and actions." Identification, target server (`virtualServerId`), and the `enabled` toggle stay live.

## 4. State / data layer

- Dioxus signals scoped per page (no global flow store in v1.1).
- API client lives in `crates/ts6-manager-server/src/ui/api/flows.rs` (new), mirroring `ui/api/music_bots.rs`.
- All requests carry the same JWT as the rest of the UI (existing `RequireAuth` middleware on the SPA's API calls).

## 5. Empty / error / loading

- **Loading** — table-row skeletons; no spinner overlay (`music_bots` page convention).
- **Permission denied (read)** — never happens in v1.1 because read is `RequireAuth` and any logged-in user can read.
- **Permission denied (write)** — toast: "Admin-only. Ask your server admin to make the change." Buttons stay visible (preserves discoverability) but disabled with a tooltip explaining the requirement.
- **Engine saturated (503)** — toast: "Flow engine busy. Try again in a moment." We do not auto-retry.
- **Rate-limited fire (429)** — toast: "Slow down — flow can only fire once every 2 s manually."

## 6. Operator-facing footguns we surface in copy

These are the architectural sharp edges; the UI is responsible for making them legible.

1. **Cron does not catch up on restart.** Below the cron input: "Missed ticks during downtime are not replayed."
2. **In-flight runs are dropped, not queued.** On the detail page, if a fired trigger collided with an in-flight run, the run row carries `status = "skipped_disabled"` (engine reuses this status; future docs may rename) and a tooltip explains "A run was already in flight."
3. **Self-trigger loops.** Help text on the actions section: "If your actions cause the same trigger to fire again, the second run will be dropped — but it's better to design the flow so this doesn't happen."
4. **Single-manager.** Status footer on `/flows`: "Flows run on this manager only. (Multi-manager support is a future feature.)"

## 7. Visual design notes (UXDesigner pickup)

- Reuse the existing pill / badge / button atoms from `music_bots` — do not invent a flow-specific design language.
- The create form is the only page that needs new layout thinking; everything else is "same table, different columns". Allocate UX design effort accordingly.
- Action kinds and run statuses carry glyph icons for fast scanning — the resolved token set is **§7.1** (PURA-253 H2 / L2).

### 7.1 Flow icon token set (PURA-253 H2 / L2)

The flow pages use glyph icons — the manager's existing `.icn` pattern (no SVG
registry; the sidebar passes Unicode glyphs directly). Each icon is a named
token; DioxusLead wires the helpers. Every placement also shows a text label,
so the glyph is decorative (`aria-hidden="true"`) and the surfaces stay
colour- and icon-independent.

**Action-kind icons** — actions-list cards (form), read-only Definition tab,
per-run action-results drawer:

| Action kind        | Token                        | Glyph | Rationale |
| ------------------ | ---------------------------- | ----- | --------- |
| TS6 command        | `flow-icon-ts6-command`      | `»`   | A command sent *into* the server — the brief's chevron-into-server. |
| Music-bot command  | `flow-icon-musicbot-command` | `♪`   | Reuses the **Music bots** nav glyph — recognition link between the action and the feature it drives (Jakob's Law). |
| Webhook            | `flow-icon-webhook`          | `↗`   | An arrow leaving the manager — outbound HTTP. |
| Log line           | `flow-icon-log-line`         | `≡`   | Reuses the **Logs** nav glyph — same recognition link. |

**Run / action-status icons** — inside the `bot-badge` pill and the status
dot-legend. The surface is already colour-independent via the text label; the
glyph hardens that (WCAG 1.4.1) and, more usefully, disambiguates
**Interrupted** from **Skipped**, which share the neutral colour today:

| Status             | Token                          | Glyph |
| ------------------ | ------------------------------ | ----- |
| Running (InFlight) | `flow-icon-status-running`     | `⟳`   |
| Ok                 | `flow-icon-status-ok`          | `✓`   |
| Errored            | `flow-icon-status-errored`     | `✕`   |
| Interrupted        | `flow-icon-status-interrupted` | `‖`   |
| Skipped            | `flow-icon-status-skipped`     | `↷`   |

Per-action status (`Ok`/`Errored`/`Skipped`) reuses the same three glyphs.

**Editor remove control (L2-tail)** — `flow-icon-remove` → `×` (the existing
glyph; PURA-248 already added the `aria-label`s). Folding it into the token set
means the key/value and header `×` controls source one registry, not a bare
string literal. The remove button needs a **≥24×24 px** hit target (Fitts's
Law) regardless of glyph size.

Glyphs are first-pass picks from the same misc-symbol range the sidebar already
ships (`♪ ≡ ⊘ ⚙ ▶`); DioxusLead swaps any that render as tofu in the live font
stack. Full helper signatures + CSS are in PURA-253.

## 8. Accessibility / i18n

- All buttons + form inputs get accessible labels (Dioxus `aria-label` props).
- The cron expression input is paired with the chip presets specifically because raw cron is **not** accessible to most operators — that decision is a UX rather than a11y concern but the result is more legible.
- i18n is out of scope for v1.1 (matches the rest of the manager UI today).

## 9. Implementation pickup checklist (for DioxusLead)

When the implementation child is filed (after v1.0 tags green per [PURA-164](/PURA/issues/PURA-164)):

- [ ] Add the `Flows` nav entry in `ui/pages/mod.rs`.
- [ ] Scaffold `ui/pages/flows/` with `mod.rs`, `list.rs`, `detail.rs`, `form.rs`.
- [ ] Wire the API client `ui/api/flows.rs` using the shared types from `ts6_manager_shared::flows`.
- [ ] Build the create form per §3.2, deferring to UXDesigner for the trigger-card visuals.
- [ ] Hook in the existing toast + dialog primitives — no new ones in v1.1.
- [ ] Headless screenshot probe added to `scripts/headless-probe.sh` invocation in CI for the three new routes.

## 10. Acceptance for the UI implementation child

- Empty `/flows` renders the create CTA.
- A `logLine` flow can be created via the form, fired from the list page, and the resulting run row appears in the detail Runs tab within 5 s.
- A `ts6ClientJoined` flow saved with `enabled = true` registers (verifiable via `GET /api/flows/{id}` returning `enabled = true`) and the manager logs the trigger registration on startup.
- All write routes are blocked for a non-admin user, with the documented tooltip.
- WCAG-AA contrast on the status pills and dot legend.

## 11. References

- [PURA-198](/PURA/issues/PURA-198) — design issue.
- [`architecture.md`](./architecture.md) — engine internals.
- [`http-api.md`](./http-api.md) — wire surface.
- [`v1.1-gate.md`](./v1.1-gate.md) — gate probe.
- `crates/ts6-manager-server/src/ui/pages/music_bots/` — closest existing page family.
