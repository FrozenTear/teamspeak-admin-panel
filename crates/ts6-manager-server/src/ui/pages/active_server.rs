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

use crate::client::api::ApiError;
use crate::client::storage::Storage;
use crate::client::ui_prefs::load_selected_server_id;
use crate::ui::layout::ServersData;

/// Spec §4.2.5 — virtual-server id defaults to `1`. Multi-VS picker is a
/// later phase; the constant is shared so a future change is a single-
/// site swap. Dashboard KPI fetches still use this Phase-1 pin.
pub const DEFAULT_VIRTUAL_SERVER_ID: i64 = 1;

/// Chrome-list → page-body decision for any surface scoped to the header
/// server pick. Distinguishes "list still in flight" from a real empty
/// pick so a hard-refresh cannot freeze on **No server selected** while
/// `ServersContext` is still `Loading` (same family as #20).
///
/// `Selected` is boxed so the enum stays small — `ServerSummary` is ~208
/// bytes (`large_enum_variant` on rustc 1.95 / clippy `-D warnings`).
#[derive(Clone, Debug, PartialEq)]
pub enum ActiveServerSelection {
    WaitingOnList,
    ListError(ApiError),
    NoServers,
    NoSelection,
    Selected(Box<ServerSummary>),
}

/// Resolve the header pick against the live chrome list.
///
/// Loading and a chrome-list error are **not** "no server selected" —
/// pages that early-return an empty state on `None` from [`resolve`]
/// never restart their `use_resource` once the list arrives (Dioxus 0.7
/// hook order: the resource is registered only on the Selected path).
pub fn selection_from_context(
    list: &ServersData,
    selected_id: Option<i64>,
) -> ActiveServerSelection {
    match list {
        ServersData::Loading => ActiveServerSelection::WaitingOnList,
        ServersData::Error(err) => ActiveServerSelection::ListError(err.clone()),
        ServersData::Loaded(rows) if rows.is_empty() => ActiveServerSelection::NoServers,
        ServersData::Loaded(_) => match resolve_selected(list, selected_id) {
            Some(server) => ActiveServerSelection::Selected(Box::new(server)),
            None => ActiveServerSelection::NoSelection,
        },
    }
}

/// Predicate for a server-scoped page fetch. First paint is Anonymous
/// and not-ready (PURA-129); a resource that fires then short-circuits
/// as `SessionAnonymous` and, if it is a one-shot `use_future`, never
/// restarts. Same bits as the chrome `/api/servers` fetch (#20).
pub fn should_load_server_scoped(ready: bool, authenticated: bool) -> bool {
    ready && authenticated
}

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

    #[test]
    fn selection_waits_while_chrome_list_is_loading() {
        // A persisted pick must not look like "no server selected" while
        // GET /api/servers is still in flight — that is the Automod
        // hard-refresh freeze (#20 family).
        assert_eq!(
            selection_from_context(&ServersData::Loading, Some(1)),
            ActiveServerSelection::WaitingOnList
        );
        assert_eq!(
            selection_from_context(&ServersData::Loading, None),
            ActiveServerSelection::WaitingOnList
        );
    }

    #[test]
    fn selection_surfaces_a_chrome_list_error() {
        let err = ApiError::Transport("boom".into());
        assert_eq!(
            selection_from_context(&ServersData::Error(err.clone()), Some(1)),
            ActiveServerSelection::ListError(err)
        );
    }

    #[test]
    fn selection_is_no_servers_when_list_is_empty() {
        assert_eq!(
            selection_from_context(&ServersData::Loaded(Vec::new()), None),
            ActiveServerSelection::NoServers
        );
        assert_eq!(
            selection_from_context(&ServersData::Loaded(Vec::new()), Some(1)),
            ActiveServerSelection::NoServers
        );
    }

    #[test]
    fn selection_is_no_selection_when_pick_is_missing_or_stale() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary"), fixture(9, "Backup")]);
        assert_eq!(
            selection_from_context(&list, None),
            ActiveServerSelection::NoSelection
        );
        assert_eq!(
            selection_from_context(&list, Some(99)),
            ActiveServerSelection::NoSelection
        );
    }

    #[test]
    fn selection_uses_the_picked_row_not_the_first_granted() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary"), fixture(9, "Backup")]);
        match selection_from_context(&list, Some(9)) {
            ActiveServerSelection::Selected(server) => {
                assert_eq!(server.id, 9);
                assert_eq!(server.name, "Backup");
            }
            other => panic!("expected Selected(Backup), got {other:?}"),
        }
    }

    #[test]
    fn server_scoped_fetch_waits_for_rehydrate_and_an_authenticated_session() {
        assert!(
            !should_load_server_scoped(false, false),
            "first paint is Anonymous and not-ready — do not hit a server-scoped API"
        );
        assert!(
            !should_load_server_scoped(false, true),
            "ready-false authed is the SSR harness path, not a live fetch"
        );
        assert!(
            !should_load_server_scoped(true, false),
            "rehydrate finished with no blob — stay Waiting; AppShell bounces"
        );
        assert!(
            should_load_server_scoped(true, true),
            "rehydrate finished with a blob — the page may resolve the pick"
        );
    }
}
