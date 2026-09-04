//! Resolve the operator's currently-active server.
//!
//! The header [`super::super::components::ServerSelector`] writes the
//! operator's pick to [`crate::ui::layout::ServersContext::selected`] and
//! persists the same id through `ui_prefs::SELECTED_SERVER_STORAGE_KEY`.
//! Page bodies that have not yet subscribed to that signal still reconverge
//! by reading the storage key (and falling back to the first live row).
//! The dashboard is the first surface that reads the shared signal
//! directly — use [`resolve_selected`] there so a missing pick renders
//! as "no selection" instead of silently showing another server's KPIs.
//!
//! Spec §4.2.5 still pins the virtual-server id to
//! [`DEFAULT_VIRTUAL_SERVER_ID`] (`1`). A live vs-picker is a later phase;
//! keep the constant until that picker exists.

use ts6_manager_shared::servers::ServerSummary;

use crate::client::storage::Storage;
use crate::client::ui_prefs::load_selected_server_id;
use crate::ui::layout::ServersData;

/// Spec §4.2.5 — virtual-server id defaults to `1`. Multi-VS picker is a
/// later phase; the constant is shared so a future change is a single-
/// site swap. Dashboard KPI fetches still use this Phase-1 pin.
pub const DEFAULT_VIRTUAL_SERVER_ID: i64 = 1;

/// Returns the active server (the one the operator picked, else the first
/// row in the live list). `None` iff the list is empty / loading / errored.
pub fn resolve(servers: &ServersData, storage: &dyn Storage) -> Option<ServerSummary> {
    let rows = servers.rows();
    if let Some(id) = load_selected_server_id(storage)
        && let Some(s) = rows.iter().find(|s| s.id == id).cloned()
    {
        return Some(s);
    }
    rows.first().cloned()
}

/// Resolve an explicit pick against the live list. Unlike [`resolve`], this
/// does **not** fall back to the first row — `None` means the operator has
/// not selected a server (or the persisted id is no longer in the list).
pub fn resolve_selected(servers: &ServersData, selected: Option<i64>) -> Option<ServerSummary> {
    let id = selected?;
    servers.rows().iter().find(|s| s.id == id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::api::ApiError;
    use chrono::Utc;

    fn fixture(id: i64, name: &str) -> ServerSummary {
        let now = Utc::now();
        ServerSummary {
            id,
            name: name.into(),
            host: "ts.example.com".into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: None,
            has_ssh_credentials: false,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
            last_seen_at: None,
        }
    }

    #[test]
    fn resolve_selected_returns_matching_row() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary"), fixture(9, "Backup")]);
        let hit = resolve_selected(&list, Some(9)).expect("row 9");
        assert_eq!(hit.id, 9);
        assert_eq!(hit.name, "Backup");
    }

    #[test]
    fn resolve_selected_is_none_without_a_pick() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary")]);
        assert!(resolve_selected(&list, None).is_none());
        assert!(resolve_selected(&list, Some(99)).is_none());
        assert!(resolve_selected(&ServersData::Loading, Some(7)).is_none());
        assert!(
            resolve_selected(
                &ServersData::Error(ApiError::Transport("boom".into())),
                Some(7)
            )
            .is_none()
        );
    }

    #[test]
    fn virtual_server_id_stays_pinned_to_phase1_default() {
        assert_eq!(DEFAULT_VIRTUAL_SERVER_ID, 1);
    }
}
