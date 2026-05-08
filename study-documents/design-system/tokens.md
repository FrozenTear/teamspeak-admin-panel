# TS6 Manager — Design Tokens

Foundation for the Dioxus design system. Every component, surface, and theme in the operator SPA derives from this token set. DioxusLead consumes these as CSS custom properties (referenced from a single `:root`/`[data-theme]` block) and re-exports typed Rust constants where component code needs the value at compile time.

**Cleanroom note.** Token values were chosen from first principles against the design lenses below; no reference-repo styles were consulted. Where a value matches industry convention (e.g., `4px` spacing baseline) it is because the convention is justified by the lens, not because it was copied.

**Mode strategy.** Dark is the default — operator workflow is long-dwell, often run on a second monitor next to a TS6 voice client (also dark). Light mode is supported per Ch. 28.2 theme toggle. Every semantic token resolves through the active mode; only the *raw palette* below differs between modes.

---

## 1. Color

### 1.1 Raw palette — dark mode (default)

Hue families pinned to specific HSL ramps. Steps named `0..900` so future additions slot in without renumbering.

| Token | Hex | HSL | Role |
|---|---|---|---|
| `--c-neutral-0` | `#0B0E13` | `hsl(216 26% 6%)` | Canvas (lowest) |
| `--c-neutral-50` | `#11151C` | `hsl(217 25% 9%)` | Surface (default) |
| `--c-neutral-100` | `#171C26` | `hsl(220 24% 12%)` | Surface raised |
| `--c-neutral-200` | `#1E2533` | `hsl(220 26% 16%)` | Surface raised++ / hovered |
| `--c-neutral-300` | `#2A3344` | `hsl(220 24% 22%)` | Borders |
| `--c-neutral-400` | `#3D485E` | `hsl(220 21% 30%)` | Borders strong / disabled fg |
| `--c-neutral-500` | `#828FAA` | `hsl(220 18% 59%)` | Muted text (lifted PURA-63 from `#737F98` so AA 4.5:1 also clears on `--bg-surface-raised`; see §1.1 contrast table for ramp-ordering note) |
| `--c-neutral-600` | `#7E8AA2` | `hsl(220 17% 57%)` | Secondary text |
| `--c-neutral-700` | `#A4ADC2` | `hsl(220 18% 70%)` | Body text (low-contrast) |
| `--c-neutral-800` | `#D5DAE5` | `hsl(220 17% 87%)` | Body text |
| `--c-neutral-900` | `#F2F4F8` | `hsl(220 18% 96%)` | High-contrast text / inverse fg |

**Brand / accent — "Beacon Cyan."** Distinct from TeamSpeak's traditional royal-blue identity to avoid implying official affiliation, while still reading as "communication / signal."

| Token | Hex | Role |
|---|---|---|
| `--c-accent-50` | `#06303F` | Accent bg (subtle / chip) |
| `--c-accent-100` | `#0A4A63` | Accent bg (hover on subtle) |
| `--c-accent-300` | `#1488B6` | Accent bg (button hover) |
| `--c-accent-500` | `#1FA8DC` | Accent bg (button default) |
| `--c-accent-600` | `#3CBEEF` | Accent fg on dark / link |
| `--c-accent-700` | `#7AD2F2` | Accent fg (low-emphasis) |
| `--c-accent-focus` | `#3CBEEF` | Focus ring (alpha 0.55 in use) |

**Status colors.**

| Token | Hex | Role |
|---|---|---|
| `--c-success-500` | `#2EB872` | Success bg / icon |
| `--c-success-600` | `#5DD299` | Success fg on dark |
| `--c-warning-500` | `#E0A93B` | Warning bg / icon |
| `--c-warning-600` | `#F2C66B` | Warning fg on dark |
| `--c-danger-500` | `#E64C5F` | Danger bg / icon |
| `--c-danger-600` | `#F2828F` | Danger fg on dark |
| `--c-info-500` | `#4F8AE0` | Info bg / icon |
| `--c-info-600` | `#86AEEA` | Info fg on dark |

