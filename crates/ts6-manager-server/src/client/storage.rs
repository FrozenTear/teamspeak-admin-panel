//! Pluggable string-keyed storage so the auth store can hydrate from
//! `localStorage` on WASM and from an in-memory map in tests.
//!
//! The trait is deliberately tiny — get / set / remove — because that is the
//! intersection of what `web-sys::Storage` exposes and what the session store
//! actually needs. Anything richer would force every backend (mock,
//! sessionStorage, indexedDB, ssr-noop) to grow surface for no gain.

use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal string-keyed key/value backend.
///
/// Implementations MUST be safe to call from the same thread that hosts the
/// Dioxus signal store. `web-sys::Storage` only exists on the browser main
/// thread, so the trait does not require `Send`/`Sync` — the caller decides.
pub trait Storage {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
    fn remove(&self, key: &str);
}

/// In-memory `Storage` for tests and the SSR pass on native targets.
///
/// `Mutex` covers the store-from-tests case where we exercise concurrent
/// pretend-callers; on the browser we'd never share a single `MemoryStore`
/// across threads anyway.
pub struct MemoryStore {
    inner: Mutex<HashMap<String, String>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_iter<I, K, V>(items: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map = items
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self {
            inner: Mutex::new(map),
        }
    }
}

impl Storage for MemoryStore {
    fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
    }

    fn remove(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

/// `window.localStorage`-backed storage. Constructable only on WASM; on
/// other targets the `new()` constructor is missing so accidental use fails
/// at compile time rather than at runtime.
#[cfg(target_arch = "wasm32")]
pub struct LocalStorageStore;

#[cfg(target_arch = "wasm32")]
impl LocalStorageStore {
    /// Returns `None` if `window.localStorage` is unavailable (e.g. the page
    /// is in a sandboxed iframe). Callers should fall back to [`MemoryStore`]
    /// rather than treating that as a fatal error.
    pub fn try_new() -> Option<Self> {
        let window = web_sys::window()?;
        // Touch the storage handle so we don't hand out a store that will
        // raise on first call. `local_storage()` returns
        // `Result<Option<Storage>, JsValue>`.
        window.local_storage().ok()??;
        Some(Self)
    }

    fn handle(&self) -> Option<web_sys::Storage> {
        let window = web_sys::window()?;
        window.local_storage().ok()?
    }
}

#[cfg(target_arch = "wasm32")]
impl Storage for LocalStorageStore {
    fn get(&self, key: &str) -> Option<String> {
        let Some(handle) = self.handle() else {
            log_session_op(key, "storage.get.no_handle", true, None);
            return None;
        };
        let result = handle.get_item(key).ok().flatten();
        log_session_op(
            key,
            "storage.get",
            result.is_none(),
            result.as_ref().map(|v| v.len()),
        );
        result
    }

    fn set(&self, key: &str, value: &str) {
        let Some(s) = self.handle() else {
            log_session_op(key, "storage.set.no_handle", true, Some(value.len()));
            return;
        };
        // localStorage can throw QuotaExceededError; silently dropping
        // is fine for a session blob — at worst the user re-logs in.
        let err = s.set_item(key, value).is_err();
        log_session_op(key, "storage.set", err, Some(value.len()));
    }

    fn remove(&self, key: &str) {
        let Some(s) = self.handle() else {
            log_session_op(key, "storage.remove.no_handle", true, None);
            return;
        };
        let err = s.remove_item(key).is_err();
        log_session_op(key, "storage.remove", err, None);
    }
}

/// PURA-226 — filtered session-blob breadcrumb for the auth debug knob.
/// Logging every `localStorage` access would drown the auth signal in
/// theme / ui-pref / ws-cursor noise, so we only emit when the key is
/// the persisted session blob (the row that actually carries the
/// access + refresh pair).
#[cfg(target_arch = "wasm32")]
fn log_session_op(key: &str, tag: &str, is_err: bool, bytes: Option<usize>) {
    use crate::client::debug as auth_debug;
    use crate::client::store::SESSION_STORAGE_KEY;
    if key != SESSION_STORAGE_KEY {
        return;
    }
    let bytes_value = bytes
        .map(|n| serde_json::Value::from(n))
        .unwrap_or(serde_json::Value::Null);
    auth_debug::log(
        tag,
        auth_debug::fields(&[("err", is_err.into()), ("bytes", bytes_value)]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_strings() {
        let s = MemoryStore::new();
        assert_eq!(s.get("missing"), None);
        s.set("a", "alpha");
        s.set("b", "beta");
        assert_eq!(s.get("a"), Some("alpha".into()));
        assert_eq!(s.get("b"), Some("beta".into()));
        s.set("a", "alpha2");
        assert_eq!(s.get("a"), Some("alpha2".into()));
        s.remove("a");
        assert_eq!(s.get("a"), None);
    }

    #[test]
    fn from_iter_seeds_initial_values() {
        let s = MemoryStore::from_iter([("k", "v")]);
        assert_eq!(s.get("k"), Some("v".into()));
    }
}
