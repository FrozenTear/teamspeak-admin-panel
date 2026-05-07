//! Dioxus glue: a [`SessionHandle`] backed by `SyncSignal<AuthState>` so the
//! refresh interceptor and the UI both observe the same canonical state.
//!
//! This is the production wiring; the in-memory test handle in
//! `client::session::testing` is functionally equivalent but stripped of
//! Dioxus dependencies so the gate's locking + replay logic can be tested
//! without a Dioxus runtime.

use std::sync::Arc;

use dioxus::prelude::*;

use crate::client::session::SessionHandle;
use crate::client::storage::Storage;
use crate::client::store::{AuthState, load_state, save_state};

/// Storage abstraction used at runtime. Required to be `Send + Sync` so the
/// session handle satisfies the gate's bounds — the WASM build's
/// `LocalStorageStore` is single-threaded but `Send + Sync` is trivially
/// satisfiable on a single thread.
pub type SessionStorage = Arc<dyn Storage + Send + Sync>;

/// Session backing the Dioxus `Signal` and `localStorage` together.
///
/// `state` is a [`SyncSignal`] so the handle is `Send + Sync` (required by
/// [`SessionHandle`]). UI components read the same signal via context to
/// re-render on every mutation, regardless of whether the mutation came
/// from the login button or from the refresh interceptor.
#[derive(Clone)]
pub struct DioxusSession {
    pub state: SyncSignal<AuthState>,
    pub storage: SessionStorage,
}

impl DioxusSession {
    /// Build a session whose initial state is hydrated from `storage`.
    ///
    /// Call this once from the root component (`use_context_provider`) so
    /// every consumer sees the same `Signal`.
    pub fn hydrated(storage: SessionStorage) -> Self {
        let initial = load_state(&*storage);
        Self {
            state: SyncSignal::new_maybe_sync(initial),
            storage,
        }
    }

    /// Replace the entire state — used by the login flow on success and by
    /// logout on success. The interceptor uses [`SessionHandle::update_pair`]
    /// instead because it preserves the cached `UserInfo`.
    pub fn replace(&self, state: AuthState) {
        *self.state.write_unchecked() = state.clone();
        save_state(&*self.storage, &state);
    }
}

impl SessionHandle for DioxusSession {
    fn read(&self) -> AuthState {
        self.state.read().clone()
    }
    fn update_pair(&self, access: String, refresh: String) {
        let next = match &*self.state.read() {
            AuthState::Authenticated { user, .. } => AuthState::Authenticated {
                access,
                refresh,
                user: user.clone(),
            },
            // Race: someone invalidated us between the gate's lock and our
            // write. Don't resurrect a session — leave Anonymous in place.
            AuthState::Anonymous => return,
        };
        *self.state.write_unchecked() = next.clone();
        save_state(&*self.storage, &next);
    }
    fn invalidate(&self) {
        *self.state.write_unchecked() = AuthState::Anonymous;
        save_state(&*self.storage, &AuthState::Anonymous);
    }
}

/// Pull the session out of context. Panics if no `DioxusSession` provider
/// is mounted above the caller — every page is expected to be a descendant
/// of `<App>` (which calls `use_context_provider`), so a missing provider
/// is a programmer error, not a runtime situation to recover from.
pub fn use_session() -> DioxusSession {
    use_context::<DioxusSession>()
}

/// Provide a [`DioxusSession`] backed by `localStorage` on the browser and
/// by an in-memory `MemoryStore` everywhere else (server SSR / native
/// tests / sandboxed iframes where `window.localStorage` is missing).
///
/// Returns the session by value so the caller can push it into context
/// (`use_context_provider(|| provide_session())`) and also hand a clone to
/// any non-context consumer such as a button-click closure.
pub fn provide_session() -> DioxusSession {
    let storage: SessionStorage = pick_default_storage();
    DioxusSession::hydrated(storage)
}

#[cfg(target_arch = "wasm32")]
fn pick_default_storage() -> SessionStorage {
    use crate::client::storage::{LocalStorageStore, MemoryStore};
    match LocalStorageStore::try_new() {
        Some(s) => Arc::new(s),
        None => Arc::new(MemoryStore::new()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pick_default_storage() -> SessionStorage {
    use crate::client::storage::MemoryStore;
    Arc::new(MemoryStore::new())
}
