//! Single-flight refresh-on-401 interceptor.
//!
//! Wraps any `(access_token) -> Future<Result<T, AuthError>>` call. If the
//! call returns 401 with the spec body [`auth_error_strings::INVALID_TOKEN`],
//! the interceptor:
//!
//! 1. Acquires a process-wide [`futures::lock::Mutex`] so only one refresh
//!    fires inside this tab, then an exclusive cross-tab lock
//!    ([`cross_tab`]). On wasm that is
//!    `navigator.locks.request('ts6-auth-refresh')`. Native builds and
//!    browsers without Web Locks use a no-op lock.
//! 2. Re-reads the token pair from storage. Tokens live in `localStorage`,
//!    shared by every tab. If the stored refresh token differs from the
//!    one this tab was about to send, another tab already rotated: adopt
//!    the stored pair into memory and replay **without** calling
//!    `/api/auth/refresh`. A cleared store means another tab logged out;
//!    adopt anonymous and do not refresh.
//! 3. Re-checks the in-memory access token — another caller in this tab
//!    may have rotated it while we waited on the mutex. If so, replay.
//! 4. Otherwise calls `POST /api/auth/refresh` once. On success, updates the
//!    [`AuthState`] (in-memory + storage). **Both the in-process mutex and
//!    the cross-tab lock are released as soon as that pair is persisted, or
//!    as soon as an adopted pair is in memory — before the original call is
//!    replayed.** A slow replay must not make other tabs wait to refresh.
//! 5. **A 401 on the refresh call re-reads storage before invalidating.**
//!    If the stored refresh token changed meanwhile, adopt it and replay
//!    instead of logging out. Invalidate only when storage still holds the
//!    token that was rejected. Other failure modes (transport, 5xx,
//!    deserialise) propagate untouched so the session survives a transient
//!    server hiccup or restart blip. Spec §6.5.3 reuse-detection only
//!    surfaces as 401 + `Invalid or expired token`; anything else means
//!    the rotation did not happen, which is recoverable on a later retry.
//!    See PURA-214 for the incident this contract was tightened against.
//!    The server will not add a grace window for a second rotation, and
//!    the at-most-one-successor fix is a separate API change — this gate
//!    is what stops the second POST.
//!
//! The refresh transport is injected via [`RefreshFn`] so unit tests can
//! exercise the locking + replay logic without touching the network.
//!
//! `futures::lock::Mutex` is runtime-agnostic: the same lock works under
//! tokio (server-side / native tests) and under wasm-bindgen-futures
//! (browser). We avoid `tokio::sync::Mutex` so this module compiles on the
//! `wasm32-unknown-unknown` target the dx-CLI builds for the SPA bundle.

use futures::lock::Mutex;
use std::future::Future;
use std::sync::Arc;

use ts6_manager_shared::auth::{RefreshRequest, TokenPairResponse, UserInfo};

use crate::client::auth::AuthError;
use crate::client::debug as auth_debug;
use crate::client::store::AuthState;

/// `BoxFuture` that the gate's trait objects return. On the browser
/// `JsFuture` is `!Send`, so the wasm build uses the non-Send variant; on
/// native, the `Send`-bearing variant flows through `tokio::spawn` correctly.
#[cfg(target_arch = "wasm32")]
type GateFuture<T> = futures::future::LocalBoxFuture<'static, Result<T, AuthError>>;
#[cfg(not(target_arch = "wasm32"))]
type GateFuture<T> = futures::future::BoxFuture<'static, Result<T, AuthError>>;

mod cross_tab;

/// Pluggable refresh transport. The default impl built on top of [`crate::client::auth::refresh`]
/// hits the real endpoint; tests inject a counting fake.
///
/// `Send + Sync` is required only on native — wasm is single-threaded and
/// `JsFuture` cannot satisfy the bound. The trait is identical otherwise.
#[cfg(target_arch = "wasm32")]
pub trait RefreshFn {
    fn refresh(&self, token: String) -> GateFuture<TokenPairResponse>;
}
#[cfg(not(target_arch = "wasm32"))]
pub trait RefreshFn: Send + Sync {
    fn refresh(&self, token: String) -> GateFuture<TokenPairResponse>;
}

/// Refresh transport that calls [`crate::client::auth::refresh`] against a
/// configured base URL. Cheap to clone (`Arc<str>` for the URL).
pub struct HttpRefresh {
    base_url: Arc<str>,
}

impl HttpRefresh {
    pub fn new(base_url: impl Into<Arc<str>>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl RefreshFn for HttpRefresh {
    fn refresh(&self, token: String) -> GateFuture<TokenPairResponse> {
        let base = self.base_url.clone();
        Box::pin(async move {
            crate::client::auth::refresh(
                &base,
                &RefreshRequest {
                    refresh_token: token,
                },
            )
            .await
        })
    }
}

/// Snapshot of the session that the gate hands to a caller's request fn.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub access: String,
    pub refresh: String,
    pub user: UserInfo,
}

/// Mutable session backing the gate. Tests construct it directly; the
/// runtime version wraps the Dioxus signal that `use_session()` exposes.
///
/// The contract: `read()` returns the live in-memory tokens, `update_pair()`
/// swaps the access/refresh in place (keeping the user) and writes storage,
/// and `invalidate()` sets the session to anonymous + clears storage.
/// `load_persisted()` re-reads the shared store (another tab may have
/// rotated). `adopt_memory()` copies that snapshot into memory without
/// writing storage back — storage is already the source of truth.
/// Signal updates are synchronous on the same task.
pub trait SessionHandle: Send + Sync {
    fn read(&self) -> AuthState;
    fn update_pair(&self, access: String, refresh: String);
    fn invalidate(&self);
    /// Re-read the token pair from the shared store.
    fn load_persisted(&self) -> AuthState;
    /// Replace in-memory state without writing storage.
    fn adopt_memory(&self, state: AuthState);
}

/// Single-flight refresh gate.
///
/// Holds the in-flight refresh mutex so only one rotation runs at a time
/// inside this process, plus a [`cross_tab::CrossTabLock`] so only one
/// rotation runs across tabs. Transport and persistence are injected so
/// tests exercise every branch without `web-sys`; they pass
/// [`cross_tab::NoopCrossTabLock`].
pub struct RefreshGate {
    session: Arc<dyn SessionHandle>,
    refresh_fn: Arc<dyn RefreshFn>,
    /// In-flight refresh lock. `futures::lock::Mutex` is runtime-agnostic
    /// so the same gate compiles under tokio tests and wasm-bindgen.
    /// Per spec §6.5.3 we never want two refreshes running concurrently
    /// inside the same browser tab.
    lock: Arc<Mutex<()>>,
    cross_tab: Arc<dyn cross_tab::CrossTabLock>,
}

impl RefreshGate {
    pub fn new(session: Arc<dyn SessionHandle>, refresh_fn: Arc<dyn RefreshFn>) -> Self {
        Self::with_cross_tab_lock(session, refresh_fn, cross_tab::platform_default())
    }

    /// Same as [`Self::new`] with an explicit cross-tab lock. Tests pass
    /// [`cross_tab::NoopCrossTabLock`] (also the native default). The wasm
    /// runtime passes a Web Locks implementation.
    pub(crate) fn with_cross_tab_lock(
        session: Arc<dyn SessionHandle>,
        refresh_fn: Arc<dyn RefreshFn>,
        cross_tab: Arc<dyn cross_tab::CrossTabLock>,
    ) -> Self {
        Self {
            session,
            refresh_fn,
            lock: Arc::new(Mutex::new(())),
            cross_tab,
        }
    }

