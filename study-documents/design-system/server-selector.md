# TS6 Manager — Multi-Tenant Server Selector

Operators frequently manage **more than one** TeamSpeak server connection (per spec §28, §30). The server selector is the single most-used switching control in the app — every per-server query rebinds when it changes. Getting it wrong wrecks the operator's flow.

**Spec basis:** Ch. 30 ("Server-Selection and Multi-Tenant UX") + Ch. 28.3 (selected-server is a global persisted state).

---

## 1. Behavioral contract

| Rule | Spec | Implementation |
|---|---|---|
| The selector lists every `TsServerConfig` the user has access to | Ch. 30 | Fetched once on app boot, refetched on `/servers` mutations |
| Admins see all configs | Ch. 30 | API filters server-side |
| Non-admins see those with `UserServerAccess` rows | Ch. 30 | API filters server-side |
| Exactly-one server: SHOULD auto-select | Ch. 30 | First load only; user choice respected after |
| Zero servers: empty state instead of dashboard | Ch. 30 | Per `phase-1-ux.md` §3.8 |
| Selecting triggers re-fetch of every per-server query | Ch. 30 | Cache key includes `selectedConfigId`; switch invalidates per-server queries |
| Choice persists across reloads | Ch. 28.3 | `localStorage` key `ts6-server` (or whatever DioxusLead chooses) |

---

## 2. Surface — header trigger

The selector lives in the header to the **left** of the user menu. It is the most prominent global control.

### 2.1 Anatomy (closed)

```
┌── Header ────────────────────────────────────────────────────┐
│ ◉ TS6 Manager   ┌──────────────────┐ ●ws  🌙  user ▾  Logout│
│                 │ ⬢ My Community  ▾│                         │
│                 └──────────────────┘                         │
└──────────────────────────────────────────────────────────────┘
```

| Slot | Tokens |
|---|---|
| Height | `--header-height` (56px) |
| Trigger pill height | 36px |
| Trigger pill padding | `--space-3` `--space-5` |
| Trigger background | `--bg-surface-raised` |
| Trigger border | `1px solid --border-subtle` |
| Trigger radius | `--radius-md` |
| Type | `--text-base --weight-medium --text-primary` |
| Icon | leading server-mark `⬢` (`--accent-fg`), trailing chevron-down |
| Hover | `bg: --bg-hover` |
| Open | `bg: --bg-surface-raised`, border `--accent-fg`, chevron rotates 180° |

If the selected server's most recent connection check is failing, the leading icon swaps to a **status dot** in `--warning-bg` or `--danger-bg`. Tooltip on hover: "Connection degraded — see dashboard." This makes the most pressing per-server health signal visible across every screen.

### 2.2 Mobile

On <`--bp-sm`, the trigger is a **full-width bar** below the header rather than a pill in it. Reasoning: phone widths can't fit a pill alongside a hamburger + user menu without truncation that the user can't expand.

```
┌──────────────────────────────────────┐
│ ☰  ◉ TS6 Manager       ●ws  user ▾  │
├──────────────────────────────────────┤
│ ⬢ My Community                  ▾    │  full-width 44px bar
├──────────────────────────────────────┤
│  ... page content                    │
└──────────────────────────────────────┘
```

---

## 3. Surface — open menu

### 3.1 Anatomy

```
┌──────────────────────────────────────────────────┐
│ Switch server                                    │
│ ┌──────────────────────────────────────────────┐ │
│ │ 🔍 Filter…                                    │ │
│ └──────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────┤
│ ⬢  My Community                ●  47/512  ✓     │  selected
│    ts.example.com:10080                          │
├──────────────────────────────────────────────────┤
│ ⬢  Test server                 ●   3/32         │
│    test.local:10080                              │
├──────────────────────────────────────────────────┤
│ ⬢  Friends server              ⊘   offline      │  degraded
│    friends.example.com                           │
├──────────────────────────────────────────────────┤
│  + Add server                                    │  admin only
├──────────────────────────────────────────────────┤
│  ⚙  Manage servers                               │  admin only
└──────────────────────────────────────────────────┘
```

| Slot | Tokens |
|---|---|
| Menu width | `min(380px, calc(100vw - var(--space-7)))` |
| Menu max-height | `min(560px, calc(100vh - var(--header-height) - var(--space-7)))` |
| Background | `--bg-surface-raised` |
| Border | `1px solid --border-subtle` |
| Radius | `--radius-lg` |
| Shadow | `--shadow-md` |
| Section header type | `--text-2xs --weight-semibold --text-secondary`, uppercase |
| Section header padding | `--space-4` `--space-6` |
| Filter input | matches `Input` size `sm` |
| Row height | 60px (two-line: name + host) |
| Row padding | `--space-4` `--space-6` |
| Row hover | `bg: --bg-hover` |
| Row selected | `bg: --bg-selected`, left-border `2px solid --accent-fg` |
| Selected indicator | trailing `✓` in `--accent-fg` |
| Status pill | per `components.md` §8 |

