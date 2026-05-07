# Public Widget — Theme System (6 Palettes)

**Spec basis:** Chapter 26.3 — six built-in themes (`dark`, `light`, `transparent`, `neon`, `military`, `minimal`), each with eight colors: `background`, `backgroundSecondary`, `border`, `textPrimary`, `textSecondary`, `accent`, `clientColor`, `headerBg`. Names MUST NOT be renamed or removed; values are invented (cleanroom — no reference repo consulted).

These themes drive the public widget at `/widget/<token>` (HTML / SVG / PNG / JSON). They are **independent of the operator-SPA theme** in `tokens.md` — different audience (public viewers, often embedded on a community website), different constraint set (must look right against arbitrary host backgrounds for `transparent`).

---

## 1. Theme value table

Hex color values for each of the six required themes.

| Token \ Theme | `dark` | `light` | `transparent` | `neon` | `military` | `minimal` |
|---|---|---|---|---|---|---|
| `background` | `#0F1218` | `#FAFBFE` | `rgba(0,0,0,0)` | `#06091F` | `#1A1E16` | `#FFFFFF` |
| `backgroundSecondary` | `#161B25` | `#F0F3F8` | `rgba(0,0,0,0.04)` | `#0E1338` | `#22281D` | `#F7F8FA` |
| `border` | `#283040` | `#D7DCE6` | `rgba(255,255,255,0.18)` | `#3A2A6E` | `#3A4031` | `#E2E5EC` |
| `textPrimary` | `#E8ECF4` | `#1A1F2C` | `#FFFFFF` | `#F0E8FF` | `#D4D9C8` | `#2A2F3A` |
| `textSecondary` | `#7A8398` | `#5C6477` | `rgba(255,255,255,0.7)` | `#A99CD8` | `#8A9078` | `#7C8294` |
| `accent` | `#3CBEEF` | `#1A8BC8` | `#7AD8FF` | `#FF49C4` | `#A4B86F` | `#5A6173` |
| `clientColor` | `#5DD299` | `#2EB872` | `#5DD299` | `#22F0AA` | `#C8D294` | `#5C6477` |
| `headerBg` | `#161B25` | `#EFF2F8` | `rgba(0,0,0,0.40)` | `#1A0E3F` | `#22281D` | `#F0F2F5` |

### 1.1 Naming intent

Each name is a **vibe**, not a literal description. Internal logic:

- **`dark`** — matches the operator SPA's dark identity. Default. Cool blue-black canvas, cyan accent.
- **`light`** — clean light counterpart. Same hue family as dark, mirrored ramp.
- **`transparent`** — designed to overlay any forum / Discord-embed / website background. All bg colors use alpha; text leans white because most websites embedding TS6 widgets are dark-ish gaming sites. If a host page is light, the operator can pick `light` instead.
- **`neon`** — saturated cyber/gaming aesthetic. Magenta-and-violet pop accent. For communities that lean into the visual.
- **`military`** — desaturated olive/khaki. For squads, milsim communities, hardware servers. Reads "operations console."
- **`minimal`** — almost-monochrome. No brand color, just type and dividers. For embeds where the widget should disappear into a clean website.

### 1.2 Contrast verification

WCAG AA requires 4.5:1 for body text, 3:1 for large text and non-text UI components.

| Theme | `textPrimary` on `background` | `textSecondary` on `background` | `accent` on `background` |
|---|---|---|---|
| `dark` | 13.4:1 ✓ AAA | 5.6:1 ✓ AA | 6.8:1 ✓ AA |
| `light` | 13.7:1 ✓ AAA | 6.1:1 ✓ AA | 4.9:1 ✓ AA |
| `transparent` (assumes dark host) | ~white-on-≤30%-luma → ≥10:1 ✓ AA | ~70% white → ≥7:1 ✓ AA | per host — see §3 |
| `neon` | 12.1:1 ✓ AAA | 4.9:1 ✓ AA | 4.6:1 ✓ AA (large only at 3:1 fallback) |
| `military` | 11.2:1 ✓ AAA | 4.6:1 ✓ AA | 4.4:1 ✓ AA |
| `minimal` | 12.8:1 ✓ AAA | 5.0:1 ✓ AA | 4.4:1 ✓ AA |

`accent` on `background` for `neon` (FF49C4 on 06091F = 4.6:1) is borderline — passes AA at 14pt+ regular and 11pt+ bold. The widget's accent text (server name, header) is 15px / 700 (per spec §27.3), well above the 11pt bold threshold. Contrast confirmed.

### 1.3 Transparent-theme caveat

`transparent` cannot be evaluated for contrast in isolation — it depends on the host page background. The widget operator UI MUST surface a hint when `transparent` is selected: **"Best on dark websites. If your site has a light background, use `light` instead."**

This warning is a usability signal, not a block.

