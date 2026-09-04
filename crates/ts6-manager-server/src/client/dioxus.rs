//! Dioxus glue: a [`SessionHandle`] backed by `SyncSignal<AuthState>` so the
//! refresh interceptor and the UI both observe the same canonical state.
//!
//! This is the production wiring; the in-memory test handle in
//! `client::session::testing` is functionally equivalent but stripped of
//! Dioxus dependencies so the gate's locking + replay logic can be tested
//! without a Dioxus runtime.

use std::sync::Arc;

use dioxus::prelude::*;

use crate::client::api;
use crate::client::debug as auth_debug;
use crate::client::session::{HttpRefresh, RefreshFn, RefreshGate, SessionHandle};
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
///
/// `ready` stays `false` until [`rehydrate_from_storage`] (or a login
/// [`DioxusSession::replace`]) has resolved the first-paint `Anonymous`
/// placeholder. Auth-gate redirects must wait for this bit — a bool
/// captured on first paint is always anonymous (PURA-129) and will not
/// re-run after the blob lands.
#[derive(Clone)]
pub struct DioxusSession {
    pub state: SyncSignal<AuthState>,
    pub storage: SessionStorage,
    pub ready: SyncSignal<bool>,
}

impl DioxusSession {
    /// Build a session whose initial state is `Anonymous` regardless of
    /// platform.
    ///
    /// Reading `storage` synchronously here would diverge between SSR
    /// (`MemoryStore` — always empty) and the browser (`LocalStorageStore`
    /// — holds the persisted blob), producing different first-render trees
    /// and a hydration mismatch (`this.nodes[id]` undefined inside
    /// `dioxus-interpreter-js`). The post-mount rehydrate happens in
    /// [`rehydrate_from_storage`] via a client-only `use_effect`, so first
    /// render lines up byte-for-byte across server and browser.
    ///
    /// Call this once from the root component (`use_context_provider`) so
    /// every consumer sees the same `Signal`.
    pub fn new_anonymous(storage: SessionStorage) -> Self {
        Self {
            state: SyncSignal::new_maybe_sync(AuthState::Anonymous),
            storage,
            ready: SyncSignal::new_maybe_sync(false),
        }
    }

    /// Session that is already past the post-mount rehydrate gate.
    ///
    /// SSR chrome tests and page harnesses inject a known [`AuthState`]
    /// instead of running [`rehydrate_from_storage`]; they must start
    /// `ready` so an AppShell-style gate does not treat them as the
    /// first-paint placeholder.
    pub fn new_ready(state: AuthState, storage: SessionStorage) -> Self {
        Self {
            state: SyncSignal::new_maybe_sync(state),
            storage,
            ready: SyncSignal::new_maybe_sync(true),
        }
    }

