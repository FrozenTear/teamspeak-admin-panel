//! Canvas CSS — tokenised against the TS6 Manager design system.
//!
//! Replaces the PURA-267 placeholder sheet. Every colour, space, radius,
//! type, shadow and motion value resolves from a `tokens.css` custom
//! property, so the canvas follows the operator's theme instead of being
//! hard-pinned to a light palette (the placeholder shipped `#fff`/`#1c2230`
//! literals — it was a permanently-light surface inside a dark-first app).
//!
//! Authored under [PURA-276](/PURA/issues/PURA-276); spec of record is
//! `docs/flows/v2/canvas-visual-spec.md`.
//!
//! Three rule groups below are **pre-staged** — the CSS is ready but the
//! matching markup lands with DioxusLead's PURA-267/266 integration pass:
//!   * `.fc-{node,chip}--{trigger,effect,control}` — per-kind family tint;
//!   * `.fc-node--{running,ok,errored,skipped,interrupted}` — run overlay;
//!   * `.fc-node-status` — the in-card overlay status row.
//!
//! They are inert (no selector matches) until then; harmless dead CSS.
//!
//! Scoped under `.fc-*` so it cannot leak into app chrome.

/// The canvas stylesheet, injected once per editor mount via a `<style>`
/// element. Resolves `var(--*)` against the globally-loaded `tokens.css`.
pub const CANVAS_CSS: &str = r#"
.fc-editor { display: grid; grid-template-columns: 184px 1fr 280px;
  height: calc(100vh - 168px); min-height: 460px;
  border: 1px solid var(--border-subtle); border-radius: var(--radius-lg);
  overflow: hidden; background: var(--bg-surface);
  font-family: var(--font-sans); color: var(--text-primary); }
.fc-editor.read-only .fc-palette, .fc-editor.read-only .fc-toolbar { display: none; }

/* ── palette ───────────────────────────────────────────────── */
.fc-palette { padding: var(--space-4) var(--space-3);
  border-right: 1px solid var(--border-subtle);
  background: var(--bg-surface); overflow-y: auto; }
.fc-pane-title { margin: 0 0 var(--space-4);
  font-size: var(--text-2xs); line-height: var(--lh-2xs);
  font-weight: var(--weight-semibold); text-transform: uppercase;
  letter-spacing: .05em; color: var(--text-secondary); }

.fc-chip { --fc-kind: var(--text-secondary);
  display: flex; flex-direction: column; gap: var(--space-1);
  width: 100%; margin-bottom: var(--space-3); padding: var(--space-3);
  border: 1px solid var(--border-strong); border-left: 3px solid var(--fc-kind);
  border-radius: var(--radius-md); background: var(--bg-surface-raised);
  cursor: grab; text-align: left;
  transition: border-color var(--motion-fast) var(--ease-standard); }
.fc-chip:hover:not(:disabled) { border-color: var(--accent-fg); }
.fc-chip:focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 1px; }
.fc-chip:disabled { opacity: .5; cursor: not-allowed; }
.fc-chip--trigger { --fc-kind: var(--info-bg); }
.fc-chip--effect  { --fc-kind: var(--accent-bg); }
.fc-chip--control { --fc-kind: var(--text-secondary); }
.fc-chip-head { display: flex; align-items: center; gap: var(--space-3);
  font-size: var(--text-sm); line-height: var(--lh-sm);
  font-weight: var(--weight-semibold); color: var(--text-primary); }
.fc-chip-desc { font-size: var(--text-2xs); line-height: var(--lh-2xs);
  color: var(--text-muted); }
.fc-glyph { font-size: var(--text-md); line-height: 1; color: var(--fc-kind); }

/* ── canvas surface ────────────────────────────────────────── */
.fc-canvas-wrap { position: relative; overflow: hidden; touch-action: none;
  background: var(--bg-canvas)
    radial-gradient(var(--border-subtle) 1px, transparent 1px);
  background-size: 22px 22px; }
.fc-canvas-wrap.panning { cursor: grabbing; }
.fc-world { position: absolute; inset: 0; transform-origin: 0 0; }

/* ── edges ─────────────────────────────────────────────────── */
.fc-edges { position: absolute; left: 0; top: 0; overflow: visible;
  pointer-events: none; }
