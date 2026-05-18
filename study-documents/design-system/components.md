# TS6 Manager — Component Primitives (v1)

The minimum-viable component library for Phase 1. Every primitive references tokens from `tokens.md`; nothing is hard-coded.

DioxusLead implements each primitive as a Dioxus component with a typed props enum. The HTML-equivalent markup in this doc is **specification, not implementation** — it documents the rendered shape, attributes, and ARIA contract.

---

## 1. Button

**Variants:** `primary`, `secondary`, `ghost`, `danger`, `link`.
**Sizes:** `sm` (28px), `md` (36px — default), `lg` (44px — touch).
**States:** rest, hover, active, focus-visible, disabled, loading.

### 1.1 Spec

| Slot | Tokens |
|---|---|
| Min height | `--touch-min` (40px) for `md`, `--touch-comfortable` (44px) for `lg`, 28px for `sm` |
| Padding (X) | `--space-5` (md), `--space-4` (sm), `--space-6` (lg) |
| Padding (Y) | `--space-3` (md/sm), `--space-4` (lg) |
| Radius | `--radius-md` |
| Type | `--text-base` `--weight-medium` |
| Gap (icon ↔ label) | `--space-3` |
| Transition | `background-color`, `border-color`, `color` × `--motion-fast` `--ease-standard` |

### 1.2 Variants

| Variant | Rest | Hover | Active | Disabled |
|---|---|---|---|---|
| `primary` | `bg: --accent-bg` `fg: --text-inverse` | `bg: --accent-bg-hover` | brightness(0.92) | `bg: --c-neutral-300` `fg: --c-neutral-500` |
| `secondary` | `bg: transparent` `border: 1px --border-strong` `fg: --text-primary` | `bg: --bg-hover` | `bg: --bg-surface-raised` | `border: --c-neutral-300` `fg: --c-neutral-500` |
| `ghost` | `bg: transparent` `fg: --text-primary` | `bg: --bg-hover` | `bg: --bg-surface-raised` | `fg: --c-neutral-500` |
| `danger` | `bg: --danger-bg` `fg: --text-inverse` | brightness(1.08) | brightness(0.92) | `bg: --c-neutral-300` `fg: --c-neutral-500` |
| `link` | `bg: transparent` `fg: --text-link` `underline` | `fg: brighten(--text-link)` | underline-thickness 2px | `fg: --c-neutral-500` |

### 1.3 Loading

When `loading={true}`:

- Button is `aria-busy="true"`, `disabled`.
- Label fades to `opacity: 0` over `--motion-fast`; spinner appears centred.
- Spinner is a 16px stroke ring, `currentColor`, `animation: spin 600ms linear infinite` (suppressed by reduced-motion: replaced by static "…").
- Width is **frozen** at the rest-state width to prevent layout jump (Lens: Doherty / system feedback consistency).

### 1.4 Focus

```css
.btn:focus-visible {
  outline: none;
  box-shadow: var(--shadow-focus);
}
```

`:focus-visible` (not `:focus`) so mouse-clicks don't draw the ring; keyboard tabs do. Required by WCAG 2.4.7.

### 1.5 Destructive confirmation

`danger` variant alone is **not** a confirmation — it is a styling hint. Any irreversible action MUST open a `<Modal kind="confirm-destructive">` (see §3) with explicit verb-typing or "I understand" affirmation. No surprise deletes.

### 1.6 Behavior contract

- Submit-form buttons must accept `formaction`/`formmethod`.
- All button labels are imperative verbs ("Add server", "Save changes"). Never "OK" / "Yes".

---

## 2. Input (Text, Number, Password)

### 2.1 Spec

| Slot | Tokens |
|---|---|
| Height | 40px (md) — matches `--touch-min` |
| Padding (X) | `--space-5` |
| Padding (Y) | `--space-3` |
| Background | `--bg-surface-sunken` |
| Border | `1px solid --border-subtle` |
| Border (focus) | `1px solid --accent-fg` + `--shadow-focus` |
| Border (error) | `1px solid --danger-fg` |
| Radius | `--radius-md` |
| Type | `--text-md` (16px to avoid iOS zoom) |
| Placeholder color | `--text-muted` |
| Disabled | `bg: --bg-surface` `fg: --text-muted` `cursor: not-allowed` |

### 2.2 Affixes

Optional left/right slot for icons or unit text (e.g., `:` for port, `ms` for delays). Affix is non-interactive unless explicitly a button (e.g., password reveal toggle).

### 2.3 Password reveal

Eye / eye-slash toggle in right affix. Aria: `aria-pressed`, label switches between "Show password" / "Hide password". Toggling does NOT submit the form.

### 2.4 Number input

- `inputmode="numeric"`, `pattern="[0-9]*"` — mobile keyboards switch to numeric pad.
- Spinner buttons hidden by default (`appearance: textfield`); they are inconsistent across browsers and operators rarely use them.
- Min/max enforced server-side regardless of `min`/`max` attributes.

### 2.5 Required marking

Required fields display a `*` after the label, color `--danger-fg`, with `aria-label="required"`. Optional fields say "(optional)" in `--text-muted` `--text-xs`.

**Lens:** Postel's Law — accept liberally on the server, but signal expectations clearly to the user.

---

## 3. Modal

### 3.1 Variants

| Variant | Use |
|---|---|
| `default` | Forms, configuration |
| `confirm` | Two-button choice (Save/Cancel, Connect/Cancel) |
| `confirm-destructive` | Type-to-confirm or "I understand" affirmation for irreversible actions |
| `sheet` | Bottom-sheet on mobile |

### 3.2 Sizes

| Size | Width |
|---|---|
| `sm` | `min(420px, calc(100vw - var(--space-8)))` |
| `md` | `min(560px, calc(100vw - var(--space-8)))` (default) |
| `lg` | `min(720px, calc(100vw - var(--space-8)))` |
| `xl` | `min(960px, calc(100vw - var(--space-8)))` |