**Contrast verification** (WCAG AA, 4.5:1 for body text, 3:1 for large text and non-text UI):

- `--c-neutral-800` on `--c-neutral-50`: **13.2:1** — passes AAA body
- `--c-neutral-700` on `--c-neutral-50`: **8.4:1** — passes AAA body
- `--c-neutral-600` on `--c-neutral-50`: **5.5:1** — passes AA body (use for secondary)
- `--c-neutral-500` on `--c-neutral-0` (canvas): **5.94:1** — passes AA body (helper / meta on canvas)
- `--c-neutral-500` on `--c-neutral-50` (surface): **5.62:1** — passes AA body (helper inside cards)
- `--c-neutral-500` on `--c-neutral-100` (raised): **5.25:1** — passes AA body (helper inside modals / dropdowns / KPI cards)
- `--c-neutral-600` on `--c-neutral-100` (raised): **4.91:1** — passes AA body (secondary on raised)
- `--c-accent-600` on `--c-neutral-50`: **6.8:1** — passes AA body, links readable
- `--c-danger-600` on `--c-neutral-50`: **5.7:1** — passes AA body for inline error text
- White on `--c-accent-500`: **3.6:1** — passes AA large (button labels are ≥14px bold, qualifies)

`--c-neutral-500` (PURA-63 lift, `#828FAA`) now clears AA body on every neutral background up to and including `--bg-surface-raised` — the workhorse pairings for the operator chrome including dashboard KPI cards, modals, dropdowns, and popovers. The previous PURA-61 value `#737F98` cleared canvas (4.80:1) but failed on raised (4.24:1, AA-large only); axe flagged 12 nodes on `/dashboard`.

**Ramp ordering note.** At `#828FAA` (L≈59%) the muted-text token is now slightly *brighter* than `--c-neutral-600` `#7E8AA2` (L≈57%, 4.91:1 on raised). The ramp's "muted" → "secondary" → "body" semantic ordering still reads correctly in usage (muted tends to sit at 12px, secondary at 14px, so the size cue dominates), but the raw luminance ordering now inverts between 500 and 600. If a future component places muted and secondary text adjacent at the same size, this could read as a regression and would justify lifting `--c-neutral-600` in tandem (e.g., `#8B98B0`).

### 1.2 Raw palette — light mode

Mirror ramp; step `--c-neutral-0` maps to lightest, `--c-neutral-900` to darkest.

| Token | Hex |
|---|---|
| `--c-neutral-0` | `#FFFFFF` |
| `--c-neutral-50` | `#F7F9FC` |
| `--c-neutral-100` | `#EEF2F8` |
| `--c-neutral-200` | `#E1E7F1` |
| `--c-neutral-300` | `#CBD3E2` |
| `--c-neutral-400` | `#9AA4BC` |
| `--c-neutral-500` | `#6E7892` |
| `--c-neutral-600` | `#4F5970` |
| `--c-neutral-700` | `#363E50` |
| `--c-neutral-800` | `#1F2532` |
| `--c-neutral-900` | `#0E121C` |

Accent / status palettes are **shared** between modes — saturation tuned to read on both. The accent is the brand; it should not shift hue with mode.

### 1.3 Semantic color tokens

These are the only color tokens components reference. Components MUST NOT consume `--c-neutral-*` directly — that boundary keeps re-theming a one-file change.