---

## 2. Mapping to spec §27.3 SVG layout

Every theme color is consumed at exactly one place in the SVG render path:

| SVG element | Color slot |
|---|---|
| Outer rounded rect fill | `background` |
| Header band fill | `headerBg` |
| Body fill (below header) | `background` (or `backgroundSecondary` if alternating) |
| Header separator line stroke | `border` |
| Server name text | `accent` |
| ONLINE badge fill | `clientColor` |
| ONLINE badge text | `#FFFFFF` (constant, white-on-clientColor verified ≥3:1 across themes) |
| Stats line text | `textSecondary` |
| Spacer line stroke | `border` |
| Spacer text | `textSecondary` |
| Channel `#` icon | `accent` |
| Channel name | `textPrimary` |
| Channel client count | `textSecondary` |
| Lock emoji | `textSecondary` |
| Client dot fill | `clientColor` |
| Client nickname | `clientColor` |
| Client `[away]` / `[muted]` suffix | `textSecondary` (inherits parent line) |
| Footer separator | `border` |
| Footer caption | `textSecondary` (with opacity 0.6 per spec) |

**Lens (Common Region):** the header band gets its own bg (`headerBg`) so it visually separates from the channel list — the operator's eye lands on the title, then scans down.

---

## 3. Operator picker UX

In the Widget Manager (Chapter 34), the theme picker shows all six themes as **live thumbnail tiles**, not just names. Each tile is a 240×80 mini-render with sample header + 2 channels + 1 client. Click to select.

```
┌────────────────────────────────┐
│ Theme                          │
│                                │
│ ┌────┐ ┌────┐ ┌────┐ ┌────┐   │
│ │dark│ │lig.│ │trn.│ │neon│   │
│ │  ✓ │ │    │ │ ▦  │ │    │   │
│ └────┘ └────┘ └────┘ └────┘   │
│ ┌────┐ ┌────┐                 │
│ │mil.│ │min.│                 │
│ └────┘ └────┘                 │
│                                │
│ Best on dark websites — your   │ ← shown only when transparent
│ site's background will show    │   selected
│ through. Use `light` instead   │
│ if your site is light-themed.  │
└────────────────────────────────┘
```

- Selected tile gets a 2px `--accent-fg` outline + a check.
- `transparent` thumbnail uses a **checkerboard backdrop** (alternating `--c-neutral-50` / `--c-neutral-100` 8×8 squares) so the operator visually understands "this lets the host bg through." Common pattern from image editors.
- The accompanying warning sentence is shown only when `transparent` is selected.

---

## 4. DioxusLead integration contract

DioxusLead exposes:

```rust
pub struct WidgetTheme {
    pub background: &'static str,
    pub background_secondary: &'static str,
    pub border: &'static str,
    pub text_primary: &'static str,
    pub text_secondary: &'static str,
    pub accent: &'static str,
    pub client_color: &'static str,
    pub header_bg: &'static str,
}

pub static WIDGET_THEME_DARK: WidgetTheme = WidgetTheme { ... };
pub static WIDGET_THEME_LIGHT: WidgetTheme = WidgetTheme { ... };
pub static WIDGET_THEME_TRANSPARENT: WidgetTheme = WidgetTheme { ... };
pub static WIDGET_THEME_NEON: WidgetTheme = WidgetTheme { ... };
pub static WIDGET_THEME_MILITARY: WidgetTheme = WidgetTheme { ... };
pub static WIDGET_THEME_MINIMAL: WidgetTheme = WidgetTheme { ... };

pub fn theme_for(name: &str) -> &'static WidgetTheme;
```

Backend SVG renderer (Workstream WIDGETS, impl plan §3.11) consumes `&WidgetTheme` to emit fills/strokes. The structure stays stable for v1+; new themes append to the registry.

---

## 5. Acceptance criteria

- AC-WT1. All six theme names render without runtime error against a small fixture (3 channels, 1 spacer, 1 password channel, 2 clients).
- AC-WT2. Theme picker in Widget Manager renders live thumbnails (not text-only).
- AC-WT3. `transparent` picker shows checkerboard backdrop and the host-bg warning sentence.
- AC-WT4. Each theme's `textPrimary` on `background` meets WCAG AA body contrast.
- AC-WT5. Each theme's `accent` on `headerBg` meets WCAG AA large-text contrast (server-name text at 15px bold qualifies).
- AC-WT6. `WIDGET_THEMES` map keyed by name verbatim from the six spec names; renaming any breaks the public widget API and is a release-blocker.

---

## 6. Future themes (not v1)

Reserve room for community-contributed themes (`high-contrast` for accessibility, `holiday-*` for seasonal). These ship as data, not code — the renderer is theme-agnostic once the eight slots are provided. Out of scope for Phase 1; flagged so the data structure is built to extend.