.fc-edge { fill: none; stroke: var(--text-muted); stroke-width: 2; }
/* err edge — dashed is a texture cue, not colour alone (WCAG 1.4.1). */
.fc-edge.err { stroke: var(--warning-fg); stroke-dasharray: 6 4; }
.fc-edge.preview { stroke: var(--accent-fg); stroke-width: 2;
  stroke-dasharray: 6 4; }

/* ── node card ─────────────────────────────────────────────── */
.fc-node { --fc-kind: var(--text-secondary);
  position: absolute; background: var(--bg-surface-raised);
  border: 1px solid var(--border-strong);
  border-left: 3px solid var(--fc-kind); border-radius: var(--radius-md);
  box-shadow: var(--shadow-md);
  transition: box-shadow var(--motion-fast) var(--ease-standard); }
.fc-node--trigger { --fc-kind: var(--info-bg); }
.fc-node--effect  { --fc-kind: var(--accent-bg); }
.fc-node--control { --fc-kind: var(--text-secondary); }
.fc-node:focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 2px; }
.fc-node.selected { box-shadow: var(--shadow-md), 0 0 0 2px var(--accent-fg); }
.fc-node.connect-src { box-shadow: var(--shadow-md), 0 0 0 2px var(--success-fg); }

.fc-node-head { display: flex; align-items: center; gap: var(--space-3);
  padding: var(--space-3) var(--space-4); cursor: grab;
  font-size: var(--text-sm); line-height: var(--lh-sm);
  font-weight: var(--weight-semibold); color: var(--text-primary);
  border-bottom: 1px solid var(--border-subtle);
  border-radius: var(--radius-md) var(--radius-md) 0 0;
  background: var(--bg-surface-raised);
  background: color-mix(in srgb, var(--fc-kind) 14%, var(--bg-surface-raised)); }
.fc-node-title { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.fc-node-body { padding: var(--space-3) var(--space-4);
  font-size: var(--text-2xs); line-height: var(--lh-xs); color: var(--text-muted); }
/* overlay status row — glyph + text, never colour alone (ui-brief.md §5). */
.fc-node-status { display: inline-flex; align-items: center; gap: var(--space-2);
  margin-top: var(--space-2); font-size: var(--text-2xs);
  font-weight: var(--weight-semibold); color: var(--fc-status, var(--text-muted)); }

/* ── ports ─────────────────────────────────────────────────── */
/* 24x24 transparent hit target (WCAG 2.5.8); the visible 12 px dot is
   drawn by ::before so the pointer target exceeds the mark. */
.fc-port { position: absolute; width: 24px; height: 24px;
  display: grid; place-items: center; background: none; border: 0;
  padding: 0; cursor: crosshair; }
.fc-port::before { content: ""; width: 12px; height: 12px; box-sizing: border-box;
  border-radius: 50%; background: var(--bg-surface-raised);
  border: 2px solid var(--border-strong); }
.fc-port:hover::before { border-color: var(--accent-fg);
  background: var(--accent-bg); }
.fc-port:focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 0; }
.fc-port.in  { left: -12px; }
.fc-port.out { right: -12px; }
/* the try/catch error port — a square, not a circle: a shape cue that does
   not depend on colour (WCAG 1.4.1). Paired with the literal "on error". */
.fc-port.err::before { border-radius: var(--radius-sm);
  border-color: var(--warning-fg); }
.fc-port.err:hover::before { border-color: var(--warning-fg);
  background: var(--warning-bg); }
.fc-port-label { position: absolute; top: 50%; right: var(--space-6);
  font-size: var(--text-2xs); line-height: 1; color: var(--text-muted);
  white-space: nowrap; pointer-events: none; transform: translateY(-50%); }

/* ── run overlay (pre-staged; PURA-266 markup) ─────────────── */
.fc-node--running     { border-color: var(--accent-bg); --fc-status: var(--accent-fg);
  animation: fc-pulse 1.4s var(--ease-standard) infinite; }
.fc-node--ok          { border-color: var(--success-bg); --fc-status: var(--success-fg); }
.fc-node--errored     { border-color: var(--danger-bg); --fc-status: var(--danger-fg); }
.fc-node--skipped     { opacity: .55; --fc-status: var(--text-muted); }
.fc-node--interrupted { border-color: var(--border-strong); border-style: dashed;
  --fc-status: var(--text-muted); }
