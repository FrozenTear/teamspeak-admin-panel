//! Canvas CSS.
//!
//! **Placeholder styling, pending [UXDesigner](/PURA/agents/uxdesigner).**
//! `ui-brief.md` §7 mandates the node visuals, palette chips, ports, and
//! run-overlay treatment come from the design-token system, not one-off
//! values. The canvas-spike report flagged inline CSS as "a deliberate
//! throwaway shortcut, not a pattern". This sheet keeps the production
//! editor legible and self-contained until the UX sub-task lands its
//! tokens; it is scoped under `.fc-*` so swapping it is a contained edit.

/// The canvas stylesheet, injected once per editor mount via a `<style>`
/// element. Scoped to `.fc-` classes so it cannot leak into app chrome.
pub const CANVAS_CSS: &str = r#"
.fc-editor { display: grid; grid-template-columns: 184px 1fr 280px;
  height: calc(100vh - 168px); min-height: 460px; border: 1px solid #d6dbe6;
  border-radius: 10px; overflow: hidden; background: #fff;
  font-family: system-ui, -apple-system, sans-serif; color: #1c2230; }
.fc-editor.read-only .fc-palette, .fc-editor.read-only .fc-toolbar { display: none; }

.fc-palette { padding: 12px 10px; border-right: 1px solid #d6dbe6;
  background: #fbfcfe; overflow-y: auto; }
.fc-pane-title { font-size: 11px; text-transform: uppercase; letter-spacing: .05em;
  color: #6b7488; margin: 0 0 10px; }
.fc-chip { display: flex; flex-direction: column; gap: 2px; width: 100%;
  margin-bottom: 7px; padding: 8px 10px; border: 1px solid #c4cad8;
  border-radius: 8px; background: #fff; cursor: pointer; text-align: left; }
.fc-chip:hover:not(:disabled) { border-color: #4f7cff; }
.fc-chip:focus-visible { outline: 2px solid #4f7cff; outline-offset: 1px; }
.fc-chip:disabled { opacity: .45; cursor: not-allowed; }
.fc-chip-head { display: flex; align-items: center; gap: 7px;
  font-size: 13px; font-weight: 600; }
.fc-chip-desc { font-size: 11px; color: #707a90; }
.fc-glyph { font-size: 15px; }

.fc-canvas-wrap { position: relative; overflow: hidden;
  background: #f6f7fb radial-gradient(#dde1ec 1px, transparent 1px);
  background-size: 22px 22px; touch-action: none; }
.fc-canvas-wrap.panning { cursor: grabbing; }
.fc-world { position: absolute; inset: 0; transform-origin: 0 0; }
.fc-edges { position: absolute; left: 0; top: 0; overflow: visible;
  pointer-events: none; }
.fc-edge { fill: none; stroke: #8b94ab; stroke-width: 2; }
.fc-edge.err { stroke: #d98a2b; stroke-dasharray: 6 4; }
.fc-edge.preview { stroke: #4f7cff; stroke-width: 2; stroke-dasharray: 6 4; }

.fc-node { position: absolute; border: 1px solid #c4cad8; border-radius: 9px;
  background: #fff; box-shadow: 0 2px 6px rgba(20,28,48,.10); }
.fc-node:focus-visible { outline: 2px solid #4f7cff; outline-offset: 2px; }
.fc-node.selected { border-color: #4f7cff;
  box-shadow: 0 0 0 2px rgba(79,124,255,.28); }
.fc-node.connect-src { border-color: #2b9d6b;
  box-shadow: 0 0 0 2px rgba(43,157,107,.32); }
.fc-node-head { display: flex; align-items: center; gap: 7px; padding: 7px 10px;
  border-bottom: 1px solid #e5e8f0; cursor: grab; font-size: 13px;
  font-weight: 600; border-radius: 9px 9px 0 0; background: #f0f2f8; }
.fc-node-title { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.fc-node-body { padding: 7px 10px; font-size: 11px; color: #707a90;
  line-height: 1.45; }
.fc-port { position: absolute; width: 14px; height: 14px; border-radius: 50%;
  background: #fff; border: 2px solid #6b7488; cursor: crosshair; }
.fc-port:hover { border-color: #4f7cff; background: #4f7cff; }
.fc-port.in { left: -7px; }
.fc-port.out { right: -7px; }
.fc-port.err { border-color: #d98a2b; border-style: dashed; }
.fc-port-label { position: absolute; right: 14px; font-size: 9px; color: #97a0b4;
  white-space: nowrap; pointer-events: none; transform: translateY(-50%); }

.fc-toolbar { position: absolute; left: 10px; bottom: 10px; display: flex;
  gap: 4px; background: #fff; border: 1px solid #c4cad8; border-radius: 7px;
  padding: 3px; box-shadow: 0 2px 6px rgba(20,28,48,.12); }
.fc-toolbar button { width: 28px; height: 26px; border: none; background: none;
  border-radius: 5px; cursor: pointer; font-size: 13px; color: #2c3550; }
.fc-toolbar button:hover { background: #eef1f6; }
.fc-toolbar .fc-zoom-label { display: flex; align-items: center; padding: 0 6px;
  font-size: 11px; color: #5a6378; }

.fc-inspector { padding: 14px 14px 18px; border-left: 1px solid #d6dbe6;
  background: #fbfcfe; overflow-y: auto; }
.fc-inspector h3 { font-size: 13px; margin: 10px 0 6px; }
.fc-field { display: block; margin-bottom: 10px; }
.fc-field > span { display: block; font-size: 11px; color: #6b7488;
  margin-bottom: 3px; }
.fc-field input, .fc-field select, .fc-field textarea { width: 100%;
  box-sizing: border-box; padding: 6px 8px; border: 1px solid #c4cad8;
  border-radius: 6px; font-size: 13px; font-family: inherit; }
.fc-field textarea { resize: vertical; min-height: 48px;
  font-family: ui-monospace, monospace; }
.fc-hint { font-size: 11px; color: #707a90; line-height: 1.4;
  margin: 4px 0 10px; }
.fc-hint.warn { color: #9a6212; }
.fc-muted { color: #97a0b4; font-size: 13px; }
.fc-case { border: 1px solid #d6dbe6; border-radius: 7px; padding: 8px;
  margin-bottom: 7px; }
.fc-case-head { display: flex; justify-content: space-between;
  align-items: center; margin-bottom: 5px; }
.fc-btn-sm { font-size: 12px; padding: 4px 9px; border: 1px solid #c4cad8;
  border-radius: 6px; background: #fff; cursor: pointer; }
.fc-btn-sm:hover { border-color: #4f7cff; }
.fc-btn-danger { color: #b3261e; }
.fc-statusbar { grid-column: 1 / -1; display: flex; gap: 14px;
  align-items: center; padding: 6px 12px; border-top: 1px solid #d6dbe6;
  background: #f0f2f8; font-size: 12px; color: #2c3550; }
.fc-statusbar .fc-live { flex: 1; color: #5a6378; }
"#;