    /// Run a request closure with refresh-on-401.
    ///
    /// `f` receives the current access token and returns a future that
    /// resolves to the request's result. The gate itself never inspects the
    /// successful payload — generic `T` flows through unchanged.
    ///
    /// Behaviour:
    /// - If the session is `Anonymous`, returns
    ///   [`AuthError::SessionAnonymous`] without calling `f` (PURA-232).
    ///   This is a client-side short-circuit, not a server 401 — callers
    ///   render it as a transient Loading state, and the route layer
    ///   redirects to `/login` if the session never materialises.
    /// - On `Unauthorized(INVALID_TOKEN)` from `f`, take the lock, possibly
    ///   refresh, replay `f` once. Replay failure terminates the session.
    /// - On any other 401 from `f` (`NO_TOKEN` / `USER_DISABLED`), invalidate
    ///   the session before propagating. A refresh cannot rescue these
    ///   sub-codes (the user is disabled / the bearer never made it to the
    ///   server) — leaving the session `Authenticated` strands the operator
    ///   on a "Session expired" banner with no path back to `/login`
    ///   ([PURA-225](/PURA/issues/PURA-225)).
    /// - On any non-401 error, propagate without touching the session.
    pub async fn run<F, Fut, T>(&self, mut f: F) -> Result<T, AuthError>
    where
        F: FnMut(SessionSnapshot) -> Fut,
        Fut: Future<Output = Result<T, AuthError>>,
    {
        let first_snapshot = match self.snapshot() {
            Some(s) => s,
            None => {
                // PURA-226 — emit the same anonymous short-circuit signal
                // the gate's contract documents. Lets the operator see
                // when a request rode the gate without a session (race
                // with a logout, rehydrate that found no blob).
                //
                // PURA-232 — return a dedicated [`AuthError::SessionAnonymous`]
                // variant instead of the server-401 envelope. The previous
                // shape (`Unauthorized(INVALID_TOKEN)`) caused
                // `ServersIndexPage` (and any other fetch-state surface) to
                // render "Session expired — Sign in again" the moment a
                // race fired between `mount_servers_context`'s `use_future`
                // and `App`'s rehydrate `use_effect`. The race is intrinsic
                // to first-paint — `provide_session` deliberately returns
                // `Anonymous` so SSR and the browser hydrate identical
                // trees, then the post-mount effect upgrades the state.
                // Re-classifying the short-circuit as a distinct,
                // self-healing transient is the smallest fix that
                // unblocks operators stuck on the bouncing /servers
                // banner after a hard-refresh.
                auth_debug::log("gate.anonymous_short_circuit", serde_json::Value::Null);
                return Err(AuthError::SessionAnonymous);
            }
        };
        let first_access = first_snapshot.access.clone();
        auth_debug::log(
            "gate.enter",
            auth_debug::fields(&[("access", auth_debug::short_token(&first_access).into())]),
        );
        let first_err = match f(first_snapshot).await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        if !first_err.is_invalid_or_expired_token() {
            // PURA-225 — A 401 whose body is not `INVALID_TOKEN`
            // (`NO_TOKEN`, `USER_DISABLED`, or an empty/unknown body) means
            // a refresh cannot help: the bearer never reached the
            // extractor, or the user row is disabled, or the upstream is
            // emitting a 401 envelope the gate doesn't recognise. In every
            // such case the session is unrecoverable on this device, so
            // invalidate immediately. The route layer's auth-gate effect
            // then bounces to `/login` instead of leaving the operator on
            // a banner-with-no-CTA. Non-401 errors fall through unchanged.
            let session_killing = first_err.is_unauthorized();
            auth_debug::log(
                if session_killing {
                    "gate.401.session_killing"
                } else {
                    "gate.non_401_propagate"
                },
                auth_debug::fields(&[
                    ("body", first_err.to_string().into()),
                    ("invalidates_session", session_killing.into()),
                ]),
            );
            if session_killing {
                self.invalidate_with_log("first_err_session_killing");
            }
            return Err(first_err);
        }

        auth_debug::log(
            "gate.401.refresh_eligible",
            auth_debug::fields(&[("body", first_err.to_string().into())]),
        );

        // 401-with-INVALID_TOKEN path. The in-process mutex coalesces this
        // tab; the cross-tab lock coalesces every tab that shares the
        // auth blob. `locked_refresh` holds both only while it decides
        // and persists (or adopts). It drops them before returning, so
        // the replay below does not stall other tabs.
        let outcome = self.locked_refresh(&first_access).await;
        match outcome {
            LockedRefresh::Stop(err) => Err(err),
            LockedRefresh::Replay { snap, reason } => {
                self.replay_snapshot(&mut f, snap, reason).await
            }
        }
    }

    /// Refresh critical section.
    ///
    /// Returns the snapshot to replay, or a terminal error. Both guards
    /// are locals of this function: they drop when it returns, which is
    /// before [`Self::run`] awaits the replayed data call.
    async fn locked_refresh(&self, first_access: &str) -> LockedRefresh {
        let _in_process = self.lock.lock().await;
        let _cross_tab = self.cross_tab.acquire().await;

        // Re-check: another task in this tab may have rotated while we
        // waited. Then re-read storage — another tab may have rotated
        // (or logged out) without touching our memory.
        let after_lock = match self.snapshot() {
            Some(s) => s,
            None => {
                // PURA-232 — same SessionAnonymous classification as the
                // first-snapshot branch above. The session was invalidated
                // (logout, peer 401 sweep) between our initial read and
                // our lock acquisition; this is the SPA's own state
                // change, not a server 401. Render as Loading; the route
                // guard will bounce to `/login` on the next render.
                auth_debug::log("gate.session_lost_under_lock", serde_json::Value::Null);
                return LockedRefresh::Stop(AuthError::SessionAnonymous);
            }
        };
        let stored = self.session.load_persisted();
        if persisted_refresh_differs(&stored, &after_lock.refresh) {
            // Another tab already rotated. Adopt its pair and replay.
            // Do not POST the refresh token we were about to send — that
            // token is already a predecessor, and a second rotation is
            // reuse detection.
            self.session.adopt_memory(stored);
            let Some(snap) = self.snapshot() else {
                auth_debug::log("gate.stored_session_cleared", serde_json::Value::Null);
                return LockedRefresh::Stop(AuthError::SessionAnonymous);
            };
            auth_debug::log(
                "gate.replay_with_stored_rotation",
                auth_debug::fields(&[("access", auth_debug::short_token(&snap.access).into())]),
            );
            return LockedRefresh::Replay {
                snap,
                reason: "stored_rotation_replay_401",
            };
        }
        if matches!(stored, AuthState::Anonymous) {
            // Another tab cleared the blob (logout / session-killing
            // 401). Refreshing the in-memory token would present a
            // credential the user just discarded.
            auth_debug::log("gate.adopt_stored_anonymous", serde_json::Value::Null);
            self.session.adopt_memory(AuthState::Anonymous);
            return LockedRefresh::Stop(AuthError::SessionAnonymous);
        }
        if after_lock.access != first_access {
            // Another caller in this tab already rotated — skip refresh,
            // replay with the fresh access token. PURA-225 — a 401 on the
            // replay means even the freshly rotated access token was
            // rejected; that is a session-killing signal regardless of
            // sub-code, so kill it before returning so the route layer
            // can bounce.
            auth_debug::log(
                "gate.replay_with_peer_rotation",
                auth_debug::fields(&[(
                    "access",
                    auth_debug::short_token(&after_lock.access).into(),
                )]),
            );
            return LockedRefresh::Replay {
                snap: after_lock,
                reason: "peer_rotation_replay_401",
            };
        }

        // We are the rotator. Issue the refresh once. The POST stays
        // inside the lock; the replay does not.
        auth_debug::log("gate.refresh.start", serde_json::Value::Null);
        let started = auth_debug::now_ms_for_duration();
        let sent_refresh = after_lock.refresh.clone();
        match self.refresh_fn.refresh(sent_refresh.clone()).await {
            Ok(pair) => {
                auth_debug::log(
                    "gate.refresh.ok",
                    auth_debug::fields(&[
                        (
                            "new_access",
                            auth_debug::short_token(&pair.access_token).into(),
                        ),
                        ("duration_ms", auth_debug::elapsed_ms(started).into()),
                    ]),
                );
                self.update_pair_with_log(pair.access_token.clone(), pair.refresh_token.clone());
                let snap = SessionSnapshot {
                    access: pair.access_token,
                    refresh: pair.refresh_token,
                    user: after_lock.user,
                };
                // PURA-225 — same defense-in-depth as the parallel-rotator
                // branch: if the replay (with a brand-new access token)
                // still 401s, the session is dead at the server. Invalidate
                // so AppShell bounces instead of looping `data 401 →
                // refresh → replay 401 → propagate Unauthorized → stuck
                // banner` on every render. That invalidate happens in
                // `replay_snapshot`, after these guards have dropped.
                LockedRefresh::Replay {
                    snap,
                    reason: "post_refresh_replay_401",
                }
            }
            Err(e) => {
                // PURA-214 — only 401 on the refresh response means the
                // session is unrecoverably dead (token replayed, family
                // revoked, owning user disabled). Any other refresh
                // failure (transport blip, timed-out fetch, 5xx from a
                // restarting upstream, 502 through a proxy, JSON parse
                // fail) is a transient signal — the rotation did not
                // happen, but the refresh token is still valid in the DB.
                // Keep the session authenticated so a later attempt can try
                // again; propagate the raw error so the caller can render
                // the right banner. A timeout is [`AuthError::Transport`],
                // not a 401, so it takes this keep-alive path.
                //
                // A client-side refresh timeout does not cancel the
                // rotation on the server. Do not POST again inside this
                // hold — return and drop both locks. The next attempt
                // re-reads storage before any new refresh, and adopts a
                // pair another tab already stored.
                //
                // A 401 is not automatically fatal across tabs: a peer may
                // have won the rotation and written a successor while our
                // POST was in flight (no Web Lock, lock-wait timeout, or
                // the server accepted both). Re-read before invalidating.
                // Invalidate only when storage still holds the token that
                // was rejected — a blind invalidate would `remove` the
                // successor the peer just stored and log every tab out.
                let kept_alive = !e.is_unauthorized();
                auth_debug::log(
                    "gate.refresh.fail",
                    auth_debug::fields(&[
                        ("err", e.to_string().into()),
                        ("kept_session_alive", kept_alive.into()),
                        ("duration_ms", auth_debug::elapsed_ms(started).into()),
                    ]),
                );
                if e.is_unauthorized() {
                    let stored_after = self.session.load_persisted();
                    if persisted_refresh_differs(&stored_after, &sent_refresh) {
                        self.session.adopt_memory(stored_after);
                        if let Some(snap) = self.snapshot() {
                            auth_debug::log(
                                "gate.refresh_401_adopt_peer",
                                auth_debug::fields(&[(
                                    "access",
                                    auth_debug::short_token(&snap.access).into(),
                                )]),
                            );
                            return LockedRefresh::Replay {
                                snap,
                                reason: "refresh_401_adopted_replay_401",
                            };
                        }
                        auth_debug::log("gate.stored_session_cleared", serde_json::Value::Null);
                        return LockedRefresh::Stop(e);
                    }
                    if persisted_holds_refresh(&stored_after, &sent_refresh) {
                        self.invalidate_with_log("refresh_401");
                    } else {
                        auth_debug::log("gate.refresh_401_storage_moved", serde_json::Value::Null);
                        self.session.adopt_memory(stored_after);
                    }
                }
                LockedRefresh::Stop(e)
            }
        }
    }

