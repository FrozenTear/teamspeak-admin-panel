# Flow engine v2 — canvas visual spec

- **Status:** delivered, [PURA-276](/PURA/issues/PURA-276) (UXDesigner half of the
  [PURA-267](/PURA/issues/PURA-267) pairing).
- **Companion:** [`ui-brief.md`](./ui-brief.md) §4–§8 — the surface this dresses.
- **Implementation:** `crates/ts6-manager-server/src/ui/pages/flows/canvas/style.rs`
  (tokenised sheet, landed) and `…/model.rs::PaletteKind::glyph()` (glyph set,
  landed). DioxusLead wires the pre-staged modifier classes — see §8.

Specifies in named design tokens and component classes, per `ui-brief.md` §7.
No freeform values: every literal in `style.rs` resolves from `tokens.css`.

## 1. Token foundation — what tokenising fixed

The PURA-267 placeholder pinned `#fff` backgrounds and `#1c2230` text — a
permanently-**light** surface inside a dark-first app (`tokens.css` `:root` is
dark; `[data-theme="light"]` is the override). It also used a blue accent
(`#4f7cff`) that is not the system cyan (`--accent-fg`). Tokenising makes the
canvas theme-aware and brings it onto the one accent. No layout/geometry
changed — this is a value substitution plus the a11y fixes in §4 and §5.

## 2. Node card

Anatomy: a **title bar** (kind glyph + label/title, draggable) over a **body**
(one-line summary; in overlay mode, the status row — §5). All seven kinds use
one card; they differ only by the family accent (§3) and glyph (§6).

| Slot | Token |
| ---- | ----- |
| Background | `--bg-surface-raised` |
| Border | `1px solid --border-strong`, `border-left: 3px solid` the family colour |
| Radius | `--radius-md` |
| Shadow | `--shadow-md` |
| Title bar bg | `color-mix(in srgb, <family> 14%, --bg-surface-raised)` — a faint kind tint; falls back to `--bg-surface-raised` on old parsers |
| Title type | `--text-sm` / `--lh-sm` / `--weight-semibold` / `--text-primary` |
| Body type | `--text-2xs` / `--lh-xs` / `--text-muted` |
| Glyph | `--text-md`, coloured the family colour, `aria-hidden` |

**States:** `selected` → `0 0 0 2px --accent-fg` ring (additive to `--shadow-md`);
`connect-src` → `0 0 0 2px --success-fg` ring; `:focus-visible` → 2px
`--focus-ring` outline at `+2px` offset. Selection ring and run-overlay border
(§5) coexist — different channels.

## 3. Kind families — three, not seven

Seven distinct hues would exceed the design system's ~5 semantic colours and
force red/amber into non-error roles, mis-signalling against WCAG 1.4.1 and
Norman's mapping. Instead the seven kinds group into **three role families**
(Gestalt similarity — same colour ⇒ same role; the glyph and the always-present
text label carry the specific kind):

| Family | Kinds | Colour | Class | Why |
| ------ | ----- | ------ | ----- | --- |
| Trigger | trigger | `--info-bg` (blue) | `--trigger` | The single entry node — "this is where it starts". |
| Effect | action, parallel, sub-flow | `--accent-bg` (cyan) | `--effect` | Side-effecting; these carry an `err` port. |
| Control | branch, delay, transform | `--text-secondary` (neutral) | `--control` | Shape the path/data; no side effects. |

Red and amber stay reserved for error semantics (`err` port, `errored` status,
err edge, validation). The family colour drives one CSS custom property,
`--fc-kind`, which feeds the card's left strip, the title-bar tint, and the
glyph colour. Palette chips carry the same `--fc-chip--{family}` modifier so a
chip's glyph colour previews the node it creates.

## 4. Palette chips & ports

**Chip** — `--bg-surface-raised` card, `--border-strong` border + 3px family
left strip, `--radius-md`, `cursor: grab` (drag affordance, Norman signifier).
Hover lifts the border to `--accent-fg`. **Disabled** (Trigger, once the graph
has one) → `opacity: .5` + `cursor: not-allowed`; the editor already supplies
the `title` "A flow has exactly one trigger" so the *why* is surfaced
(recognition over recall) — keep it.

**Ports** — the visible dot stays 12px, but the interactive `.fc-port` element
is now **24×24** (transparent, dot drawn via `::before`), meeting WCAG 2.5.8
target size on a pointer-first canvas. `in`/`out` ports are circles; hover
fills them `--accent-bg`.

The **`err` port** reads distinct three ways, none colour-only (WCAG 1.4.1):

1. **Shape** — a square (`border-radius: --radius-sm`), not a circle.
2. **Label** — the literal text "on error" (not the raw wire id `err`).
3. Colour — `--warning-fg` border — as reinforcement, never the sole cue.

DioxusLead: render the `.fc-port-label` for an `is_err` port as `"on error"`,
not `port.name`; the wire port id stays `"err"`.

## 5. Edges & run overlay

**Edges** — normal: `--text-muted` solid 2px. **Err edge**: `--warning-fg`,
`stroke-dasharray: 6 4` — the dash is a texture cue independent of colour.
Connect-preview: `--accent-fg` dashed.

