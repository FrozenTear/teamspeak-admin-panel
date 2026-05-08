//! UI-prefs persistence (spec §28.3).
//!
//! Phase 1 persists the theme and the operator's last-selected server id
//! (PURA-34 — when the live server-selector mounts, it restores the row that
//! was active at logout so the operator doesn't have to re-pick on every
//! sign-in). Sidebar-collapsed lives with the actual collapse feature in
//! Phase 2.
//!
//! The wire format is the bare data-attr value for the theme (`"dark"` /
//! `"light"`) and the decimal `i64` id for the selected server, so a corrupted
//! browser blob is human-debuggable without a JSON parser.

use crate::client::storage::Storage;
use crate::ui::theme::Theme;

/// `localStorage` key carrying the persisted theme. Namespaced under the
/// app's prefix so a future v2 schema can add sibling keys without
/// colliding with the auth blob (`ts6-manager.auth.session`).
pub const THEME_STORAGE_KEY: &str = "ts6-manager.ui.theme";

/// `localStorage` key carrying the operator's last-selected server id (PURA-34).
/// Sibling to [`THEME_STORAGE_KEY`] under the same `ts6-manager.ui.*` namespace.
pub const SELECTED_SERVER_STORAGE_KEY: &str = "ts6-manager.ui.selected-server";

/// Read the persisted theme. Returns `None` for missing key, empty value,
/// and any unrecognised serialisation — the caller falls back to the type
/// default rather than panic on user-tampered storage.
pub fn load_theme(storage: &dyn Storage) -> Option<Theme> {
    let raw = storage.get(THEME_STORAGE_KEY)?;
    match raw.as_str() {
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        _ => None,
    }
}

/// Persist the theme. Always overwrites — there is no "remove preference"
/// flow in Phase 1; clearing the key requires a `Storage::remove` from the
/// caller (we don't expose that here because we don't need it yet).
pub fn save_theme(storage: &dyn Storage, theme: Theme) {
    storage.set(THEME_STORAGE_KEY, theme.data_attr());
}

/// Read the persisted server id. Returns `None` for missing key, empty
/// value, and any non-integer payload — the caller falls back to "first row
/// from the live list" rather than panic on user-tampered storage.
pub fn load_selected_server_id(storage: &dyn Storage) -> Option<i64> {
    let raw = storage.get(SELECTED_SERVER_STORAGE_KEY)?;
    raw.trim().parse().ok()
}

/// Persist the selected server id. Caller invokes this from the dropdown's
/// `onselect` so a refresh / re-login restores the operator's last context.
pub fn save_selected_server_id(storage: &dyn Storage, id: i64) {
    storage.set(SELECTED_SERVER_STORAGE_KEY, &id.to_string());
}

/// Drop the persisted server id. Used when the live list comes back empty
/// (operator deleted every row from another tab) so a stale id doesn't keep
/// pointing at nothing.
pub fn clear_selected_server_id(storage: &dyn Storage) {
    storage.remove(SELECTED_SERVER_STORAGE_KEY);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::MemoryStore;

    #[test]
    fn missing_key_loads_as_none() {
        let s = MemoryStore::new();
        assert_eq!(load_theme(&s), None);
    }

    #[test]
    fn empty_value_loads_as_none() {
        let s = MemoryStore::from_iter([(THEME_STORAGE_KEY, "")]);
        assert_eq!(load_theme(&s), None);
    }

    #[test]
    fn round_trip_preserves_dark() {
        let s = MemoryStore::new();
        save_theme(&s, Theme::Dark);
        assert_eq!(load_theme(&s), Some(Theme::Dark));
    }

    #[test]
    fn round_trip_preserves_light() {
        let s = MemoryStore::new();
        save_theme(&s, Theme::Light);
        assert_eq!(load_theme(&s), Some(Theme::Light));
    }

    #[test]
    fn unrecognised_value_loads_as_none() {
        // Forward-compat: a future Phase-2 build might write `"system"`
        // for prefers-color-scheme. We don't recognise it yet; the caller
        // falls back to the default rather than crashing.
        let s = MemoryStore::from_iter([(THEME_STORAGE_KEY, "system")]);
        assert_eq!(load_theme(&s), None);

        // Garbage from a tampered devtools session also loads as None.
        let s = MemoryStore::from_iter([(THEME_STORAGE_KEY, "{\"theme\":1}")]);
        assert_eq!(load_theme(&s), None);
    }

    #[test]
    fn save_uses_the_data_attr_value_directly() {
        let s = MemoryStore::new();
        save_theme(&s, Theme::Light);
        assert_eq!(s.get(THEME_STORAGE_KEY).as_deref(), Some("light"));
        save_theme(&s, Theme::Dark);
        assert_eq!(s.get(THEME_STORAGE_KEY).as_deref(), Some("dark"));
    }

    // ── selected-server persistence (PURA-34) ─────────────────────────────

    #[test]
    fn selected_server_round_trips_decimal_id() {
        let s = MemoryStore::new();
        save_selected_server_id(&s, 42);
        assert_eq!(load_selected_server_id(&s), Some(42));
        // Overwrite path — last write wins.
        save_selected_server_id(&s, 7);
        assert_eq!(load_selected_server_id(&s), Some(7));
    }

    #[test]
    fn selected_server_handles_corrupt_payload_as_none() {
        // Tampered devtools value or a future schema bump that wrote a
        // composite (`"1,2"`) — neither parses as i64 so we fall back to
        // the live "first row" default rather than crash.
        let s = MemoryStore::from_iter([(SELECTED_SERVER_STORAGE_KEY, "not-a-number")]);
        assert_eq!(load_selected_server_id(&s), None);
        let s = MemoryStore::from_iter([(SELECTED_SERVER_STORAGE_KEY, "")]);
        assert_eq!(load_selected_server_id(&s), None);
    }

    #[test]
    fn selected_server_clear_removes_key() {
        let s = MemoryStore::new();
        save_selected_server_id(&s, 1);
        clear_selected_server_id(&s);
        assert_eq!(load_selected_server_id(&s), None);
        assert_eq!(s.get(SELECTED_SERVER_STORAGE_KEY), None);
    }

    #[test]
    fn selected_server_storage_key_is_stable() {
        // Pinning the on-wire key so a refactor that renames it forces a
        // visible test failure (the operator's persisted choice would
        // silently reset otherwise).
        assert_eq!(
            SELECTED_SERVER_STORAGE_KEY,
            "ts6-manager.ui.selected-server"
        );
    }
}