| Token | Resolves to (dark) | Use |
|---|---|---|
| `--bg-canvas` | `--c-neutral-0` | Page / app background |
| `--bg-surface` | `--c-neutral-50` | Cards, panels, sidebar |
| `--bg-surface-raised` | `--c-neutral-100` | Modals, dropdowns, popovers |
| `--bg-surface-sunken` | `--c-neutral-100` | Inputs, code blocks |
| `--bg-hover` | `--c-neutral-200` | Hover on surface |
| `--bg-selected` | `--c-accent-50` | Selected row / nav item |
| `--border-subtle` | `--c-neutral-300` | Default 1px borders |
| `--border-strong` | `--c-neutral-400` | Emphasised dividers |
| `--text-primary` | `--c-neutral-800` | Body / values |
| `--text-secondary` | `--c-neutral-600` | Labels, captions |
| `--text-muted` | `--c-neutral-500` | Placeholder, helper |
| `--text-inverse` | `--c-neutral-0` | On accent / on danger fills |
| `--text-link` | `--c-accent-600` | Inline links |
| `--accent-fg` | `--c-accent-600` | Accent text & icons |
| `--accent-bg` | `--c-accent-500` | Primary button fill |
| `--accent-bg-hover` | `--c-accent-300` | Primary button hover |
| `--accent-subtle-bg` | `--c-accent-50` | Chip / tag bg |
| `--success-fg` | `--c-success-600` | Success text |
| `--success-bg` | `--c-success-500` | Success fill |
| `--warning-fg` | `--c-warning-600` | Warning text |
| `--warning-bg` | `--c-warning-500` | Warning fill |
| `--danger-fg` | `--c-danger-600` | Danger text |
| `--danger-bg` | `--c-danger-500` | Danger fill |
| `--focus-ring` | `--c-accent-focus` | 2px outer focus ring |

**Lens:** Norman's principles (mapping) — token names map intent to visual; components stay declarative.

---

## 2. Spacing

4px baseline. T-shirt scale up to 12.

| Token | Value | Use |
|---|---|---|
| `--space-0` | `0` | Reset |
| `--space-1` | `2px` | Hairline gap |
| `--space-2` | `4px` | Icon ↔ label, dense rows |
| `--space-3` | `8px` | Form field internal padding (Y) |
| `--space-4` | `12px` | Tight stack |
| `--space-5` | `16px` | Default stack between related elements |
| `--space-6` | `24px` | Section padding |
| `--space-7` | `32px` | Card padding (default) |
| `--space-8` | `48px` | Page padding (Y) |
| `--space-9` | `64px` | Hero spacing |
| `--space-10` | `96px` | Large section breaks |

**Why 4px?** Material's 4dp baseline aligns to the most common screen densities and lets `--space-5` (16px) match the default browser font size, so `1rem === --space-5`. This means a single mental conversion between "rem-thinking" type and "px-thinking" layout. Citation: Material 3 spacing system; NN/g spacing rhythm guidance.

**Lens:** Gestalt — Proximity. The scale's geometric jumps (especially `--space-5 → --space-6 → --space-7`) reinforce visible groupings rather than fighting them.

---

## 3. Type

### 3.1 Family

```
--font-sans:    "Inter", "Inter Variable", system-ui, -apple-system,
                "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
--font-mono:    "JetBrains Mono", "Fira Code", ui-monospace,
                "SF Mono", Menlo, Consolas, monospace;
```

Inter chosen for its open apertures at small sizes (operator UI is information-dense) and excellent WOFF2 size. System fallback ensures no FOIT before Inter loads. Mono is reserved for: tokens, channel/client IDs, log lines, JSON previews.

**Loading strategy:** self-host Inter (`Inter-Variable.woff2`, `font-display: swap`) so CSP can be tightened (no Google Fonts dependency, no third-party request). Fallback metric overrides via `@font-face size-adjust` so layout doesn't reflow on font load.

### 3.2 Scale

Modular scale **1.200 (minor third)** — confident but not theatrical. Base 14px (data-density baseline for an admin tool — denser than a marketing site, looser than a spreadsheet).

| Token | Size | Line height | Letter-spacing | Use |
|---|---|---|---|---|
| `--text-2xs` | `11px` | `14px` | `0.02em` | Badges, micro-labels |
| `--text-xs` | `12px` | `16px` | `0.01em` | Helper text, table meta |
| `--text-sm` | `13px` | `18px` | `0` | Dense table rows |
| `--text-base` | `14px` | `20px` | `0` | Body |
| `--text-md` | `16px` | `22px` | `-0.005em` | Form inputs (touch-friendlier) |
| `--text-lg` | `18px` | `26px` | `-0.01em` | Section headers |
| `--text-xl` | `22px` | `30px` | `-0.015em` | Page titles |
| `--text-2xl` | `28px` | `36px` | `-0.02em` | Wizard step titles |
| `--text-3xl` | `36px` | `44px` | `-0.025em` | Empty-state hero |

