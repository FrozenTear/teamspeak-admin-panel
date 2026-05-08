//! Resolve the operator's currently-active server.
//!
//! Phase 2 doesn't yet hoist a single shared selected-server signal — the
//! header's [`super::super::components::ServerSelector`] keeps its own
//! signal backed by `ui_prefs::SELECTED_SERVER_STORAGE_KEY`. Page bodies
//! and the activity feed reconverge by reading the same localStorage key
//! and falling back to "first row from `/api/servers`", which is exactly
//! what the dashboard does (see `dashboard_placeholder::fetch_dashboard`).
//!
//! When the cross-page selection signal lands in a sibling ticket, this
//! helper is the one place to swap.

use ts6_manager_shared::servers::ServerSummary;

use crate::client::storage::Storage;
use crate::client::ui_prefs::load_selected_server_id;
use crate::ui::layout::ServersData;

/// Spec §4.2.5 — virtual-server id defaults to `1`. Multi-VS picker is a
/// later phase; the constant is shared so a future change is a single-
/// site swap.
pub const DEFAULT_VIRTUAL_SERVER_ID: i64 = 1;

/// Returns the active server (the one the operator picked, else the first
/// row in the live list). `None` iff the list is empty / loading / errored.
pub fn resolve(servers: &ServersData, storage: &dyn Storage) -> Option<ServerSummary> {
    let rows = servers.rows();
    if let Some(id) = load_selected_server_id(storage) {
        if let Some(s) = rows.iter().find(|s| s.id == id).cloned() {
            return Some(s);
        }
    }
    rows.first().cloned()
}