**Run overlay** (`ui-brief.md` §5) — each of the five per-node statuses carries
a coloured card border **and** a glyph **and** a text label in the `.fc-node-status`
row; never colour alone:

| Status | Border | `--fc-status` | Glyph | Label |
| ------ | ------ | ------------- | ----- | ----- |
| running | `--accent-bg`, pulsing | `--accent-fg` | `⟳` | Running |
| ok | `--success-bg` | `--success-fg` | `✓` | Ok |
| errored | `--danger-bg` | `--danger-fg` | `✕` | Errored |
| skipped | (dim, `opacity .55`) | `--text-muted` | `↷` | Skipped |
| interrupted | `--border-strong` **dashed** | `--text-muted` | `‖` | Interrupted |

`skipped` (dim) and `interrupted` (dashed) are distinguished by treatment, not
just colour — both are neutral-coloured. The running pulse is a `box-shadow`
keyframe; under `prefers-reduced-motion: reduce` it collapses to a static 2px
ring (explicit override in `style.rs`, not a frozen animation frame). The five
glyphs are the v1.1 set (`../ui-brief.md` §7.1) verbatim — no second icon
language, and they are already proven in the live font stack.

## 6. Glyph audit

`ui-brief.md` §7's first picks were audited against the `--font-sans` stack
(Inter → `system-ui` fallbacks). Inter carries almost none of these symbols, so
they fall to the OS font — and self-hosters run a wide spread of Linux
distros. The audit therefore favours **text-presentation** codepoints from
**broadly-covered Unicode blocks** (Arrows U+21xx, Geometric Shapes U+25xx)
over emoji-presentation or rare blocks. Resolved set (landed in `model.rs`):

| Kind | §7 pick | Verdict | Resolved | Reason |
| ---- | ------- | ------- | -------- | ------ |
| Trigger | `⚡` U+26A1 | **swap** | `↯` U+21AF | U+26A1 is emoji-presentation — renders as inconsistent colour emoji. `↯` (zigzag arrow) is text, Arrows block. |
| Action | `»` U+00BB | keep | `»` | Latin-1; universal. |
| Branch | `⑂` U+2442 | **swap** | `⋔` U+22D4 | U+2442 (OCR block) has near-zero coverage → tofu. `⋔` pitchfork reads as a fork, Math Operators. |
| Parallel | `⇉` U+21C9 | keep | `⇉` | Arrows block; broad coverage. |
| Delay | `⏱` U+23F1 | **swap** | `◷` U+25F7 | U+23F1 is emoji-presentation. `◷` (circle, upper-right quadrant) reads as a clock face, Geometric Shapes. |
| Transform | `⇄` U+21C4 | keep | `⇄` | Arrows block; broad coverage. |
| Sub-flow | `⧉` U+29C9 | **swap** | `▣` U+25A3 | U+29C9 (Misc Math Symbols-B) has poor coverage → tofu. `▣` (square within a square) reads as a nested surface, Geometric Shapes. |

Action also reuses the v1.1 per-action-kind glyphs (`» ♪ ↗ ≡`) unchanged.
**QA gate:** a single render proves one machine; tofu is OS-font-dependent.
Confirm the resolved set on the live font stack across Linux (Noto/DejaVu),
macOS, and Windows in the canvas headless-probe / PURA-266 QA pass; treat any
tofu as a swap-back to another text-presentation codepoint in the same block.

## 7. Accessibility summary

- Theme-aware (was light-locked); contrast inherits the audited `tokens.css`
  ramp — spot-check `--text-muted` on `--bg-surface-raised` in QA.
- Port hit target raised to 24×24 (WCAG 2.5.8); `:focus-visible` rings added
  to chips, ports, toolbar buttons, inspector fields, small buttons.
- No colour-only signals: `err` port (shape + label), err edge (dash), run
  statuses (glyph + text + treatment).
- Glyphs are `aria-hidden`; the kind is always present as text.
- Pulse honours `prefers-reduced-motion`.

## 8. Hand-off — DioxusLead

Landed under PURA-276: `style.rs` (fully tokenised) and `model.rs` glyphs.

Pre-staged in `style.rs` but inert until markup catches up (PURA-267/266
integration pass):

1. **Family modifier** — append `fc-node--{family}` / `fc-chip--{family}` from
   the kind discriminant (`trigger`→`--trigger`; `action`/`parallel`/`subflow`→
   `--effect`; `branch`/`delay`/`transform`→`--control`). `NodeRender.css_class`
   becomes a `String`.
2. **Err port label** — render `.fc-port-label` as `"on error"` when `is_err`.
3. **Overlay status** — when the run overlay (PURA-266) is live, append
   `fc-node--{status}` and render the `.fc-node-status` row (glyph + label).

**Acceptance:** all seven kinds visually distinct by family + glyph; `err` port
reads distinct with colour removed (greyscale check); five run statuses
distinct with colour removed; no tofu on the live font stack; canvas renders
correctly in both themes at 1440×900. Verify on a `dx serve --release` render —
debug WASM is too large for headless QA.