### 3.3 Weight

| Token | Value | Use |
|---|---|---|
| `--weight-regular` | `400` | Body, labels |
| `--weight-medium` | `500` | Emphasised body, button labels |
| `--weight-semibold` | `600` | Headers H3+ |
| `--weight-bold` | `700` | H1, key metrics |

Avoid mixing >2 weights in one viewport. **Lens:** Aesthetic-Usability Effect — restraint reads as polish.

### 3.4 Reading constraints

- Form inputs use `--text-md` (16px). Reasoning: iOS Safari triggers zoom on focus when input font-size <16px, and operators on tablets shouldn't fight the viewport.
- Inline links underlined by default (`text-decoration-thickness: 1px; text-underline-offset: 2px`). Color alone is insufficient (WCAG 1.4.1 — color independence).
- Reading width capped at `64ch` for prose blocks; UI rows are not constrained.

---

## 4. Radii

| Token | Value | Use |
|---|---|---|
| `--radius-0` | `0` | Tables, sharp dividers |
| `--radius-sm` | `2px` | Tags, pills inside dense rows |
| `--radius-md` | `6px` | **Default** — buttons, inputs, cards |
| `--radius-lg` | `10px` | Modals, popovers |
| `--radius-xl` | `16px` | Hero containers |
| `--radius-full` | `9999px` | Avatars, status dots, segmented controls |

`--radius-md: 6px` is the default. 6px is large enough to read as "modern soft" but small enough to keep a technical/operator feel — a 12px default would push toward "consumer app" territory inappropriate for a server-management product.

---

## 5. Shadows / elevation

Dark surfaces struggle with traditional drop shadows. We use **layered shadows** (a sharp inner-edge highlight + a soft ambient drop) so depth still reads against `--c-neutral-0`.

| Token | Value | Use |
|---|---|---|
| `--shadow-0` | `none` | Flat |
| `--shadow-sm` | `0 1px 0 rgba(255,255,255,0.04) inset, 0 1px 2px rgba(0,0,0,0.4)` | Cards |
| `--shadow-md` | `0 1px 0 rgba(255,255,255,0.04) inset, 0 4px 12px rgba(0,0,0,0.45)` | Dropdowns, popovers |
| `--shadow-lg` | `0 1px 0 rgba(255,255,255,0.06) inset, 0 12px 32px rgba(0,0,0,0.55)` | Modals |
| `--shadow-focus` | `0 0 0 2px var(--bg-canvas), 0 0 0 4px var(--focus-ring)` | Focus ring (offset variant) |

In **light mode**, the inset highlight is dropped and the ambient shadow uses `rgba(15,23,42,0.08..0.16)` instead.

**Lens:** Nielsen #1 (system status visibility) — elevation differentiates active modal/popover from background without animation.

---

## 6. Motion

| Token | Value | Use |
|---|---|---|
| `--motion-instant` | `0ms` | State swaps with no perceived transition (form validation badge swap) |
| `--motion-fast` | `100ms` | Hover, focus, button press feedback |
| `--motion-normal` | `180ms` | Drawer slide, dropdown reveal, tooltip |
| `--motion-slow` | `280ms` | Modal in/out, page-region swap |
| `--motion-slower` | `420ms` | Onboarding transitions only |
| `--ease-standard` | `cubic-bezier(0.2, 0, 0, 1)` | Default — fast out, settles |
| `--ease-emphasized` | `cubic-bezier(0.3, 0, 0, 1)` | Bot-flow canvas node drop |
| `--ease-exit` | `cubic-bezier(0.4, 0, 1, 1)` | Modal/menu dismiss |

**Doherty Threshold (<400ms)** — every transition under 400ms keeps the operator in flow. `--motion-slower` exceeds this only for setup-wizard step transitions, where the larger motion is itself the wayfinding.