### 3.3 Anatomy

```
┌─ Backdrop (--bg-canvas + alpha 0.7, blur 4px) ─────────────────┐
│                                                                 │
│   ┌─ Modal panel (--bg-surface-raised, --shadow-lg) ─────────┐ │
│   │ Header  Title (--text-lg --weight-semibold)        [✕]   │ │
│   │ ─────────────────────────────────────────────────────────│ │
│   │ Body   Form fields, content                              │ │
│   │ ─────────────────────────────────────────────────────────│ │
│   │ Footer  [Secondary] [Primary →]              right-aligned│ │
│   └──────────────────────────────────────────────────────────┘ │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

- Padding: `--space-7` (header/body/footer).
- Header bottom border, footer top border: `1px solid --border-subtle`.
- Footer button order — primary action **right** (Western reading order, matches platform convention on Windows/Linux/web; matches Mac ordering in modal contexts post-Big-Sur).

### 3.4 Behavior

- ESC closes (unless `disableEscape`, which only `confirm-destructive` should set).
- Click outside closes (unless `disableBackdropClose`, ditto).
- Focus is **trapped**: Tab/Shift+Tab cycle within the modal. First focusable element gets focus on open; close returns focus to the trigger.
- Background scroll locked.
- Aria: `role="dialog"`, `aria-modal="true"`, `aria-labelledby` (header id), `aria-describedby` (body id if descriptive).

### 3.5 Confirm-destructive specifics

- Body MUST state the consequence in plain language ("This will permanently delete the server connection and all associated bots, flows, and widgets. This cannot be undone.").
- Body MUST list the exact entities affected if knowable ("3 flows, 1 music bot, 2 widgets").
- Confirmation requires either:
  - **Type-to-confirm**: input that must match the entity name (used for: server delete, user delete, regenerate token).
  - **Checkbox affirmation**: "I understand this is permanent." (used for: clear all flow executions, drop server connection logs).
- Primary button is `danger` variant, label is the verb ("Delete server", not "Confirm").
- No autofocus on the destructive button (the user reaches it deliberately).

**Lens:** Forgiveness — destructive actions provide clear undo or require deliberate effort. No dark patterns.

### 3.6 Sheet (mobile)

On viewport <`--bp-sm`, modals render as bottom sheets:

- Slide up from bottom over `--motion-slow` `--ease-emphasized`.
- Width = 100vw, max-height 90vh, scrollable inside.
- Drag-handle bar (32×4, `--c-neutral-400`) at top; tap-anywhere-outside or drag-down dismisses.

---

## 4. Table row

Tables are dense by default (operator workflow). The "row" primitive defines the rhythm of every list-style screen (clients, channels, flows, bots, widgets, logs).

### 4.1 Spec

| Slot | Tokens |
|---|---|
| Row height (default) | 44px |
| Row height (compact) | 36px |
| Padding (X — outer) | `--space-6` |
| Padding (X — inner cells) | `--space-5` |
| Border-bottom | `1px solid --border-subtle` |
| Hover | `bg: --bg-hover` (300ms transition not used; instant feedback) |
| Selected | `bg: --bg-selected`, left-border `2px solid --accent-fg` |
| Focus (keyboard nav) | `outline: 2px solid --focus-ring; outline-offset: -2px;` |

### 4.2 Header

- Background: `--bg-surface` (matches surrounding card).
- Sticky on vertical scroll within the card.
- Type: `--text-xs --weight-semibold --text-secondary`, uppercase, letter-spacing 0.04em.
- Sort indicator: chevron in trailing position; color `--accent-fg` when active.

### 4.3 Cell shapes

- **Identifier cell** (channel name, client nickname): icon + label + optional secondary line. Truncate with ellipsis at row width; full text in tooltip.
- **Status cell**: pill component (see Tag). Width auto, never wraps.
- **Numeric cell**: right-aligned, `--font-mono`, tabular-nums for clean column.
- **Action cell**: trailing column, right-aligned. Icon-buttons with tooltips. Always last.

### 4.4 Empty / loading row states

- **Loading**: skeleton bars (see §6.2) for ≥3 rows.
- **Empty**: collapses to a single `EmptyState` component spanning all columns (see §7).
- **Error**: error banner spanning all columns with retry action.

### 4.5 Selection

- Row checkbox in leading position. Header-row checkbox is tri-state (none / some / all).
- Selecting one row reveals a **bulk-action toolbar** that overlays the table header (slide down, `--motion-fast`).
- Toolbar shows count + actions. ESC clears selection.

---

## 5. Form field (composite)

A wrapper that combines label, input, helper text, and error state.

### 5.1 Anatomy

```
┌─ Label  required-mark? optional-mark? ────────────────────────┐
│                                                                │
│ ┌─ Input (or other control) ─────────────────────────────────┐│
│ └────────────────────────────────────────────────────────────┘│
│                                                                │
│ ↳ Helper text  (or)  Error message  (or)  Success message     │
└────────────────────────────────────────────────────────────────┘
```

### 5.2 Spec

| Slot | Tokens |
|---|---|
| Stack gap | `--space-3` (label → control → message) |
| Label type | `--text-sm --weight-medium --text-primary` |
| Helper type | `--text-xs --text-muted` |
| Error type | `--text-xs --danger-fg` |
| Required mark | `*` `--danger-fg` |
| Optional mark | `(optional)` `--text-muted` `--text-xs` |

### 5.3 Validation behavior

- Validate **on blur** for first error; **on input** after the first error has appeared. Avoids interrupting in-progress typing.
- Error message states the rule, not just "invalid": "Password must contain at least one digit." Cite from spec §6.2.2.
- Inline error icon (warning triangle) leads the message.
- `aria-describedby` links the input to the message id; `aria-invalid="true"` on error.

### 5.4 Layout

- Single-column for forms ≤6 fields. **Lens:** linear scan, no decision branching.
- Two-column only for closely-related pairs (host + port, width + height). Pair must visually group via `Common Region` (shared subtle background).
- Form actions (footer) are **always right-aligned**, primary on the right. Cancel/back is `secondary` or `ghost`.

---

## 6. Toast

Transient feedback for non-blocking events. Anchored bottom-right (desktop), bottom (mobile).

### 6.1 Variants

| Variant | Icon | Color tokens |
|---|---|---|
| `success` | check-circle | `--success-fg` accent, `--bg-surface-raised` body |
| `info` | info-circle | `--info-500` accent, `--bg-surface-raised` body |
| `warning` | alert-triangle | `--warning-fg` accent, `--bg-surface-raised` body |
| `danger` | x-circle | `--danger-fg` accent, `--bg-surface-raised` body |
| `loading` | spinner | `--text-secondary` accent, `--bg-surface-raised` body |

### 6.2 Anatomy

```
┌─ ▍ icon  Title                                   ✕ ─┐
│         Optional one-line detail.                    │
│         [Action button (text only)]                  │
└──────────────────────────────────────────────────────┘
```

- Width: `min(420px, calc(100vw - var(--space-7)))`.
- Padding: `--space-5` `--space-6`.
- Left accent bar (`--space-1` × full height) in variant color, helps color-blind discrimination.
- Stack vertical gap: `--space-4` between concurrent toasts.
- Max concurrent: 3 visible; overflow queues.

### 6.3 Timing

| Variant | Default duration |
|---|---|
| `success` | 4s |
| `info` | 6s |
| `warning` | 8s |
| `danger` | 0 (manual dismiss) |
| `loading` | 0 (replaces self with success/danger when done) |

Hovering pauses the timer. ESC dismisses the most-recent. Action button click dismisses immediately.

### 6.4 Accessibility

- `role="status"` for success/info/warning/loading (`aria-live="polite"`).
- `role="alert"` for danger (`aria-live="assertive"`).
- `aria-atomic="true"` so the whole content reads on each update.
- Reduced-motion: skip slide-in, fade only.

### 6.5 What toasts are NOT for

- **Errors that block a form submit** — these belong inline to the field (or as a banner above the form). Toasts are dismissable; field errors are persistent until fixed.
- **Confirmations of dangerous actions** — destructive results stay on screen as a banner with explicit undo (see §3.5).

---

## 7. Empty state

Used wherever a list, table, or panel has zero items. **Critical surface** — first-time setup, no flows, no widgets, viewer with zero server access (Ch. 30).

### 7.1 Anatomy

```
                  ┌─────────┐
                  │  Icon   │   60×60 illustration
                  │ (vector)│
                  └─────────┘

                Title (--text-xl --weight-semibold)
       One-sentence description (--text-base --text-secondary)

              [Primary action]  [Secondary action]