    /// Replay `f` with `snap`. A 401 on the replay invalidates: the access
    /// token we just adopted or minted was rejected, so the session cannot
    /// be recovered by another refresh.
    async fn replay_snapshot<F, Fut, T>(
        &self,
        f: &mut F,
        snap: SessionSnapshot,
        invalidate_reason: &'static str,
    ) -> Result<T, AuthError>
    where
        F: FnMut(SessionSnapshot) -> Fut,
        Fut: Future<Output = Result<T, AuthError>>,
    {
        let result = f(snap).await;
        if let Err(err) = &result
            && err.is_unauthorized()
        {
            auth_debug::log(
                "gate.replay_401_invalidate",
                auth_debug::fields(&[("body", err.to_string().into())]),
            );
            self.invalidate_with_log(invalidate_reason);
        }
        result
    }

    /// PURA-226 — single-point breadcrumb wrapper around
    /// [`SessionHandle::invalidate`]. Every gate-driven invalidation
    /// flows through here so the debug capture lists `session.invalidate`
    /// with a `reason` field that names the branch (refresh 401,
    /// post-refresh replay 401, first-error session-killing, peer
    /// rotation replay 401). The session impl itself stays free of
    /// instrumentation so both `DioxusSession` (runtime) and
    /// `InMemorySession` (tests) emit identical breadcrumbs.
    fn invalidate_with_log(&self, reason: &'static str) {
        auth_debug::log(
            "session.invalidate",
            auth_debug::fields(&[("reason", reason.into())]),
        );
        self.session.invalidate();
    }

    /// PURA-226 — single-point breadcrumb wrapper around
    /// [`SessionHandle::update_pair`]. Covers PURA-225's candidate
    /// failure #3 (refresh succeeded but rotation was racy / dropped
    /// at the storage layer) by surfacing both the new-access prefix
    /// and the fact the gate handed the rotation off to the session.
    fn update_pair_with_log(&self, access: String, refresh: String) {
        auth_debug::log(
            "session.update_pair",
            auth_debug::fields(&[("new_access", auth_debug::short_token(&access).into())]),
        );
        self.session.update_pair(access, refresh);
    }

    fn snapshot(&self) -> Option<SessionSnapshot> {
        match self.session.read() {
            AuthState::Authenticated {
                access,
                refresh,
                user,
            } => Some(SessionSnapshot {
                access,
                refresh,
                user,
            }),
            AuthState::Anonymous => None,
        }
    }

    /// `true` when the in-process refresh mutex is free. Tests use this
    /// from the replay closure to prove the mutex dropped before `f`.
    #[cfg(test)]
    fn in_process_unlocked(&self) -> bool {
        self.lock.try_lock().is_some()
    }
}

/// What [`RefreshGate::locked_refresh`] decided while it held both locks.
/// The replay arm is executed only after those guards have dropped.
enum LockedRefresh {
    Replay {
        snap: SessionSnapshot,
        reason: &'static str,
    },
    Stop(AuthError),
}

/// `true` when `stored` is authenticated with a refresh token other than
/// `rejected`. Anonymous is not a "different pair" — there is nothing to
/// replay with. Callers handle that branch on its own.
fn persisted_refresh_differs(stored: &AuthState, rejected: &str) -> bool {
    match stored.refresh_token() {
        Some(refresh) => refresh != rejected,
        None => false,
    }
}

/// `true` when storage still contains the refresh token the server just
/// rejected. That is the only case where invalidating is safe: it clears
/// the credential that failed, and does not delete a successor another
/// tab has since written.
fn persisted_holds_refresh(stored: &AuthState, rejected: &str) -> bool {
    stored.refresh_token() == Some(rejected)
}

// ---------------------------------------------------------------------------
// In-memory `SessionHandle` for tests. The Dioxus-Signal-backed handle
// lives in `crate::client::session::dioxus` (added in the /login route
// follow-up).
//
// We expose the test handle alongside the trait so unit tests can exercise
// the whole gate end-to-end without dragging Dioxus into the test scope.

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::client::storage::Storage;
    use crate::client::store::save_state;
    use std::sync::Mutex as StdMutex;

    pub struct InMemorySession {
        pub state: StdMutex<AuthState>,
        pub storage: Arc<dyn Storage + Send + Sync>,
    }

    impl InMemorySession {
        pub fn new(initial: AuthState, storage: Arc<dyn Storage + Send + Sync>) -> Self {
            save_state(&*storage, &initial);
            Self {
                state: StdMutex::new(initial),
                storage,
            }
        }
    }

