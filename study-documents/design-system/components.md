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

## 10. Component naming for DioxusLead

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

All components accept a `class` / `style` prop (escape hatch) but must NOT require it for any documented use case.