```

### 7.2 Spec

| Slot | Tokens |
|---|---|
| Container padding | `--space-9` `--space-7` |
| Stack gap | `--space-5` (icon → title → desc → actions) |
| Title type | `--text-xl --weight-semibold --text-primary` |
| Desc type | `--text-base --text-secondary`, max-width `48ch` |
| Action gap | `--space-4` |
| Background | `--bg-surface` (when used inside a card) or transparent (page-level) |
| Border | dashed `1px --border-subtle` for "drop here" / "create here" patterns; none otherwise |

### 7.3 Variants

- **Setup** ("Add your first server") — primary CTA prominent, illustrated.
- **Filtered** ("No flows match your search") — secondary action `Clear filters`.
- **Permission-denied** ("You don't have access to any servers yet") — explanation + contact path. NO retry button.
- **Error** (network failure) — icon in `--danger-fg`, retry button as primary.

### 7.4 Copy guidelines

- Title: state the situation, not the cause. "No flows yet." not "Failed to load flows."
- Description: one sentence, plain language, avoid jargon ("flows" is fine; "BotExecution rows" is not).
- Actions: imperative verb ("Add server", "Create flow", "Clear filters"). Never "Click here".

### 7.5 Lens references

- **Recognition over recall** — empty states show what would be there.
- **Information scent** — actions point at the next step, not back to the menu.
- **Aesthetic-Usability** — illustrated empty states reduce perceived friction.

---

## 8. Tag / Status pill (supporting primitive)

Used in tables (status cells), header (server status indicator), and chips throughout.

### 8.1 Spec

| Slot | Tokens |
|---|---|
| Height | 22px |
| Padding (X) | `--space-4` |
| Radius | `--radius-full` |
| Type | `--text-xs --weight-medium` |
| Gap (dot ↔ label) | `--space-3` |

### 8.2 Variants

| Variant | Dot color | Bg | Fg |
|---|---|---|---|
| `online` / `success` | `--success-bg` | `--c-success-500/0.12` | `--success-fg` |
| `connecting` / `info` | `--info-500` | `--c-info-500/0.12` | `--info-600` |
| `idle` / `neutral` | `--c-neutral-500` | `--c-neutral-300/0.5` | `--text-secondary` |
| `warning` | `--warning-bg` | `--c-warning-500/0.12` | `--warning-fg` |
| `offline` / `danger` | `--danger-bg` | `--c-danger-500/0.12` | `--danger-fg` |

Status communicated via **dot + label**, not color alone (WCAG 1.4.1).

---

## 9. Skeleton (supporting primitive)

Loading placeholder bars. Replace content during fetch; suppress for fetches expected <100ms (just show stale content or no flicker).

### 9.1 Spec

- Background: `linear-gradient(90deg, --bg-surface-sunken 0%, --bg-hover 50%, --bg-surface-sunken 100%)`.
- Animation: `shimmer 1500ms ease-in-out infinite` (suppressed by reduced-motion → static `--bg-surface-sunken`).
- Default height: matches the type token of the replaced text (e.g., 14px for `--text-base`).
- Default radius: `--radius-md` for blocks, `--radius-full` for avatars.

### 9.2 When to use

- Initial page load: yes.
- Subsequent refetch (TanStack Query equivalent): no — keep stale data, show subtle "refreshing" indicator in header.
- Optimistic UI: no — show the optimistic value, revert on error.

---

## 10. Card

A surface for grouping related content. Used for KPI tiles, dashboard panels, the connection card, the login card, the per-server entry on `/servers`, etc. CSS class: `.card` (and the dashboard-local `.panel` is the same primitive — it should fold into `.card` by Phase 2).

### 10.1 Spec

| Token | Value |
|---|---|
| Background | `--bg-surface` |
| Border | `1px solid --border-subtle` |
| Radius | `--radius-md` (8px) |
| Padding (default) | `--space-7` (24px) |
| Padding (compact) | `--space-6` (16px) — for KPI tiles |
| Shadow | `--shadow-sm` (raise to `--shadow-md` only on hover when interactive) |

Only one elevation step per card. Cards do not nest in cards — switch to a `<section>` with no surface treatment instead.

### 10.2 Variants

| Variant | When |
|---|---|
| `default` | Static surface (login form, KPI tile, panel). |
| `interactive` | Whole-card click target (server list row, dashboard "go to channels" tile). Add `cursor: pointer`, `:hover { box-shadow: var(--shadow-md); border-color: var(--border-strong); }`. |
| `selected` | Currently active (e.g., the active server in `/servers`). Border becomes `--accent-fg`, optional 2px inset on the leading edge. |

### 10.3 Anatomy

`Card { header?, body, footer? }`. Header is optional; when present, it's a single row with title + optional `actions` slot (right-aligned). Footer is for primary/secondary action buttons; right-aligned, gap `--space-4`.

### 10.4 Lens references

- **Common Region (Gestalt)**: card border + shared padding tells the eye "these elements belong together."
- **Pragnanz**: a single rectangle with one elevation step is more readable than a stack of dividers.

---

## 11. Layout chrome (Sidebar + Header)

App chrome is two primitives: a fixed left **Sidebar** with grouped navigation and a top **Header** that hosts the server selector, websocket indicator, theme toggle, user menu, and logout. CSS classes: `.app`, `.sidebar`, `.header`, `.main`. Implemented end-to-end in `preview/dashboard.html`.

### 11.1 Layout grid

```
┌──────────┬────────────────────────────────┐
│  brand   │  header (selector / spacer / user)
├──────────┼────────────────────────────────┤
│  sidebar │  main
│          │
└──────────┴────────────────────────────────┘
```

| Token | Value |
|---|---|
| Sidebar width (expanded) | `--sidebar-width-expanded` (240px) |
| Sidebar width (collapsed) | `--sidebar-width-collapsed` (64px) — Phase 2, not Phase 1 |
| Header height | `--header-height` (56px) |
| Sidebar background | `--bg-surface` |
| Header background | `--bg-canvas` |

### 11.2 Sidebar

- Brand block at top (`.brand`) — wordmark + glow dot. **Identity, not navigation.** Sits OUTSIDE `<nav aria-label="Primary">` so the primary-nav landmark contains only route entries (a screen-reader landing on the landmark hears "Primary navigation: Dashboard, Channels, …" without the brand reading as a peer entry).
  - The brand MAY optionally link to a home route, but only if that URL is distinct from every primary-nav entry. If the brand and a nav item share a URL, both will auto-emit `aria-current="page"` on that route — two elements claiming current page is non-conformant. In that case, render the brand as a non-routed element (plain `<a href="/">` or no link at all). Phase 1 currently aliases `/` to dashboard, so brand-as-plain-anchor is the right choice today; see [PURA-37](/PURA/issues/PURA-37) for the route-disambiguation follow-up that lets brand return to SPA `Link`.
  - The brand is NOT a focusable nav target by default — operators reach the dashboard via the explicit Dashboard NavItem. Brand-as-link is a courtesy redundancy, not the primary affordance.
- Nav groups (`.nav-group`) with uppercase 11px label (`.nav-group-label`) and items (`.nav-item`). Phase 1 ships four groups: **Server**, **Moderation**, **Automation**, **Admin**.
- Active item: `--bg-selected` background, 2px leading-edge accent border, `--accent-fg` text. Hover: `--bg-hover`.
- Group label is decoration only — not focusable, not announced as a heading. The `<nav aria-label="Primary">` landmark wraps the nav-groups + items only.
- Active-route indication relies on the SPA router's automatic `aria-current="page"` (Dioxus `Link` handles this). Do NOT pass an explicit `aria-current` prop on top of a router-aware Link — duplicate emission is a real a11y bug, not a hypothetical one.
- Mobile (≤768px): sidebar hides, replaced by hamburger that opens it as a left-edge drawer over a backdrop. Drawer width = expanded width (240px), sheet animation `--motion-base var(--ease-standard)`.

### 11.3 Header

- Slot order, left → right: hamburger (mobile only) · server-selector trigger · websocket dot · spacer · theme toggle · user menu · logout.
- Server-selector trigger is the `Dropdown` from §13 with the server-icon mark and chevron. Full spec lives in `server-selector.md`.
- Websocket indicator (`.ws-dot`) is a status pill without a label — green = connected, amber = reconnecting, red = disconnected. Hover/focus reveals a tooltip with the live state. Color is never the only signal — pair with the tooltip text.
- User menu trigger shows initials avatar + first name + caret. Opens a Dropdown with profile / settings / logout.
- Mobile: sidebar hides, server-selector moves into a dedicated `.mobile-selector-bar` row directly under the header so it stays a one-tap reach. Logout collapses into the user menu.

### 11.4 Acceptance

- Sidebar nav has visible focus ring (per §1.4) on every item, in tab order matching visual order.
- The active route is announced with `aria-current="page"` exactly once. The brand and any other chrome element MUST NOT also emit `aria-current` on the active route.
- Brand sits outside the `<nav aria-label="Primary">` landmark. The landmark contains only route entries.
- Sidebar is reachable via "skip to navigation" link from the header (the operator may need to jump back without ten tabs through main content).
- At 768px and below, hamburger opens the sidebar drawer, Escape closes it, focus returns to the hamburger trigger.

---

## 12. Banner / Alert (inline)

Form- or page-level inline message. NOT a toast. Banners stick to the surface they describe — login error above the form, server-add failure above the form, danger-zone warning at the top of a destructive section.

### 12.1 Spec

| Token | Value |
|---|---|
| Padding | `--space-5 --space-6` |
| Radius | `--radius-md` |
| Leading bar | 4px solid, color matches variant |
| Background | variant-bg at ~10% opacity over surface |
| Foreground | variant-fg |
| Icon | `--text-md`, semantic for variant |

### 12.2 Variants

| Variant | When | Class |
|---|---|---|
| `danger` | Submission failed; required state isn't safe to act on. | `.banner` |
| `warning` | Acceptable but risky (token expiring, action irreversible). | `.banner.banner-warning` |
| `info` | Neutral context (read-only mode, restored from cache). | `.banner.banner-info` (add) |
| `success` | Confirmed success that's load-bearing on the surface (e.g., "License activated"). Reserved — toast is usually right instead. | `.banner.banner-success` (add) |

### 12.3 Behavior

- `role="alert"` on `danger` and `warning`. `role="status"` on `info` / `success` so screen readers don't interrupt.
- Dismiss button only when content is purely informational. Errors stay on screen until the underlying state changes.
- Never use a banner where a `Field` error message belongs — banners are for state that affects the whole form/section, not a single input.

### 12.4 Lens references

- **Nielsen — visibility of system status**: pairs with the form, can't be missed.
- **Forgiveness**: errors stay until resolved; nothing dismisses on a timer.

---

## 13. Dropdown / Menu

Trigger + popover for choice lists. Used by: server selector (`server-selector.md`), user menu, table row actions, theme toggle when it grows beyond binary, dashboard time-range chooser. Modal is for confirmations and forms — NOT for a list of choices.

### 13.1 Anatomy

```
[trigger ▾]              ← button or selector pill
  └─[ menu ]
     ├─ filter (optional, for >7 items)
     ├─ section-label (optional)
     ├─ item · item · item
     └─ footer-actions (optional)
