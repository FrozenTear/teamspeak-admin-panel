//! UI-prefs persistence (spec §28.3).
//!
//! Phase 1 only persists the theme — sidebar-collapsed lives with the
//! actual collapse feature in Phase 2, and selected-server lives with the
//! functional server-selector dropdown.
//!
//! The wire format is the bare data-attr value that `tokens.css` keys off
//! (`"dark"` / `"light"`) so a corrupted browser blob is human-debuggable
//! without a JSON parser.

use crate::client::storage::Storage;
use crate::ui::theme::Theme;

/// `localStorage` key carrying the persisted theme. Namespaced under the
/// app's prefix so a future v2 schema can add sibling keys without
/// colliding with the auth blob (`ts6-manager.auth.session`).
pub const THEME_STORAGE_KEY: &str = "ts6-manager.ui.theme";

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
}