**Reduced motion.** All transitions wrap in:

```css
@media (prefers-reduced-motion: reduce) {
  *,
  *::before,
  *::after {
    animation-duration: 0.01ms !important;
    transition-duration: 0.01ms !important;
  }
}
```

State changes still occur — only the animation is suppressed. Loading spinners replaced with static "Loading…" text.

---

## 7. Z-index

A short, named scale — never freeform `z-index: 9999`.

| Token | Value | Use |
|---|---|---|
| `--z-base` | `0` | Default |
| `--z-sticky` | `100` | Sticky table headers |
| `--z-dropdown` | `200` | Dropdowns, server selector menu |
| `--z-overlay` | `300` | Modal backdrop |
| `--z-modal` | `310` | Modal panel |
| `--z-popover` | `400` | Tooltips, popovers (above modal so help-in-modal works) |
| `--z-toast` | `500` | Toast region |

---

## 8. Layout primitives

| Token | Value | Use |
|---|---|---|
| `--sidebar-width-expanded` | `240px` | Default sidebar |
| `--sidebar-width-collapsed` | `64px` | Icons-only |
| `--header-height` | `56px` | Top chrome |
| `--content-max-width` | `1280px` | Centered content cap (table rows respect this) |
| `--touch-min` | `40px` | Minimum interactive target |
| `--touch-comfortable` | `44px` | iOS HIG guidance |

**Lens:** Fitts's Law — sidebar items, server-selector trigger, and primary actions all hit `--touch-comfortable` at minimum. Dense table-row controls are exempt (operator workflow, no thumb input expected).

---

## 9. Breakpoints

Desktop-first. Operator workflow lives at 1280–1920px; mobile is *responsive degradation*, not a primary surface.

| Token | Value | Notes |
|---|---|---|
| `--bp-sm` | `640px` | Phones landscape — sidebar collapses to drawer |
| `--bp-md` | `768px` | Tablets — sidebar can re-expand by tap |
| `--bp-lg` | `1024px` | Small laptop — sidebar always visible |
| `--bp-xl` | `1280px` | Standard operator viewport |
| `--bp-2xl` | `1600px` | Wide operator monitor |

Mobile (<640px) **out of scope** for: bot-flow canvas (Ch. 31), permission editor, file browser, server logs. These collapse to a "Open on a larger screen" empty state. Phase 1 surfaces (login, setup, dashboard) MUST be usable on mobile.

---

## 10. Dioxus integration contract

DioxusLead implements:

1. A single `tokens.css` (or scoped `<style>`) emitting all tokens above as CSS custom properties under `:root` and `[data-theme="light"]`.
2. A Rust module `crate::ui::tokens` re-exporting frequently-needed values as constants (e.g., `SPACE_5: u32 = 16` for canvas math).
3. A `<ThemeProvider>` at app root that toggles `data-theme` on `<html>` and persists the choice to UI-prefs storage (per Ch. 28.3).
4. A `useReducedMotion()` hook that returns the matchMedia value, so motion-heavy components can branch (the canvas, mainly).

Component code MUST consume semantic tokens only (Section 1.3), spacing/type/radius/shadow/motion tokens only by their scale name. Hex literals in component code are a code-review block.

---

## 11. Citations / lens references

- **Material Design 3 — Spacing system.** 4dp baseline justification.
- **NN/g — Visual Hierarchy** (Lukáš Polák, 2023). Type scale and weight restraint.
- **WCAG 2.2** — 1.4.3 contrast, 1.4.11 non-text contrast, 1.4.12 text spacing, 2.5.5 target size.
- **Apple HIG** — touch target 44×44.
- **Doherty Threshold** (Doherty & Thadani, 1982) — 400ms ceiling on perceived flow.
- **Inter typeface** (Rasmus Andersson) — open apertures, x-height optimised for UI.
- **Refactoring UI** (Adam Wathan, Steve Schoger, 2018) — modular type scale and color step rationale.