```

### 13.2 Trigger spec

- Reuses Button (`ghost` or `secondary`) OR the `.selector` pill (server selector). Always shows a trailing chevron (`▾`) to signal "opens a menu" — Norman's signifier.
- Pressing `Enter` / `Space` / `↓` opens the menu. Clicking outside closes it.

### 13.3 Menu spec

| Token | Value |
|---|---|
| Background | `--bg-surface-raised` |
| Border | `1px solid --border-subtle` |
| Radius | `--radius-md` |
| Padding | `--space-2` block / 0 inline |
| Shadow | `--shadow-md` |
| Min width | match trigger; max 320px |
| Item height | 32px (compact) or 40px (with avatar/icon) |
| Item padding | `--space-3 --space-5` |
| Active item bg | `--bg-hover` |
| Focused item bg | `--bg-selected` (keyboard focus shown explicitly) |

### 13.4 Behavior

- Single-select: closes on selection, focus returns to trigger.
- Multi-select: stays open until trigger click-out or `Esc`. Selected items get a leading checkmark.
- Filter input (when used): autofocuses on open, narrows the list with substring match, keyboard `↑/↓` navigates filtered results.
- Empty state: "No matches" centered, 8px vertical padding. No search-tip inception inside the menu.

### 13.5 Keyboard

- `↓` / `↑` move active item. `Enter` selects. `Esc` closes and returns focus to trigger. `Home` / `End` jump to first/last. Type-ahead jumps to first item beginning with the typed character.
- Active item has a visible focus ring AND an `aria-activedescendant` reference so AT picks it up.

### 13.6 When NOT to use

- For 2 mutually exclusive choices → Toggle / Switch instead.
- For a long catalog → searchable Combobox (Phase 2).
- For "do you really want to do this" → Modal (§3).

### 13.7 Reference markup

The trigger is whatever already-built control invokes the menu — for the server selector that's `.selector`, for the user menu it's a Button. The menu portal is the same shape regardless of trigger:

```html
<!-- Trigger lives in normal flow. -->
<button class="selector"
        id="server-selector-trigger"
        aria-haspopup="menu"
        aria-expanded="true"
        aria-controls="server-selector-menu">
  <span class="mark">⬢</span>
  <span class="label">My Community</span>
  <span class="chev">▾</span>