### 3.2 Filter input

- Visible only when the user has 5+ accessible servers (otherwise filter just adds clutter — Hick's Law).
- Filters by name (case-insensitive substring) **and** host (substring).
- Debounce 80ms.
- Up/Down arrow keys navigate the list with the filter active. Enter selects.

### 3.3 Footer actions

- "Add server" — admin only — opens AddServer modal.
- "Manage servers" — admin only — routes to `/servers`.
- Both items are visually separated from the server list by a 1px border-top.

### 3.4 Empty + zero-access cases

- **0 servers, viewer:** menu shows `EmptyState` ("No server access yet. Ask an admin to grant you access."). No add/manage links. Trigger pill disabled with tooltip "No servers available."
- **0 servers, admin:** menu shows two-line empty state ("No TeamSpeak servers connected." + primary `+ Add server` button).
- **1 server, any role:** Per spec, this server is auto-selected on load. The trigger is still rendered (admins may want to add more). The dropdown still opens — no point hiding it.

---

## 4. Switch behavior

### 4.1 Optimistic switch

- On row click: trigger pill text updates **immediately** to the new server name. Menu closes.
- The route the operator is on **stays the same** but its data refetches. The page header subtitle updates ("My Community / Channels" → "Test server / Channels").
- Per-server queries are invalidated; their refetch shows skeleton states until first response.

### 4.2 Slow switch

- If any per-server query takes >300ms (Doherty), display an inline "Switching to *<server>*…" toast with a spinner, anchored bottom-right (per `components.md` §6).
- Toast auto-dismisses on first successful query response.

### 4.3 Failed switch (server unreachable)

- Switch still completes — operator may want to *manage* an unreachable server.
- Each affected per-server card / table shows the standard "server unreachable" error state.
- Status pill in header flips to `Offline`.
- Toast (danger): "Connected to *<server>* but it's not responding. [Open dashboard]"

### 4.4 Cancel

- ESC closes the menu without selecting.
- Click outside closes.

---

## 5. Switching context that survives the move

Some operator state is inherently per-server (selected channel in the channels page, current flow being edited). Some is global (theme, sidebar collapsed). The selector switch must:

- Reset per-server scoped state (selected channel, drilldown filters, table sort).
- Preserve global state (theme, sidebar, panel layouts, pending modal data — though see exception below).

**Exception — open modals.** If the operator has unsaved form data in a modal (e.g., editing a channel's permissions) and switches server:

- Show a `confirm` modal: "Discard changes? You're switching servers — your unsaved channel permission edits will be lost."
- Default action: Cancel (do not switch). Forward action: Discard and switch.

**Lens:** Forgiveness — don't silently destroy operator work.

---

## 6. Keyboard

| Key | Behavior |
|---|---|
| `⌘K` / `Ctrl+K` | Opens the selector menu with filter focused. (Future: this hotkey can grow into a global command palette; for Phase 1 it's just the server picker.) |
| `Escape` | Close menu, no change. |
| `↑` / `↓` | Move highlight up / down. Wraps. |
| `Enter` | Select highlighted server. |
| `/` | When menu is open, focus filter (vim convention). |

---

## 7. Accessibility

- Trigger button: `aria-haspopup="listbox"`, `aria-expanded` reflecting open state, `aria-label="Switch server"`.
- Menu: `role="listbox"`, `aria-activedescendant` updates with arrow-key navigation.
- Each row: `role="option"`, `aria-selected="true"` for the current server.
- Filter: `aria-controls` points at the listbox; results count is announced via `aria-live="polite"` on filter change ("3 servers shown").
- Status indicators are non-color-dependent (dot + text label or icon).

---

## 8. Acceptance criteria

- AC-SS1. Trigger pill renders selected server's name and degraded-status indicator.
- AC-SS2. Menu opens on click and on `⌘K`/`Ctrl+K`.
- AC-SS3. Filter visible only when ≥5 servers accessible.
- AC-SS4. Switching invalidates all per-server queries; per-server cache key includes `selectedConfigId`.
- AC-SS5. Open modals with unsaved data prompt before switch.
- AC-SS6. Single-server users have the server auto-selected on first login (per Ch. 30).
- AC-SS7. Zero-server viewers see the no-access empty state in both the menu and the dashboard (per Ch. 30 + `phase-1-ux.md` §3.8).
- AC-SS8. WCAG AA contrast on all menu rows including the degraded status row.
- AC-SS9. Cache key `ts6-server` (or analog) survives reload; chosen server is the active one on next visit, validated against the user's current access list (no 404 if the server was deleted between sessions).