/* pulse spread uses the literal --c-accent-600 (#3CBEEF) since a box-shadow
   spread needs an alpha ramp a single token cannot express. */
@keyframes fc-pulse {
  0%, 100% { box-shadow: var(--shadow-md), 0 0 0 0 rgba(60, 190, 239, .45); }
  50%      { box-shadow: var(--shadow-md), 0 0 0 5px rgba(60, 190, 239, 0); }
}
@media (prefers-reduced-motion: reduce) {
  .fc-node--running { animation: none;
    box-shadow: var(--shadow-md), 0 0 0 2px var(--accent-bg); }
}

/* ── toolbar ───────────────────────────────────────────────── */
.fc-toolbar { position: absolute; left: var(--space-4); bottom: var(--space-4);
  display: flex; gap: var(--space-1); padding: var(--space-1);
  background: var(--bg-surface-raised); border: 1px solid var(--border-strong);
  border-radius: var(--radius-md); box-shadow: var(--shadow-md); }
.fc-toolbar button { width: 28px; height: 26px; border: 0; background: none;
  border-radius: var(--radius-sm); cursor: pointer;
  font-size: var(--text-sm); color: var(--text-primary); }
.fc-toolbar button:hover { background: var(--bg-hover); }
.fc-toolbar button:focus-visible { outline: 2px solid var(--focus-ring);
  outline-offset: -1px; }
.fc-toolbar .fc-zoom-label { display: flex; align-items: center;
  padding: 0 var(--space-3); font-size: var(--text-2xs);
  color: var(--text-secondary); }

/* ── inspector ─────────────────────────────────────────────── */
.fc-inspector { padding: var(--space-5);
  border-left: 1px solid var(--border-subtle);
  background: var(--bg-surface); overflow-y: auto; }
.fc-inspector h3 { margin: var(--space-4) 0 var(--space-3);
  font-size: var(--text-sm); line-height: var(--lh-sm);
  font-weight: var(--weight-semibold); }
.fc-field { display: block; margin-bottom: var(--space-4); }
.fc-field > span { display: block; margin-bottom: var(--space-2);
  font-size: var(--text-2xs); line-height: var(--lh-2xs);
  color: var(--text-secondary); }
.fc-field input, .fc-field select, .fc-field textarea { width: 100%;
  box-sizing: border-box; padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border-strong); border-radius: var(--radius-md);
  background: var(--bg-canvas); color: var(--text-primary);
  font-size: var(--text-sm); font-family: inherit; }
.fc-field input:focus-visible, .fc-field select:focus-visible,
.fc-field textarea:focus-visible { outline: 2px solid var(--focus-ring);
  outline-offset: 1px; border-color: var(--accent-fg); }
.fc-field textarea { resize: vertical; min-height: 48px;
  font-family: var(--font-mono); }
.fc-hint { margin: var(--space-2) 0 var(--space-4);
  font-size: var(--text-2xs); line-height: var(--lh-xs); color: var(--text-muted); }
.fc-hint.warn { color: var(--warning-fg); }
.fc-muted { color: var(--text-muted); font-size: var(--text-sm); }
.fc-case { margin-bottom: var(--space-3); padding: var(--space-3);
  border: 1px solid var(--border-subtle); border-radius: var(--radius-md); }
.fc-case-head { display: flex; justify-content: space-between;
  align-items: center; margin-bottom: var(--space-2); }
.fc-btn-sm { padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border-strong); border-radius: var(--radius-md);
  background: var(--bg-surface-raised); color: var(--text-primary);
  font-size: var(--text-xs); cursor: pointer; }
.fc-btn-sm:hover { border-color: var(--accent-fg); }
.fc-btn-sm:focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 1px; }
.fc-btn-danger { color: var(--danger-fg); }

/* ── status bar ────────────────────────────────────────────── */
.fc-statusbar { grid-column: 1 / -1; display: flex; gap: var(--space-5);
  align-items: center; padding: var(--space-2) var(--space-4);
  border-top: 1px solid var(--border-subtle);
  background: var(--bg-surface); font-size: var(--text-xs);
  color: var(--text-primary); }
.fc-statusbar .fc-live { flex: 1; color: var(--text-secondary); }
"#;
