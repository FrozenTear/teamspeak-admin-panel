//! `/flows/...` — operator-facing flow-engine UI (PURA-243).
//!
//! Four pages per `docs/flows/ui-brief.md`:
//!
//! - [`FlowsListPage`] — `/flows` — list, fire, enable/disable, delete.
//! - [`FlowFormPage`] — `/flows/new` — create-flow form.
//! - [`FlowDetailPage`] — `/flows/{id}` — tabs (Runs / Definition).
//! - [`FlowEditPage`] — `/flows/{id}/edit` — same shape as create, with
//!   the trigger/actions sections locked while the flow is enabled.
//!
//! Pages share REST plumbing through [`crate::client::flows`]; on-screen
//! styling reuses the existing `data-table`, `card`, `empty`, `crumb`,
//! `page-header`, and `bot-badge` tokens so no new design language is
//! introduced (matches the `music_bots` family from PURA-124 WS-6).

mod canvas;
mod detail;
mod dialog;
mod form;
mod list;
mod shared;

pub use detail::FlowDetailPage;
pub use form::{FlowEditPage, FlowFormPage};
pub use list::FlowsListPage;

// PURA-267 — v2 visual canvas builder. The editor component is re-exported
// for the future `/flows/new` + `/flows/{id}/edit` swap; the debug-only
// `/dev/flow-canvas` page mounts it while the v2 HTTP surface lands.
#[cfg(debug_assertions)]
pub use canvas::DevFlowCanvasPage;
#[allow(unused_imports)]
pub use canvas::FlowCanvasEditor;