    impl SessionHandle for InMemorySession {
        fn read(&self) -> AuthState {
            self.state.lock().unwrap().clone()
        }
        fn update_pair(&self, access: String, refresh: String) {
            let mut g = self.state.lock().unwrap();
            if let AuthState::Authenticated { user, .. } = &*g {
                let user = user.clone();
                *g = AuthState::Authenticated {
                    access,
                    refresh,
                    user,
                };
                save_state(&*self.storage, &g);
            }
        }
        fn invalidate(&self) {
            let mut g = self.state.lock().unwrap();
            *g = AuthState::Anonymous;
            save_state(&*self.storage, &g);
        }
        fn load_persisted(&self) -> AuthState {
            crate::client::store::load_state(&*self.storage)
        }
        fn adopt_memory(&self, state: AuthState) {
            *self.state.lock().unwrap() = state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::{MemoryStore, Storage};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::testing::InMemorySession;

    fn user() -> UserInfo {
        UserInfo {
            id: 1,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: "viewer".into(),
        }
    }

    fn authed(access: &str, refresh: &str) -> AuthState {
        AuthState::Authenticated {
            access: access.into(),
            refresh: refresh.into(),
            user: user(),
        }
    }

    /// Refresh stub: each call increments `count` and returns whatever
    /// `responder` decides for the call number. Lets a test see exactly
    /// how many refreshes ran during a concurrent burst.
    struct StubRefresh {
        count: AtomicU32,
        responder: Box<dyn Fn(u32) -> Result<TokenPairResponse, AuthError> + Send + Sync>,
    }

    impl StubRefresh {
        fn new(
            responder: impl Fn(u32) -> Result<TokenPairResponse, AuthError> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                count: AtomicU32::new(0),
                responder: Box::new(responder),
            })
        }
        fn calls(&self) -> u32 {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl RefreshFn for StubRefresh {
        fn refresh(
            &self,
            _token: String,
        ) -> futures::future::BoxFuture<'static, Result<TokenPairResponse, AuthError>> {
            let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            let result = (self.responder)(n);
            Box::pin(async move { result })
        }
    }

    fn arc_storage(s: MemoryStore) -> Arc<dyn Storage + Send + Sync> {
        Arc::new(s)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_call_succeeds_without_refresh() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session, stub.clone());

        let out = gate
            .run(|snap| async move {
                assert_eq!(snap.access, "ax");
                Ok::<_, AuthError>(42u32)
            })
            .await
            .unwrap();
        assert_eq!(out, 42);
        assert_eq!(stub.calls(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn anonymous_session_short_circuits_with_session_anonymous_error() {
        // PURA-232 regression — the gate's Anonymous short-circuit MUST
        // return a dedicated [`AuthError::SessionAnonymous`] variant, not
        // a synthetic `Unauthorized("Invalid or expired token")`. Pages
        // map the latter to "Session expired — Sign in again", which
        // strands operators in a bouncing banner the moment
        // `mount_servers_context`'s `use_future` polls before
        // `rehydrate_from_storage` upgrades the session signal.
        //
        // Pinned invariants:
        //   - error variant is `SessionAnonymous`
        //   - `is_unauthorized()` returns false (this is not a server 401)
        //   - `is_invalid_or_expired_token()` returns false (no refresh
        //     dance — there's no token to refresh)
        //   - refresh fn never fires
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(AuthState::Anonymous, storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session, stub.clone());

        let err = gate
            .run(|_| async { Ok::<u32, AuthError>(0) })
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::SessionAnonymous),
            "anonymous short-circuit must return SessionAnonymous, got: {err:?}"
        );
        assert!(
            !err.is_unauthorized(),
            "SessionAnonymous must not be classified as a server 401: {err}"
        );
        assert!(
            !err.is_invalid_or_expired_token(),
            "SessionAnonymous must not trigger refresh-eligible classification: {err}"
        );
        assert_eq!(stub.calls(), 0);
    }

    /// PURA-232 — the Anonymous short-circuit MUST NOT call
    /// `session.invalidate()`. The session is already Anonymous, so the
    /// op is a no-op at best, and at worst it pollutes the breadcrumb
    /// trail and triggers spurious storage writes. Pin the no-op
    /// invariant by asserting the session and storage are untouched
    /// after the short-circuit fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn anonymous_short_circuit_does_not_invalidate_or_touch_storage() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(AuthState::Anonymous, storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session.clone(), stub.clone());

        // Sanity: no persisted blob to start with.
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_none(),
            "fresh storage should be empty"
        );

        let _ = gate.run(|_| async { Ok::<u32, AuthError>(0) }).await;

