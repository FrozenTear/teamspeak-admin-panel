//! v2 visual flow-canvas builder — `docs/flows/v2/ui-brief.md`
//! ([PURA-267](/PURA/issues/PURA-267), Phase 8 child of
//! [PURA-259](/PURA/issues/PURA-259)).
//!
//! The three-pane editor (palette · canvas · inspector) that replaces the
//! v1.1 vertical card form for *graph* flows. Built on the Option-A model
//! confirmed by the canvas-tech spike ([PURA-264](/PURA/issues/PURA-264)):
//! bespoke SVG/CSS in Dioxus, no JS interop, the graph *is* a
//! `Signal<FlowGraph>`.
//!
//! Module layout:
//!
//! - [`geometry`] — derived port coordinates + bezier edge paths.
//! - [`model`] — the palette catalogue, per-kind port shapes, graph
//!   mutators (add / remove / connect / prune).
//! - [`inspector`] — the selected-node config form, one body per kind.
//! - [`editor`] — the three-pane [`FlowCanvasEditor`] component, drag /
//!   connect / pan / zoom / keyboard.
//! - [`style`] — placeholder CSS, pending the UXDesigner token sub-task.
//!
//! Wired here: the editor surface. Pending the v2 HTTP API
//! ([PURA-266](/PURA/issues/PURA-266)) and so **not** yet wired: debounced
//! `POST /validate` linting, the run-overlay poll, save, and the
//! production route swap. The editor mounts debug-only at `/dev/flow-canvas`
//! ([`DevFlowCanvasPage`]) until those land — the v1.1 form pages stay live
//! for operators in the meantime.

mod editor;
mod geometry;
mod inspector;
mod model;
mod style;

#[cfg(debug_assertions)]
pub use editor::DevFlowCanvasPage;
pub use editor::FlowCanvasEditor;
pub use model::starter_graph;