</button>

<!-- Menu is positioned by the host (portal or relative parent). -->
<div class="menu"
     id="server-selector-menu"
     role="menu"
     aria-labelledby="server-selector-trigger"
     aria-activedescendant="server-selector-item-2">
  <input class="menu-filter"
         type="text"
         placeholder="Filter servers…"
         aria-label="Filter servers">

  <div class="menu-section-label">Servers</div>

  <a class="menu-item is-rich"
     id="server-selector-item-1"
     role="menuitemradio"
     aria-checked="true">
    <span class="check">✓</span>
    <span class="label">My Community</span>
    <span class="meta">connected</span>
  </a>

  <a class="menu-item is-rich"
     id="server-selector-item-2"
     role="menuitemradio"
     aria-checked="false">
    <span class="check"></span>
    <span class="label">Tournament server</span>
    <span class="meta">reconnecting</span>
  </a>

  <div class="menu-divider" role="separator"></div>

  <button class="menu-item" role="menuitem">
    <span class="icn">＋</span>
    <span class="label">Add server</span>
  </button>
</div>
```

For an empty filter result, replace items with `<div class="menu-empty">No matches</div>`. For destructive items, add `is-danger` to `.menu-item`.

### 13.8 ARIA wiring contract

| Element | Required attributes |
|---|---|
| Trigger | `aria-haspopup="menu"`, `aria-expanded` toggled with state, `aria-controls` pointing at the menu's `id` |
| Menu container | `role="menu"`, `aria-labelledby` pointing at the trigger's `id`, `aria-activedescendant` pointing at the focused item's `id` while keyboard navigating |
| Single-select item | `role="menuitemradio"` + `aria-checked` |
| Multi-select item | `role="menuitemcheckbox"` + `aria-checked` |
| Plain action item | `role="menuitem"` |
| Filter input | own `aria-label` (it's not a `menuitem` — it's chrome) |
| Section label | not focusable; never a `role="heading"` (would split the menu's announce stream) |
| Divider | `role="separator"` |
| Disabled item | `aria-disabled="true"` (do NOT set `disabled`; it removes from focus order and breaks keyboard nav) |

Do not ship a Dropdown that omits `aria-activedescendant`. Type-ahead and arrow nav need it for screen-reader users to follow the focus.

### 13.9 Positioning contract

Position is the **host's** responsibility — the primitive does not decide. Concrete recipe for the server selector and user menu (matches the visible mock):

- Anchor to the trigger's bounding box.
- Place flush below by default, gap = `--space-2` (4px).
- Width: `max(trigger-width, 240px)`, capped at 320px (the menu's `max-width`). For a `.selector` trigger that's 200–300px wide, this is "match the trigger".
- Viewport-aware shift: if menu would overflow the bottom edge, flip to above. If it would overflow right (mobile, RTL), shift left so the right edge aligns with the trigger's right edge.
- On mobile (≤480px), prefer the **sheet variant** — slide up from the bottom edge, full width, max-height 60vh, with a 4px top accent bar (use `--bg-surface-raised`, no border-radius on the bottom). The bot-flow drawer pattern in `bot-canvas-brief.md` §4.2 is the same idea.

Implementation freedom: a hand-rolled `getBoundingClientRect`-based positioner is fine for Phase 1. A fancier `floating-ui` port comes when more menus arrive — Phase 2 cleanup.

### 13.10 Rust component contract (for DioxusLead)

```rust
#[derive(Clone, PartialEq, Eq)]
pub enum MenuItemKind {
    Action,
    Radio { checked: bool },
    Checkbox { checked: bool },
}