        // Session stays Anonymous; storage stays empty. No
        // `session.invalidate()` fired (which would have called
        // `save_state(Anonymous)` and is a no-op here but would still
        // surface in instrumentation as a misleading
        // `session.invalidate` breadcrumb).
        assert_eq!(session.read(), AuthState::Anonymous);
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_none(),
            "anonymous short-circuit must not write to storage"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refreshes_once_and_replays_on_401_invalid_token() {
        use ts6_manager_shared::auth::auth_error_strings as msg;
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|n| {
            assert_eq!(n, 1, "exactly one refresh expected");
            Ok(TokenPairResponse {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let out = gate
            .run(move |snap| {
                let calls = calls_clone.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        assert_eq!(snap.access, "old-access");
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        assert_eq!(snap.access, "new-access");
                        Ok::<u32, AuthError>(7)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 7);
        assert_eq!(stub.calls(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Session must be persisted with the rotated tokens.
        match session.read() {
            AuthState::Authenticated {
                access, refresh, ..
            } => {
                assert_eq!(access, "new-access");
                assert_eq!(refresh, "new-refresh");
            }
            _ => panic!("session should still be authenticated"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refresh_failure_invalidates_session_no_silent_retry() {
        use ts6_manager_shared::auth::auth_error_strings as msg;
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into())));
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let req_calls = Arc::new(AtomicU32::new(0));
        let req_clone = req_calls.clone();
        let err = gate
            .run(move |_| {
                let req = req_clone.clone();
                async move {
                    req.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                }
            })
            .await
            .unwrap_err();
        assert!(err.is_unauthorized());
        // Refresh fired exactly once — never silently retry.
        assert_eq!(stub.calls(), 1);
        // Original request fired once (initial 401 only). The replay
        // never happened because refresh failed.
        assert_eq!(req_calls.load(Ordering::SeqCst), 1);
        // Session is wiped + storage cleared.
        assert_eq!(session.read(), AuthState::Anonymous);
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_none()
        );
    }

    /// PURA-214 regression — a 5xx, transport blip, or deserialise failure
    /// from `/api/auth/refresh` MUST NOT bounce the user to /login. The
    /// rotation didn't happen but the refresh token is still live in the
    /// DB; the next call retries naturally and the session keeps running.
    ///
    /// Before the fix this test fails: the gate invalidated on any refresh
    /// error and the user lost their session on every restart-blip during
    /// the access-token expiry window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transient_refresh_failure_keeps_session_authenticated() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        for transient in [
            AuthError::Server {
                status: 502,
                message: "Bad Gateway".into(),
            },
            AuthError::Server {
                status: 503,
                message: "Service Unavailable".into(),
            },
            AuthError::Transport("fetch failed".into()),
            AuthError::Deserialise("not json".into()),
        ] {
            let storage = arc_storage(MemoryStore::new());
            let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
                authed("old-access", "old-refresh"),
                storage.clone(),
            ));
            let err_template = transient.clone();
            let stub = StubRefresh::new(move |_| Err(err_template.clone()));
            let gate = RefreshGate::new(session.clone(), stub.clone());

            let req_calls = Arc::new(AtomicU32::new(0));
            let req_clone = req_calls.clone();
            let err = gate
                .run(move |_| {
                    let req = req_clone.clone();
                    async move {
                        req.fetch_add(1, Ordering::SeqCst);
                        Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    }
                })
                .await
                .unwrap_err();
            // Raw refresh error surfaces — caller renders the right banner
            // instead of "session expired".
            match (&transient, &err) {
                (AuthError::Server { status: a, .. }, AuthError::Server { status: b, .. }) => {
                    assert_eq!(a, b)
                }
                (AuthError::Transport(_), AuthError::Transport(_)) => {}
                (AuthError::Deserialise(_), AuthError::Deserialise(_)) => {}
                (a, b) => panic!("variant mismatch — transient: {a:?}, got: {b:?}"),
            }
            assert!(
                !err.is_unauthorized(),
                "transient refresh failure must not surface as 401: {err:?}"
            );
            assert_eq!(stub.calls(), 1, "refresh fired exactly once");
            assert_eq!(
                req_calls.load(Ordering::SeqCst),
                1,
                "original request fired once (initial 401), no replay"
            );
            // Critical invariant: session stays Authenticated so the next
            // call retries instead of bouncing through /login.
            assert!(
                session.read().is_authenticated(),
                "transient refresh failure must NOT invalidate the session ({transient:?})"
            );
            assert!(
                storage
                    .get(crate::client::store::SESSION_STORAGE_KEY)
                    .is_some(),
                "persisted session blob must survive transient refresh failure"
            );
        }
    }

    /// PURA-214 — a 4xx that ISN'T a 401 (e.g. server returned 400 because
    /// of a malformed request, an outdated client, or a deployment skew)
    /// is also not a "session is dead" signal. Don't invalidate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_401_4xx_refresh_failure_keeps_session_authenticated() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| {
            Err(AuthError::Client {
                status: 400,
                message: "Bad Request".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let err = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into())) })
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Client { status: 400, .. }));
        assert!(
            session.read().is_authenticated(),
            "non-401 4xx must NOT kill the session"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn other_errors_do_not_trigger_refresh_or_invalidate() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let err = gate
            .run(|_| async {
                Err::<u32, _>(AuthError::Server {
                    status: 500,
                    message: "boom".into(),
                })
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Server { .. }));
        assert_eq!(stub.calls(), 0);
        assert!(session.read().is_authenticated());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_a_single_refresh() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|n| {
            assert_eq!(n, 1, "single-flight: exactly one refresh");
            Ok(TokenPairResponse {
                access_token: "fresh-access".into(),
                refresh_token: "fresh-refresh".into(),
            })
        });
        let gate = Arc::new(RefreshGate::new(session.clone(), stub.clone()));

        // Each caller's request fn:
        //   call #1 with old-access → 401 (forces refresh)
        //   call #2 with fresh-access → success
        // The state machine guarantees: refresh runs once, every caller
        // eventually sees `fresh-access` on its second attempt.
        const CALLERS: usize = 8;
        let attempts_per_caller = Arc::new(AtomicU32::new(0));
        let mut joins = Vec::with_capacity(CALLERS);
        for i in 0..CALLERS {
            let gate = gate.clone();
            let attempts = attempts_per_caller.clone();
            joins.push(tokio::spawn(async move {
                let attempts = attempts.clone();
                gate.run(move |snap| {
                    let attempts = attempts.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        if snap.access == "old-access" {
                            Err::<usize, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                        } else {
                            assert_eq!(snap.access, "fresh-access");
                            Ok(i)
                        }
                    }
                })
                .await
            }));
        }
        for j in joins {
            j.await.unwrap().unwrap();
        }
        // Refresh ran exactly once for the whole burst.
        assert_eq!(stub.calls(), 1);
        // Every caller eventually saw the fresh token.
        match session.read() {
            AuthState::Authenticated { access, .. } => assert_eq!(access, "fresh-access"),
            _ => panic!("session should be authenticated after burst"),
        }
    }

    /// Another tab rotated the shared refresh token before this tab called
    /// `/api/auth/refresh`. The gate re-reads storage under the lock, adopts
    /// the stored pair, and replays with the new access token. The refresh
    /// transport must not run — posting the predecessor is what the server
    /// treats as replay.
    ///
    /// Runs on the native no-op cross-tab lock, which is also the fallback
    /// when `navigator.locks` is missing: the re-read does not depend on
    /// Web Locks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stored_refresh_changed_before_refresh_skips_refresh_and_replays() {
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        // Peer tab wrote a successor. This tab's memory is still the
        // predecessor that just 401'd.
        save_state(&*storage, &authed("peer-access", "peer-refresh"));
        let stub = StubRefresh::new(|_| panic!("must not refresh a predecessor"));
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let out = gate
            .run(move |snap| {
                let calls = calls_clone.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        assert_eq!(snap.access, "old-access");
                        assert_eq!(snap.refresh, "old-refresh");
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        assert_eq!(snap.access, "peer-access");
                        assert_eq!(snap.refresh, "peer-refresh");
                        Ok::<u32, AuthError>(9)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 9);
        assert_eq!(stub.calls(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        match session.read() {
            AuthState::Authenticated {
                access, refresh, ..
            } => {
                assert_eq!(access, "peer-access");
                assert_eq!(refresh, "peer-refresh");
            }
            AuthState::Anonymous => panic!("adopting a stored pair must not log out"),
        }
        // Adopting must not rewrite storage (a racing peer write would be
        // clobbered). The successor blob is still there.
        let raw = storage
            .get(crate::client::store::SESSION_STORAGE_KEY)
            .expect("stored successor must survive adopt");
        assert!(raw.contains("peer-refresh"), "{raw}");
    }

    /// Refresh itself 401'd, but storage now holds a newer pair (the other
    /// tab won the rotation). Adopt that pair and replay; do not invalidate,
    /// which would delete the successor from localStorage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refresh_401_newer_stored_pair_adopts_and_does_not_invalidate() {
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let storage_for_peer = storage.clone();
        let stub = StubRefresh::new(move |n| {
            assert_eq!(n, 1, "exactly one refresh of the predecessor");
            save_state(&*storage_for_peer, &authed("newer-access", "newer-refresh"));
            Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let out = gate
            .run(move |snap| {
                let calls = calls_clone.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        assert_eq!(snap.access, "old-access");
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        assert_eq!(snap.access, "newer-access");
                        Ok::<u32, AuthError>(4)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 4);
        assert_eq!(stub.calls(), 1);
        assert!(
            session.read().is_authenticated(),
            "a newer stored pair must not invalidate the session"
        );
        match session.read() {
            AuthState::Authenticated {
                access, refresh, ..
            } => {
                assert_eq!(access, "newer-access");
                assert_eq!(refresh, "newer-refresh");
            }
            AuthState::Anonymous => panic!("session was invalidated"),
        }
        let raw = storage
            .get(crate::client::store::SESSION_STORAGE_KEY)
            .expect("successor blob must not be removed");
        assert!(raw.contains("newer-refresh"), "{raw}");
    }

    /// Refresh 401 and the stored refresh token is still the one that was
    /// rejected (including a same-value rewrite from another waiter). That
    /// is the existing session-killing path: invalidate memory and storage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn refresh_401_and_storage_unchanged_invalidates_session() {
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let storage_for_same = storage.clone();
        let stub = StubRefresh::new(move |_| {
            // Re-read must observe the rejected token, not "storage was
            // touched". Writing the same pair still invalidates.
            save_state(&*storage_for_same, &authed("old-access", "old-refresh"));
            Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let err = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into())) })
            .await
            .unwrap_err();
        assert!(err.is_unauthorized());
        assert_eq!(stub.calls(), 1);
        assert_eq!(session.read(), AuthState::Anonymous);
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_none(),
            "rejected token must be cleared"
        );
    }

    /// The cross-tab lock trait is what tests substitute. Happy-path calls
    /// do not acquire it; the refresh critical section acquires it once,
    /// including when the storage re-read then skips the HTTP refresh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cross_tab_lock_wraps_only_the_refresh_critical_section() {
        use super::cross_tab::{CrossTabGuard, CrossTabLock, LockFuture};
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        struct CountingLock {
            acquires: AtomicU32,
        }
        impl CrossTabLock for CountingLock {
            fn acquire(&self) -> LockFuture<CrossTabGuard> {
                self.acquires.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { CrossTabGuard::noop() })
            }
        }

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let lock = Arc::new(CountingLock {
            acquires: AtomicU32::new(0),
        });
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::with_cross_tab_lock(session.clone(), stub.clone(), lock.clone());
        gate.run(|_| async { Ok::<u32, AuthError>(1) })
            .await
            .unwrap();
        assert_eq!(lock.acquires.load(Ordering::SeqCst), 0);

        save_state(&*storage, &authed("peer-access", "peer-refresh"));
        let stub = StubRefresh::new(|_| panic!("stored successor must skip refresh"));
        let gate = RefreshGate::with_cross_tab_lock(session.clone(), stub, lock.clone());
        gate.run(|snap| async move {
            if snap.access == "ax" {
                Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
            } else {
                assert_eq!(snap.access, "peer-access");
                Ok::<u32, AuthError>(2)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            lock.acquires.load(Ordering::SeqCst),
            1,
            "refresh section takes the cross-tab lock once"
        );
    }

    /// Both guards drop once the new pair is persisted, before the replay
    /// closure runs. A slow `f` must not keep the cross-tab lock or the
    /// in-process mutex.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn locks_drop_before_replay_after_persisted_refresh() {
        use super::cross_tab::{CrossTabGuard, CrossTabLock, LockFuture};
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let released = Arc::new(AtomicBool::new(false));
        struct RecordingLock {
            released: Arc<AtomicBool>,
        }
        impl CrossTabLock for RecordingLock {
            fn acquire(&self) -> LockFuture<CrossTabGuard> {
                let released = self.released.clone();
                Box::pin(async move {
                    CrossTabGuard::with_drop_hook(move || {
                        released.store(true, Ordering::SeqCst);
                    })
                })
            }
        }

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let lock = Arc::new(RecordingLock {
            released: released.clone(),
        });
        let stub = StubRefresh::new(|_| {
            Ok(TokenPairResponse {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
            })
        });
        let gate = Arc::new(RefreshGate::with_cross_tab_lock(
            session.clone(),
            stub.clone(),
            lock,
        ));
        let gate_for_replay = gate.clone();
        let released_for_replay = released.clone();
        let out = gate
            .run(move |snap| {
                let gate_for_replay = gate_for_replay.clone();
                let released_for_replay = released_for_replay.clone();
                async move {
                    if snap.access == "old-access" {
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        assert!(
                            released_for_replay.load(Ordering::SeqCst),
                            "cross-tab guard must drop before the replay closure"
                        );
                        assert!(
                            gate_for_replay.in_process_unlocked(),
                            "in-process mutex must drop before the replay closure"
                        );
                        assert_eq!(snap.access, "new-access");
                        Ok::<u32, AuthError>(7)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 7);
        assert_eq!(stub.calls(), 1);
        assert!(released.load(Ordering::SeqCst));
    }

    /// Adopting a peer pair also drops both guards before replay, with no
    /// refresh POST in between.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn locks_drop_before_replay_after_adopted_pair() {
        use super::cross_tab::{CrossTabGuard, CrossTabLock, LockFuture};
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let released = Arc::new(AtomicBool::new(false));
        struct RecordingLock {
            released: Arc<AtomicBool>,
        }
        impl CrossTabLock for RecordingLock {
            fn acquire(&self) -> LockFuture<CrossTabGuard> {
                let released = self.released.clone();
                Box::pin(async move {
                    CrossTabGuard::with_drop_hook(move || {
                        released.store(true, Ordering::SeqCst);
                    })
                })
            }
        }

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        save_state(&*storage, &authed("peer-access", "peer-refresh"));
        let lock = Arc::new(RecordingLock {
            released: released.clone(),
        });
        let stub = StubRefresh::new(|_| panic!("adopted pair must skip refresh"));
        let gate = Arc::new(RefreshGate::with_cross_tab_lock(
            session.clone(),
            stub.clone(),
            lock,
        ));
        let gate_for_replay = gate.clone();
        let released_for_replay = released.clone();
        let out = gate
            .run(move |snap| {
                let gate_for_replay = gate_for_replay.clone();
                let released_for_replay = released_for_replay.clone();
                async move {
                    if snap.access == "old-access" {
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        assert!(
                            released_for_replay.load(Ordering::SeqCst),
                            "cross-tab guard must drop before the replay closure"
                        );
                        assert!(
                            gate_for_replay.in_process_unlocked(),
                            "in-process mutex must drop before the replay closure"
                        );
                        assert_eq!(snap.access, "peer-access");
                        Ok::<u32, AuthError>(8)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 8);
        assert_eq!(stub.calls(), 0);
        assert!(released.load(Ordering::SeqCst));
    }

    /// A timed-out `navigator.locks.request` yields a no-op guard (the
    /// wasm acquire path does this when the AbortSignal fires). The gate
    /// still re-reads storage and, when the blob is unchanged, refreshes.
    /// It must not log out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn timed_out_lock_falls_back_to_reread_then_refresh() {
        use super::cross_tab::{CrossTabGuard, CrossTabLock, LockFuture};

        struct TimeoutFallbackLock;
        impl CrossTabLock for TimeoutFallbackLock {
            fn acquire(&self) -> LockFuture<CrossTabGuard> {
                Box::pin(async { CrossTabGuard::fallback() })
            }
        }

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| {
            Ok(TokenPairResponse {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
            })
        });
        let gate = RefreshGate::with_cross_tab_lock(
            session.clone(),
            stub.clone(),
            Arc::new(TimeoutFallbackLock),
        );
        let out = gate
            .run(|snap| async move {
                if snap.access == "old-access" {
                    Err(AuthError::Unauthorized(
                        ts6_manager_shared::auth::auth_error_strings::INVALID_TOKEN.into(),
                    ))
                } else {
                    assert_eq!(snap.access, "new-access");
                    Ok::<u32, AuthError>(3)
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 3);
        assert_eq!(stub.calls(), 1, "unchanged storage still refreshes");
        match session.read() {
            AuthState::Authenticated {
                access, refresh, ..
            } => {
                assert_eq!(access, "new-access");
                assert_eq!(refresh, "new-refresh");
            }
            AuthState::Anonymous => panic!("lock timeout must not log out"),
        }
    }

    /// Same fallback when a peer already wrote a successor: re-read adopts
    /// it and does not refresh or invalidate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn timed_out_lock_rereads_peer_rotation_without_logout() {
        use super::cross_tab::{CrossTabGuard, CrossTabLock, LockFuture};
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        struct TimeoutFallbackLock;
        impl CrossTabLock for TimeoutFallbackLock {
            fn acquire(&self) -> LockFuture<CrossTabGuard> {
                Box::pin(async { CrossTabGuard::fallback() })
            }
        }

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        save_state(&*storage, &authed("peer-access", "peer-refresh"));
        let stub = StubRefresh::new(|_| panic!("peer successor must skip refresh"));
        let gate = RefreshGate::with_cross_tab_lock(
            session.clone(),
            stub.clone(),
            Arc::new(TimeoutFallbackLock),
        );
        let out = gate
            .run(|snap| async move {
                if snap.access == "old-access" {
                    Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                } else {
                    assert_eq!(snap.access, "peer-access");
                    Ok::<u32, AuthError>(5)
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 5);
        assert_eq!(stub.calls(), 0);
        assert!(session.read().is_authenticated());
    }

    /// A refresh fetch that hits its abort deadline is a transport error.
    /// The session stays authenticated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn timed_out_refresh_is_transport_and_does_not_invalidate() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let timeout_err = crate::client::auth::refresh_aborted_error("AbortError");
        let stub = StubRefresh::new({
            let timeout_err = timeout_err.clone();
            move |_| Err(timeout_err.clone())
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());
        let err = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into())) })
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::Transport(ref msg) if msg.contains("refresh timed out")),
            "timeout must surface as transport, got {err:?}"
        );
        assert!(
            !err.is_unauthorized(),
            "a timed-out refresh must not look like a 401"
        );
        assert_eq!(stub.calls(), 1);
        assert!(
            session.read().is_authenticated(),
            "a timed-out refresh must not invalidate the session"
        );
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_some(),
            "persisted session blob must survive a timed-out refresh"
        );
    }

    /// A timed-out refresh does not cancel the server rotation, so this
    /// hold must not POST again. The lock drops with the error. A later
    /// attempt re-reads storage first and adopts a pair another tab stored,
    /// instead of sending the predecessor token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn timed_out_refresh_does_not_post_again_in_the_same_hold() {
        use crate::client::store::save_state;
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|n| {
            assert_eq!(
                n, 1,
                "timed-out refresh must not POST again inside the same critical section"
            );
            Err(crate::client::auth::refresh_aborted_error("AbortError"))
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let err = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into())) })
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Transport(_)), "got {err:?}");
        assert!(!err.is_unauthorized());
        assert_eq!(stub.calls(), 1);
        assert!(session.read().is_authenticated());

        // The hold has ended. Another tab finished the rotation and wrote
        // the successor. The next attempt must adopt it and not POST.
        save_state(&*storage, &authed("peer-access", "peer-refresh"));
        let out = gate
            .run(|snap| async move {
                if snap.access == "old-access" {
                    Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                } else {
                    assert_eq!(snap.access, "peer-access");
                    assert_eq!(snap.refresh, "peer-refresh");
                    Ok::<u32, AuthError>(11)
                }
            })
            .await
            .unwrap();
        assert_eq!(out, 11);
        assert_eq!(
            stub.calls(),
            1,
            "later attempt adopts the stored pair and does not refresh"
        );
        match session.read() {
            AuthState::Authenticated {
                access, refresh, ..
            } => {
                assert_eq!(access, "peer-access");
                assert_eq!(refresh, "peer-refresh");
            }
            AuthState::Anonymous => panic!("adopting after a timeout must not log out"),
        }
    }

    /// PURA-225 regression — when the data endpoint returns a 401 whose
    /// body is `USER_DISABLED` or `NO_TOKEN` (anything except the spec
    /// [`msg::INVALID_TOKEN`] envelope), refresh cannot help: the session
    /// is unrecoverable on this device. The gate MUST invalidate the
    /// session before propagating so the route layer's auth-gate effect
    /// bounces the operator to `/login`.
    ///
    /// Before the fix the gate propagated the error verbatim and left the
    /// session `Authenticated`, stranding the operator on a "Session
    /// expired" banner with no path forward.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_invalid_token_data_401_invalidates_session() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        for body in [msg::USER_DISABLED, msg::NO_TOKEN] {
            let storage = arc_storage(MemoryStore::new());
            let session: Arc<dyn SessionHandle> =
                Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
            let stub = StubRefresh::new(|_| panic!("must not refresh on non-INVALID_TOKEN 401"));
            let gate = RefreshGate::new(session.clone(), stub.clone());

            let err = gate
                .run(|_| {
                    let body = body.to_owned();
                    async move { Err::<u32, _>(AuthError::Unauthorized(body)) }
                })
                .await
                .unwrap_err();
            assert!(err.is_unauthorized());
            assert!(
                !err.is_invalid_or_expired_token(),
                "test contract: body must not be INVALID_TOKEN, got: {err}"
            );
            assert_eq!(stub.calls(), 0, "refresh must not fire for {body}");
            // Critical invariant added by PURA-225 — these sub-codes are
            // session-killing.
            assert_eq!(
                session.read(),
                AuthState::Anonymous,
                "401 with body `{body}` must invalidate the session"
            );
            assert!(
                storage
                    .get(crate::client::store::SESSION_STORAGE_KEY)
                    .is_none(),
                "persisted session blob must be cleared on session-killing 401"
            );
        }
    }

    /// PURA-225 — non-401 errors keep the session alive (regression guard
    /// for the same fix above). A 4xx or 5xx from a data endpoint is not a
    /// session-killing signal; only `is_unauthorized()` is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_401_data_errors_do_not_invalidate_session() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh on non-401"));
        let gate = RefreshGate::new(session.clone(), stub.clone());

        for err_fixture in [
            AuthError::Server {
                status: 500,
                message: "boom".into(),
            },
            AuthError::Server {
                status: 502,
                message: "Bad Gateway".into(),
            },
            AuthError::Client {
                status: 400,
                message: "bad request".into(),
            },
            AuthError::Transport("network down".into()),
        ] {
            let err_template = err_fixture.clone();
            let err = gate
                .run(|_| {
                    let err = err_template.clone();
                    async move { Err::<u32, _>(err) }
                })
                .await
                .unwrap_err();
            assert!(
                !err.is_unauthorized(),
                "fixture must not be a 401: {err_fixture:?}"
            );
            assert!(
                session.read().is_authenticated(),
                "non-401 error must NOT invalidate the session: {err_fixture:?}"
            );
        }
    }

    /// PURA-225 — after a successful refresh, if the replayed data call
    /// still returns 401, the session is dead at the server (the refresh
    /// produced a token the upstream immediately rejects). Invalidate so
    /// AppShell bounces, instead of looping `data 401 → refresh ok →
    /// replay 401 → stuck banner` on every subsequent render.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replay_401_after_successful_refresh_invalidates_session() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|n| {
            assert_eq!(n, 1, "exactly one refresh expected");
            Ok(TokenPairResponse {
                access_token: "fresh-access".into(),
                refresh_token: "fresh-refresh".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let err = gate
            .run(move |snap| {
                let calls = calls_clone.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        assert_eq!(snap.access, "old-access");
                        Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    } else {
                        // Replay with the rotated token also 401s — even
                        // INVALID_TOKEN here is a session-killing signal
                        // because we just rotated; the server rejects the
                        // fresh token, so the family is dead.
                        assert_eq!(snap.access, "fresh-access");
                        Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                    }
                }
            })
            .await
            .unwrap_err();
        assert!(err.is_unauthorized());
        assert_eq!(stub.calls(), 1, "exactly one refresh — never loop");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "initial + one replay, never a second refresh"
        );
        assert_eq!(
            session.read(),
            AuthState::Anonymous,
            "post-refresh replay 401 must invalidate the session"
        );
        assert!(
            storage
                .get(crate::client::store::SESSION_STORAGE_KEY)
                .is_none(),
        );
    }

    // ---------------------------------------------------------------
    // PURA-226 — debug-knob instrumentation regression tests.
    //
    // The default-off contract is the load-bearing invariant: a
    // production build with the operator's knob un-flipped MUST NOT
    // emit any auth-path breadcrumbs (no console noise, no log spam,
    // no test pollution). The "when enabled" tests pin exactly which
    // tags fire on the canonical failure-mode paths so a future
    // refactor that drops a breadcrumb is caught at PR time.
    // ---------------------------------------------------------------

    use crate::client::debug as auth_debug;

    #[tokio::test(flavor = "current_thread")]
    async fn debug_knob_off_emits_no_breadcrumbs_on_happy_path() {
        auth_debug::test_override::disable();
        let _ = auth_debug::test_override::drain();
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session, stub.clone());

        gate.run(|_| async { Ok::<u32, AuthError>(42) })
            .await
            .unwrap();

        let captured = auth_debug::test_override::drain();
        assert!(
            captured.is_empty(),
            "default-off knob leaked breadcrumbs: {captured:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn debug_knob_on_emits_gate_breadcrumbs_for_session_killing_401() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        auth_debug::test_override::enable();
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(authed("ax", "rx"), storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh on USER_DISABLED"));
        let gate = RefreshGate::new(session, stub.clone());

        let _ = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::USER_DISABLED.into())) })
            .await;
        let captured = auth_debug::test_override::drain();
        auth_debug::test_override::disable();

        let joined = captured.join("\n");
        assert!(
            joined.contains(r#""tag":"gate.enter""#),
            "expected gate.enter:\n{joined}"
        );
        assert!(
            joined.contains(r#""tag":"gate.401.session_killing""#),
            "expected gate.401.session_killing:\n{joined}"
        );
        assert!(
            joined.contains(r#""tag":"session.invalidate""#),
            "expected session.invalidate (PURA-225 path):\n{joined}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn debug_knob_on_emits_refresh_breadcrumbs_on_rotation() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        auth_debug::test_override::enable();
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| {
            Ok(TokenPairResponse {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        gate.run(move |snap| {
            let calls = calls_clone.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert_eq!(snap.access, "old-access");
                    Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                } else {
                    Ok::<u32, AuthError>(7)
                }
            }
        })
        .await
        .unwrap();

        let captured = auth_debug::test_override::drain();
        auth_debug::test_override::disable();

        let joined = captured.join("\n");
        // Spec §6.5.3 happy-path: refresh-eligible 401 → refresh.start →
        // refresh.ok → session.update_pair (rotation persisted).
        assert!(
            joined.contains(r#""tag":"gate.401.refresh_eligible""#),
            "missing gate.401.refresh_eligible:\n{joined}"
        );
        assert!(
            joined.contains(r#""tag":"gate.refresh.start""#),
            "missing gate.refresh.start:\n{joined}"
        );
        assert!(
            joined.contains(r#""tag":"gate.refresh.ok""#),
            "missing gate.refresh.ok:\n{joined}"
        );
        assert!(
            joined.contains(r#""tag":"session.update_pair""#),
            "missing session.update_pair (PURA-226 candidate #3 hook):\n{joined}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn debug_knob_on_logs_refresh_failure_with_kept_session_flag() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        auth_debug::test_override::enable();
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("old-access", "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| {
            Err(AuthError::Server {
                status: 502,
                message: "Bad Gateway".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let _ = gate
            .run(|_| async { Err::<u32, _>(AuthError::Unauthorized(msg::INVALID_TOKEN.into())) })
            .await;
        let captured = auth_debug::test_override::drain();
        auth_debug::test_override::disable();

        let joined = captured.join("\n");
        // PURA-214 contract: 5xx refresh failure surfaces with the
        // "kept_session_alive" flag set, so the operator sees the bug
        // class on first read (candidate failure #2 in PURA-225's list).
        assert!(
            joined.contains(r#""tag":"gate.refresh.fail""#),
            "missing gate.refresh.fail:\n{joined}"
        );
        assert!(
            joined.contains(r#""kept_session_alive":true"#),
            "expected kept_session_alive=true on transient refresh fail:\n{joined}"
        );
        // Critical paired invariant — the session is still authed.
        assert!(session.read().is_authenticated());
    }

    /// PURA-233 regression — the v1.0.10 instrumentation truncated
    /// every token to its first 8 bytes for the breadcrumb. Every
    /// JWT minted by [`crate::auth::jwt`] starts with `eyJ0eXAi`
    /// (the base64url encoding of the canonical `{"typ":"JWT",`
    /// header), so the post-refresh `session.update_pair`
    /// breadcrumb collapsed onto the same `access:"eyJ0eXAi"`
    /// string as the pre-refresh `gate.enter` line. An operator
    /// pasting their `?debug=auth` console capture into the issue
    /// thread could not tell whether the rotation had actually
    /// happened — exactly the diagnostic blindspot that left the
    /// board stranded on the `/servers` Session-expired loop with
    /// no way to discriminate between the four candidate failure
    /// modes listed in [PURA-225](/PURA/issues/PURA-225).
    ///
    /// The contract this test pins: after a successful refresh,
    /// the `gate.enter` breadcrumb (logged against the OLD access
    /// token) and the `session.update_pair` breadcrumb (logged
    /// against the NEW access token) MUST emit distinct
    /// `new_access` / `access` prefixes whenever the underlying
    /// tokens differ. Two JWTs that differ only in their signature
    /// segment — the only segment that necessarily varies across a
    /// rotation, since payload-level claims (`sub`, `username`,
    /// `role`) are usually unchanged — must surface as different
    /// breadcrumbs.
    #[tokio::test(flavor = "current_thread")]
    async fn refresh_breadcrumbs_distinguish_jwts_with_shared_header_prefix() {
        use ts6_manager_shared::auth::auth_error_strings as msg;

        // Two JWT-shaped strings that share the standard header +
        // payload prefix but differ in their signature tail. Without
        // the PURA-233 short_token fix both would render as
        // `access:"eyJ0eXAi"` in the breadcrumb and an operator
        // could not tell them apart from a console capture alone.
        const JWT_OLD: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SIG_OLD";
        const JWT_NEW: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SIG_NEW";

        auth_debug::test_override::enable();
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed(JWT_OLD, "old-refresh"),
            storage.clone(),
        ));
        let stub = StubRefresh::new(|_| {
            Ok(TokenPairResponse {
                access_token: JWT_NEW.into(),
                refresh_token: "new-refresh".into(),
            })
        });
        let gate = RefreshGate::new(session.clone(), stub.clone());

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        gate.run(move |snap| {
            let calls = calls_clone.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert_eq!(snap.access, JWT_OLD);
                    Err(AuthError::Unauthorized(msg::INVALID_TOKEN.into()))
                } else {
                    assert_eq!(snap.access, JWT_NEW);
                    Ok::<u32, AuthError>(7)
                }
            }
        })
        .await
        .unwrap();

        let captured = auth_debug::test_override::drain();
        auth_debug::test_override::disable();

        // The two breadcrumbs the operator correlates by eye:
        //   gate.enter        — recorded against JWT_OLD
        //   session.update_pair — recorded against JWT_NEW
        let gate_enter_line = captured
            .iter()
            .find(|line| line.contains(r#""tag":"gate.enter""#))
            .unwrap_or_else(|| panic!("no gate.enter breadcrumb captured:\n{captured:?}"));
        let update_pair_line = captured
            .iter()
            .find(|line| line.contains(r#""tag":"session.update_pair""#))
            .unwrap_or_else(|| panic!("no session.update_pair captured:\n{captured:?}"));

        // The `access` field is wrapped in JSON quotes, so the prefix
        // appears as `"access":"<short_token output>"`. The
        // post-refresh line uses the `new_access` field name on the
        // same value. Extract both and pin them as distinct.
        let gate_prefix = extract_field(gate_enter_line, "access");
        let update_prefix = extract_field(update_pair_line, "new_access");
        assert_ne!(
            gate_prefix, update_prefix,
            "post-rotation breadcrumb MUST distinguish the new access token \
             from the old; both collapsed onto {gate_prefix:?} with the v1.0.10 \
             short_token (PURA-233):\n\
             gate.enter:        {gate_enter_line}\n\
             session.update_pair: {update_pair_line}"
        );
    }

    /// Tiny scraper for `"field":"value"` pairs inside a JSON-encoded
    /// breadcrumb line. The test capture format is a single-line
    /// `serde_json::to_string` of an [`auth_debug::AuthEvent`]; we
    /// avoid pulling in the full parser because the captured lines
    /// can contain ad-hoc fields that vary across breadcrumbs and the
    /// asserts only care about one field at a time.
    fn extract_field(line: &str, key: &str) -> String {
        let needle = format!(r#""{key}":""#);
        let idx = line
            .find(&needle)
            .unwrap_or_else(|| panic!("field `{key}` not found in:\n{line}"));
        let rest = &line[idx + needle.len()..];
        let end = rest
            .find('"')
            .unwrap_or_else(|| panic!("unterminated value for `{key}` in:\n{line}"));
        rest[..end].to_string()
    }
}