    /// Replace the entire state — used by the login flow on success and by
    /// logout on success. The interceptor uses [`SessionHandle::update_pair`]
    /// instead because it preserves the cached `UserInfo`.
    pub fn replace(&self, state: AuthState) {
        let next_authed = state.is_authenticated();
        let prev_authed = self.state.read().is_authenticated();
        auth_debug::log(
            "session.replace",
            auth_debug::fields(&[("from", prev_authed.into()), ("to", next_authed.into())]),
        );
        *self.state.write_unchecked() = state.clone();
        *self.ready.write_unchecked() = true;
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
            // PURA-226 — emit a dedicated breadcrumb for this branch so a
            // dropped rotation is distinguishable from a successful one in
            // the console capture. The gate's `session.update_pair` line
            // fires *before* this call, so a tail of
            // `session.update_pair → session.update_pair.dropped_on_anonymous`
            // is the candidate failure #3 fingerprint.
            AuthState::Anonymous => {
                auth_debug::log(
                    "session.update_pair.dropped_on_anonymous",
                    serde_json::Value::Null,
                );
                return;
            }
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

/// Pull the shared [`RefreshGate`] out of context. Same contract as
/// [`use_session`] — every authenticated surface descends from `<App>`,
/// which provides exactly one gate that funnels every fetch through the
/// single-flight refresh interceptor.
pub fn use_auth_gate() -> Arc<RefreshGate> {
    use_context::<Arc<RefreshGate>>()
}

/// Build the [`RefreshGate`] backing every non-auth fetch in the SPA.
///
/// One gate per `<App>`: the single mutex inside ensures that no matter how
/// many concurrent fetches see a 401 at once, exactly one refresh fires.
/// Reuse via `use_context` — see [`use_auth_gate`].
pub fn provide_auth_gate(session: DioxusSession) -> Arc<RefreshGate> {
    let session: Arc<dyn SessionHandle> = Arc::new(session);
    let refresh: Arc<dyn RefreshFn> = Arc::new(HttpRefresh::new(api::api_base()));
    Arc::new(RefreshGate::new(session, refresh))
}

/// Provide a [`DioxusSession`] backed by `localStorage` on the browser and
/// by an in-memory `MemoryStore` everywhere else (server SSR / native
/// tests / sandboxed iframes where `window.localStorage` is missing).
///
/// Returns the session in the `Anonymous` state. The browser-side blob is
/// applied post-mount by [`rehydrate_from_storage`].
///
/// Returns the session by value so the caller can push it into context
/// (`use_context_provider(|| provide_session())`) and also hand a clone to
/// any non-context consumer such as a button-click closure.
pub fn provide_session() -> DioxusSession {
    let storage: SessionStorage = pick_default_storage();
    DioxusSession::new_anonymous(storage)
}

/// Read the persisted auth blob and copy it into `session.state`.
///
/// Mount this from the root component inside a `use_effect` — `use_effect`
/// is client-only (it does not run during SSR), so the first render on
/// both server and browser observes the same `Anonymous` state, hydration
/// walks identical trees, and the real auth state is applied immediately
/// after mount.
///
/// Always flips [`DioxusSession::ready`] — including the empty-storage
/// path — so auth-gate effects that *subscribe* to `ready` + `state`
/// (instead of closing over a first-paint bool) can decide whether to
/// bounce to `/login`. `use_ws_lifecycle` already reads `session.state`
/// inside its effect and self-heals; `AppShell` / `LoginPage` did not,
/// which is what stranded a valid `localStorage` blob on the login form
/// after a hard refresh.
pub fn rehydrate_from_storage(session: &DioxusSession) {
    let loaded = load_state(&*session.storage);
    let hydrated = matches!(loaded, AuthState::Authenticated { .. });
    auth_debug::log(
        "session.rehydrate",
        auth_debug::fields(&[
            ("hydrated", hydrated.into()),
            (
                "access",
                match &loaded {
                    AuthState::Authenticated { access, .. } => {
                        auth_debug::short_token(access).into()
                    }
                    AuthState::Anonymous => "".into(),
                },
            ),
        ]),
    );
    if hydrated {
        let state = session.state;
        *state.write_unchecked() = loaded;
    }
    *session.ready.write_unchecked() = true;
}

/// AppShell `/login` bounce predicate.
///
/// First paint is always [`AuthState::Anonymous`] (PURA-129). Bouncing
/// before [`rehydrate_from_storage`] flips `ready` races a valid
/// `localStorage` blob onto the login form; waiting for `ready` is the
/// same class of gate as PURA-232's `SessionAnonymous` short-circuit.
pub fn should_redirect_anonymous_to_login(ready: bool, authenticated: bool) -> bool {
    ready && !authenticated
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::MemoryStore;
    use crate::client::store::save_state;
    use ts6_manager_shared::auth::UserInfo;

    fn authed() -> AuthState {
        AuthState::Authenticated {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            user: UserInfo {
                id: 1,
                username: "op".into(),
                display_name: "Operator".into(),
                role: "admin".into(),
            },
        }
    }

    #[test]
    fn anonymous_bounce_waits_for_ready() {
        assert!(
            !should_redirect_anonymous_to_login(false, false),
            "first paint is Anonymous and not ready — do not bounce"
        );
        assert!(
            !should_redirect_anonymous_to_login(false, true),
            "a ready-false authed session is the SSR harness path, not a bounce"
        );
        assert!(
            should_redirect_anonymous_to_login(true, false),
            "rehydrate finished with no blob — bounce to login"
        );
        assert!(
            !should_redirect_anonymous_to_login(true, true),
            "rehydrate finished with a blob — stay on the panel"
        );
    }

    #[test]
    fn rehydrate_empty_storage_stays_anonymous_and_marks_ready() {
        let mut dom = VirtualDom::new(EmptyRehydrateHarness);
        dom.rebuild_in_place();
    }

    #[test]
    fn rehydrate_copies_blob_and_marks_ready() {
        let mut dom = VirtualDom::new(BlobRehydrateHarness);
        dom.rebuild_in_place();
    }

    #[component]
    fn EmptyRehydrateHarness() -> Element {
        let session = use_hook(|| DioxusSession::new_anonymous(Arc::new(MemoryStore::new())));
        assert!(
            !*session.ready.peek(),
            "new_anonymous must start not-ready so AppShell waits"
        );
        assert!(!session.state.read().is_authenticated());
        rehydrate_from_storage(&session);
        assert!(
            *session.ready.peek(),
            "empty-storage rehydrate must still flip ready"
        );
        assert!(
            !session.state.read().is_authenticated(),
            "no blob means the signal stays Anonymous"
        );
        rsx! { "" }
    }

    #[component]
    fn BlobRehydrateHarness() -> Element {
        let session = use_hook(|| {
            let storage: SessionStorage = Arc::new(MemoryStore::new());
            save_state(&*storage, &authed());
            DioxusSession::new_anonymous(storage)
        });
        assert!(!*session.ready.peek());
        assert!(
            !session.state.read().is_authenticated(),
            "first paint stays Anonymous even when storage holds a blob"
        );
        rehydrate_from_storage(&session);
        assert!(*session.ready.peek());
        assert!(
            session.state.read().is_authenticated(),
            "rehydrate must copy the persisted blob into the signal"
        );
        rsx! { "" }
    }

    #[test]
    fn new_ready_starts_past_the_rehydrate_gate() {
        let mut dom = VirtualDom::new(ReadyHarness);
        dom.rebuild_in_place();
    }

    #[component]
    fn ReadyHarness() -> Element {
        let session = use_hook(|| DioxusSession::new_ready(authed(), Arc::new(MemoryStore::new())));
        assert!(
            *session.ready.peek() && session.state.read().is_authenticated(),
            "harness sessions skip the first-paint Anonymous placeholder"
        );
        rsx! { "" }
    }
}