#[component]
pub fn Dropdown(
    /// Element id for the trigger; used for aria-controls / aria-labelledby.
    trigger_id: String,
    /// Element id for the menu portal.
    menu_id: String,
    /// Open/closed state. Owned by the host so it can be controlled.
    open: Signal<bool>,
    /// Trigger node (e.g., a Button or a `.selector` pill). Caller wires
    /// `aria-haspopup`, `aria-expanded`, `aria-controls` from the host
    /// using the ids above — Dropdown does NOT mutate the trigger.
    trigger: Element,
    /// Menu body. Compose with Menu / MenuItem / MenuSection / etc.
    children: Element,
    /// Optional placement override; default = below trigger, viewport-aware.
    #[props(default)] placement: MenuPlacement,
    /// Closes when focus leaves; default true. Off for sticky multi-select.
    #[props(default = true)] close_on_select: bool,
) -> Element { /* … */ }

#[component]
pub fn Menu(
    /// Active item id for `aria-activedescendant`. None = no keyboard cursor yet.
    active_id: Option<String>,
    /// Menu node id; matches Dropdown's `menu_id`.
    id: String,
    /// `aria-labelledby` target — usually Dropdown's `trigger_id`.
    labelled_by: String,
    children: Element,
) -> Element { /* … */ }

#[component]
pub fn MenuItem(
    id: String,
    kind: MenuItemKind,
    #[props(default)] disabled: bool,
    #[props(default)] danger: bool,
    #[props(default)] rich: bool,           // 40px height
    #[props(default)] onselect: EventHandler<()>,
    children: Element,
) -> Element { /* … */ }

#[component]
pub fn MenuSection(label: String, children: Element) -> Element { /* … */ }

#[component]
pub fn MenuFilter(
    value: Signal<String>,
    #[props(default)] placeholder: Option<String>,
) -> Element { /* … */ }

#[component]
pub fn MenuDivider() -> Element { /* … */ }

#[component]
pub fn MenuEmpty(#[props(default = "No matches".into())] text: String) -> Element { /* … */ }

#[component]
pub fn MenuFooter(children: Element) -> Element { /* … */ }
```

State ownership: the **host** owns `open`, the active item id, and the filter value. The Dropdown primitive owns: outside-click detection, Escape handling, focus return on close, and the keyboard handler (`↑`/`↓`/`Home`/`End`/`Enter`/type-ahead).

The keyboard handler updates `active_id` (a signal the host controls — pass it down via context) so screen readers see `aria-activedescendant` move. `Enter` calls the focused item's `onselect`. Items themselves don't need to be focusable HTML elements — `tabindex="-1"` on them keeps Tab-traversal sane.

### 13.11 Server-selector specialization

The server selector (`server-selector.md`) is a Dropdown with these specifics layered on:
- Trigger uses `.selector` (not Button).
- Items are radios (`MenuItemKind::Radio`) — single-select. Selected = current server.
- `MenuFilter` shown only when `servers.len() > 7` (per §13.4).
- `MenuFooter` contains a "Manage servers…" link routing to `/servers`.
- Optimistic switch: on select, close, fire route change, mark new active. On switch failure, surface a toast (`server-selector.md` §4.3).

The server-selector implementation can compose Dropdown directly — no need for a one-off pill.

---

## 14. Spinner (supporting primitive)

Inline spinner used inside Buttons (`is-loading`), small status indicators, and the `is-pending` state in `Field`. CSS class: `.spinner`. Distinct from `Skeleton` (§9): spinner = "this one thing is busy"; skeleton = "this whole region is loading."

### 14.1 Spec

| Token | Value |
|---|---|
| Size | 16px (default), 12px (compact, `is-sm`) |
| Stroke | 2px |
| Track color | `--border-subtle` |
| Spinner color | `currentColor` (so it inherits Button text color) |
| Duration | `--motion-spinner` (1000ms) |
| Easing | `linear` |

### 14.2 Behavior

- Pair with text (`Loading…`, `Saving…`) wherever the spinner alone wouldn't be self-evident — recognition over recall.
- Hide for `prefers-reduced-motion` users by swapping rotation for a 3-dot ellipsis pulse.

---

## 15. Step indicator

Used in the `/setup` wizard (3 steps) and reusable for any future ≤5-step linear flow. CSS class: `.steps`.

### 15.1 Spec

- Numbered circles (18px) with a 1px `--border-subtle` connector between them.
- States: `default`, `is-active` (filled `--accent-bg`, white numeral), `is-done` (filled `--success-bg`, checkmark glyph).
- Label below the circle for active step only on mobile, all steps on desktop.

### 15.2 Behavior

- Step indicators are NOT navigational — operator can't click ahead. Backward click to a completed step is allowed.
- `aria-current="step"` on the active step.
- Reduced motion: no shimmer / fade between steps; instant swap.

---

## 16. Layout / Spacing utilities

Token-backed page-level helpers. Live in `assets/layout.css`, loaded from `ui::App` alongside `tokens.css` and `components.css`. **Token-only — never invent pixel values; if a token doesn't exist, propose it as a system change rather than hard-coding.**

### 16.1 `.app-root`

Page-level wrapper that establishes the canvas surface, base text colour, and base font for any standalone route (login, error, setup wizard) that isn't mounted inside the §11 chrome.

| Prop | Value |
|---|---|
| `min-height` | `100vh` |
| `background` | `--bg-canvas` |
| `color` | `--text-primary` |
| `font-family` | `--font-sans` |

### 16.2 `.stack-{xs,sm,md,lg}` — vertical-rhythm stacks

Flex columns whose `gap` is the *only* vertical-rhythm knob. Use these instead of margin-stacks on siblings so rhythm stays a single source of truth.

| Class | `gap` token | Pixel value | Typical use |
|---|---|---|---|
| `.stack-xs` | `--space-4` | 12px | Dense list rows, label → control inside a tight field |
| `.stack-sm` | `--space-5` | 16px | Form rows inside a field group, list items in a card |
| `.stack-md` | `--space-6` | 24px | Section content inside a card (heading → paragraph → form) |
| `.stack-lg` | `--space-7` | 32px | Top-level page sections (hero → body → footer) |

All four classes share the same display contract: `display: flex; flex-direction: column;`. Compose with any other utility — `.stack-md` does not assume a parent.

### 16.3 `.login-page` + `.login-card`

Centred single-column auth surface. `.login-page` is the viewport-filling outer that vertically and horizontally centres its child; `.login-card` is the bounded card itself. Pair the card class on the inner stack so the utility surface stays flat:

```html
<div class="app-root login-page">
  <section class="stack-md login-card">
    <h1>Sign in</h1>
    <p>…</p>
    <!-- form -->
  </section>
</div>
```

| `.login-page` | Value |
|---|---|
| `display` | `grid` |
| `place-items` | `center` |
| `padding` | `--space-7 --space-5` |
| `min-height` | `100vh` |

| `.login-card` | Value |
|---|---|
| `width` | `min(420px, 100%)` |
| `padding` | `--space-7` |
| `background` | `--bg-surface-raised` |
| `border` | `1px solid --border-subtle` |
| `border-radius` | `--radius-lg` |
| `box-shadow` | `--shadow-md` |

`.login-page h1` and `.login-page p` get a single typography rule each so the heading + helper paragraph have correct tone (semibold `--text-xl`/`--lh-lg` for the heading, `--text-secondary` colour for the paragraph) without any inline styles.

### 16.4 Layout targets

- **Desktop 1440x900** — card centred horizontally and vertically, max width 420px, comfortable space below for future "Forgot password?" links.
- **Mobile 390x844** — card is effectively full-bleed minus `--space-5` side padding, vertical centring preserved, no horizontal scroll.
- **Reduced motion** — these utilities ship no animations; rely on the `prefers-reduced-motion` block in `tokens.css`.

### 16.5 When NOT to use these

- Inside the §11 chrome (`.app .main`). The chrome already owns the canvas/font/colour for the dashboard surfaces. Stacks remain useful for vertical rhythm there, but `.app-root` and `.login-page` are only for routes that opt out of the chrome.
- For horizontal layouts. Stacks are flex-column only by design — pair with `--space-*` tokens directly when you need a row.

---

## 17. Component naming for DioxusLead

Suggested Rust component names, for handoff:

| Spec section | Dioxus component |
|---|---|
| §1 Button | `Button { variant, size, loading, disabled, .. }` |
| §2 Input | `TextInput`, `NumberInput`, `PasswordInput` |
| §3 Modal | `Modal { size, kind, .. }`, `ConfirmModal`, `DestructiveConfirmModal` |
| §4 Table row | `Table`, `TableRow`, `TableHeader`, `TableCell` |
| §5 Form field | `Field { label, helper, error, required, optional, .. }` |
| §6 Toast | `Toast`, `ToastRegion` (singleton at app root), `useToast()` |
| §7 Empty state | `EmptyState { variant, title, description, actions, .. }` |
| §8 Tag | `Tag`, `StatusPill` |
| §9 Skeleton | `Skeleton`, `SkeletonText`, `SkeletonAvatar` |
| §10 Card | `Card { variant, header?, footer? }` |
| §11 Chrome | `AppShell`, `Sidebar`, `NavGroup`, `NavItem`, `Header`, `WebsocketIndicator`, `UserMenu` |
| §12 Banner | `Banner { variant, dismissible? }` |
| §13 Dropdown | `Dropdown`, `Menu`, `MenuItem`, `MenuSection`, `MenuFilter`, `MenuDivider`, `MenuEmpty`, `MenuFooter` (full prop signatures in §13.10) |
| §14 Spinner | `Spinner { size }` |
| §15 Steps | `Steps`, `Step { state, label }` |
| §18 Switch | `Switch { label, checked, disabled, onchange }` |
| §19 Tabs | `Tabs { tabs, active, onselect }`, `TabPanel`, `TabItem` |

---

## 18. Switch / Toggle

2-state boolean control. CSS class: `.switch`. Used for every boolean (`b_*`) permission row and the boolean group settings in the moderation group editors, and for any future on/off setting. For a choice between two *mutually exclusive labelled options* (not on/off), use a segmented control or radios — not a Switch (§13.6).

### 18.1 Spec

| Token | Value |
|---|---|
| Role | `role="switch"` on a `<button>`; `aria-checked` reflects state |
| Hit target | `--touch-min` (40px) min-height on the whole row |
| Track (off) | `--c-neutral-300` |
| Track (on) | `--accent-bg` |
| Thumb | `--text-primary`, `--shadow-sm`, 16px circle |
| Track size | 36×20px, `--radius-full` |
| Label | always present, to the **left** of the track |
| Slide | `transform` over `--motion-fast`, `--ease-standard` |
| Focus | `--shadow-focus` on the track |
| Disabled | `opacity: 0.5`, `cursor: not-allowed` |

### 18.2 Anatomy

The whole row is one `<button role="switch">`: the visible label is the button's content, so the label *is* the accessible name and clicking anywhere on the row toggles. The track + thumb are decorative spans (`aria-hidden`).

No "pure white" token exists, so the thumb uses `--text-primary` — it contrasts both the off track (`--c-neutral-300`) and the on track (`--accent-bg`) in light and dark themes, and reads as a solid knob. If a dedicated `--switch-thumb` surface token is later added, swap to it.

### 18.3 Behavior

- `<button>` answers Space and Enter natively, so `role="switch"` needs no extra keyboard handler.
- Reduced motion: the `prefers-reduced-motion` block in `tokens.css` collapses every transition to ~0ms, so the thumb swaps instantly with no slide. No component-level override needed.
- The label must read as a name on its own (e.g. "Can kick clients", not "Kick").

### 18.4 Rust component contract (for DioxusLead)

```rust
#[component]
pub fn Switch(
    /// Visible label; doubles as the button's accessible name.
    label: String,
    /// Current on/off state. Host-owned — Switch never mutates it.
    checked: bool,
    #[props(default)] disabled: bool,
    /// Optional element id, so a Field label can target the control.
    #[props(default)] id: Option<String>,
    /// Optional aria-describedby target (helper / error text id).
    #[props(default)] described_by: Option<String>,
    /// Fired with the requested next state.
    #[props(default)] onchange: EventHandler<bool>,
) -> Element { /* … */ }
```

State ownership: the **host** owns `checked`. `Switch` reports the requested next value through `onchange` and the host writes its signal.

---

## 19. Tabs

Horizontal underline tab bar for splitting a detail surface into sections — built for the moderation group-detail Permissions / Members / Settings split, reusable for any ≤~6-tab detail page. CSS classes: `.tabs`, `.tab`, `.tab-panel`.

### 19.1 Spec

| Token | Value |
|---|---|
| Roles | `role="tablist"` / `tab` / `tabpanel` |
| Tab font | `--weight-medium` `--text-sm` |
| Tab color | `--text-secondary`, `--text-primary` on hover |
| Active tab | `--accent-fg` text, 2px `--accent-fg` underline |
| Tablist border | 1px `--border-subtle` bottom edge |
| Tab focus | 2px `--accent-bg` outline, `-2px` offset |
| Tabindex | roving — active tab `0`, others `-1` |

### 19.2 Behavior

- **Automatic activation**: arrow keys move the cursor *and* select the tab it lands on (WAI-ARIA "tabs with automatic activation"). Suits a detail page where every panel is cheap to mount.
- Keyboard: `←` / `→` move with wrap, `Home` / `End` jump to first / last. Tab into the bar lands on the active tab (roving tabindex).
- `aria-selected="true"` on the active tab; `aria-controls` points at its panel; the panel's `aria-labelledby` points back.
- Inactive panels stay in the DOM with the `hidden` attribute so the `aria-controls` target always resolves.

### 19.3 Rust component contract (for DioxusLead)

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct TabItem { pub id: String, pub label: String }

#[component]
pub fn Tabs(
    /// Tab descriptors, in display order.
    tabs: Vec<TabItem>,
    /// Currently-selected tab id. Host-owned.
    active: String,
    /// Id stem shared with the paired TabPanels; default "tabs".
    #[props(default = String::from("tabs"))] id: String,
    /// Optional aria-label for the tablist.
    #[props(default)] aria_label: Option<String>,
    /// Fired with the requested tab id on click or arrow-key move.
    onselect: EventHandler<String>,
) -> Element { /* … */ }

#[component]
pub fn TabPanel(
    /// Tab id this panel belongs to — matches a TabItem::id.
    id: String,
    /// Id stem — must match the paired Tabs `id`; default "tabs".
    #[props(default = String::from("tabs"))] tabs_id: String,
    /// Whether this panel's tab is selected.
    active: bool,
    children: Element,
) -> Element { /* … */ }
```

State ownership: the **host** owns `active`. The host renders one `TabPanel` per tab and drives each panel's `active`. `Tabs` reports the requested selection through `onselect` and never mutates it.

All components accept a `class` / `style` prop (escape hatch) but must NOT require it for any documented use case.
