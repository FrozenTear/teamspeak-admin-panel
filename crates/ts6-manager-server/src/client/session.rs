//! Single-flight refresh-on-401 interceptor.
//!
//! Wraps any `(access_token) -> Future<Result<T, AuthError>>` call. If the
//! call returns 401 with the spec body [`auth_error_strings::INVALID_TOKEN`],
//! the interceptor:
//!
//! 1. Acquires a process-wide [`futures::lock::Mutex`] so only one refresh
//!    fires regardless of how many callers raced into the gate.
//! 2. Re-checks the access token — another caller may have rotated it
//!    while we were waiting. If so, replays the original call with the
//!    fresh token and returns.
//! 3. Otherwise calls `POST /api/auth/refresh` once. On success, updates the
//!    [`AuthState`] (in-memory + storage) and replays the original call.
//! 4. **On any refresh failure invalidates the session** — no silent
//!    looping.
//!
//! The refresh transport is injected via [`RefreshFn`] so unit tests can
//! exercise the locking + replay logic without touching the network.
//!
//! `futures::lock::Mutex` is runtime-agnostic: the same lock works under
//! tokio (server-side / native tests) and under wasm-bindgen-futures
//! (browser). We avoid `tokio::sync::Mutex` so this module compiles on the
//! `wasm32-unknown-unknown` target the dx-CLI builds for the SPA bundle.

use std::future::Future;
use std::sync::Arc;
use futures::lock::Mutex;

use ts6_manager_shared::auth::{RefreshRequest, TokenPairResponse, UserInfo};

use crate::client::auth::AuthError;
use crate::client::store::AuthState;

/// `BoxFuture` that the gate's trait objects return. On the browser
/// `JsFuture` is `!Send`, so the wasm build uses the non-Send variant; on
/// native, the `Send`-bearing variant flows through `tokio::spawn` correctly.
#[cfg(target_arch = "wasm32")]
type GateFuture<T> = futures::future::LocalBoxFuture<'static, Result<T, AuthError>>;
#[cfg(not(target_arch = "wasm32"))]
type GateFuture<T> = futures::future::BoxFuture<'static, Result<T, AuthError>>;

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
                    refreshToken: token,
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
/// The contract: `read()` returns the live tokens, `update_pair()` swaps
/// the access/refresh in place (keeping the user), and `invalidate()` sets
/// the session to anonymous + clears storage. All three are called only by
/// the gate's critical section, so a non-locking handle is fine — Signal
/// updates are synchronous on the same task.
pub trait SessionHandle: Send + Sync {
    fn read(&self) -> AuthState;
    fn update_pair(&self, access: String, refresh: String);
    fn invalidate(&self);
}

/// Single-flight refresh gate.
///
/// Holds the in-flight refresh mutex so only one rotation runs at a time.
/// All other concerns (transport, persistence) are injected so this struct
/// is pure logic and tests can exercise every branch without `web-sys`.
pub struct RefreshGate {
    session: Arc<dyn SessionHandle>,
    refresh_fn: Arc<dyn RefreshFn>,
    /// In-flight refresh lock. `tokio::sync::Mutex` works on WASM via
    /// `wasm-bindgen-futures`; per spec §6.5.3 we never want two refreshes
    /// running concurrently for the same browser tab.
    lock: Arc<Mutex<()>>,
}

impl RefreshGate {
    pub fn new(session: Arc<dyn SessionHandle>, refresh_fn: Arc<dyn RefreshFn>) -> Self {
        Self {
            session,
            refresh_fn,
            lock: Arc::new(Mutex::new(())),
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
    ///   `AuthError::Unauthorized(INVALID_TOKEN)` without calling `f`. The
    ///   route layer is expected to redirect to `/login` rather than have
    ///   us forge a bearer.
    /// - On `Unauthorized(INVALID_TOKEN)` from `f`, take the lock, possibly
    ///   refresh, replay `f` once. Replay failure terminates the session.
    /// - On any other error, propagate without touching the session.
    pub async fn run<F, Fut, T>(&self, mut f: F) -> Result<T, AuthError>
    where
        F: FnMut(SessionSnapshot) -> Fut,
        Fut: Future<Output = Result<T, AuthError>>,
    {
        let first_snapshot = match self.snapshot() {
            Some(s) => s,
            None => {
                return Err(AuthError::Unauthorized(
                    ts6_manager_shared::auth::auth_error_strings::INVALID_TOKEN.into(),
                ));
            }
        };
        let first_access = first_snapshot.access.clone();
        let first_err = match f(first_snapshot).await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        if !first_err.is_invalid_or_expired_token() {
            return Err(first_err);
        }

        // 401-with-INVALID_TOKEN path: take the gate.
        let _guard = self.lock.lock().await;

        // Re-check: another task may have rotated while we waited.
        let after_lock = match self.snapshot() {
            Some(s) => s,
            None => {
                return Err(AuthError::Unauthorized(
                    ts6_manager_shared::auth::auth_error_strings::INVALID_TOKEN.into(),
                ));
            }
        };
        if after_lock.access != first_access {
            // Another caller already rotated — skip refresh, replay with
            // the fresh access token.
            return f(after_lock).await;
        }

        // We are the rotator. Issue the refresh once.
        match self.refresh_fn.refresh(after_lock.refresh.clone()).await {
            Ok(pair) => {
                self.session
                    .update_pair(pair.accessToken.clone(), pair.refreshToken.clone());
                let replay = SessionSnapshot {
                    access: pair.accessToken,
                    refresh: pair.refreshToken,
                    user: after_lock.user,
                };
                f(replay).await
            }
            Err(e) => {
                // Refresh failure: invalidate the session immediately.
                // Spec §6.5.3 reuse-detection means a 401 here may indicate
                // the family was revoked; either way the user must re-auth.
                self.session.invalidate();
                Err(translate_refresh_error(e))
            }
        }
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
}

/// Surface a refresh-call failure to the original caller.
///
/// We always return some flavour of [`AuthError::Unauthorized`] so the route
/// layer's "logged out" handler kicks in. The original error's message is
/// preserved for debug logs even though most callers will only branch on the
/// 401-ness.
fn translate_refresh_error(e: AuthError) -> AuthError {
    use ts6_manager_shared::auth::auth_error_strings as msg;
    match e {
        AuthError::Unauthorized(m) => AuthError::Unauthorized(m),
        AuthError::Transport(m) => AuthError::Transport(m),
        AuthError::Deserialise(m) => AuthError::Deserialise(m),
        AuthError::Client { status, message }
        | AuthError::Server { status, message } if status >= 500 => AuthError::Server { status, message },
        AuthError::Client { .. } | AuthError::Server { .. } => {
            AuthError::Unauthorized(msg::INVALID_TOKEN.into())
        }
        AuthError::UnsupportedTarget => AuthError::UnsupportedTarget,
    }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::{MemoryStore, Storage};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::testing::InMemorySession;

    fn user() -> UserInfo {
        UserInfo {
            id: 1,
            username: "alice".into(),
            displayName: "Alice".into(),
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
        responder: Box<
            dyn Fn(u32) -> Result<TokenPairResponse, AuthError> + Send + Sync,
        >,
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
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("ax", "rx"),
            storage.clone(),
        ));
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
    async fn anonymous_session_short_circuits_to_unauthorized() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> =
            Arc::new(InMemorySession::new(AuthState::Anonymous, storage.clone()));
        let stub = StubRefresh::new(|_| panic!("must not refresh"));
        let gate = RefreshGate::new(session, stub.clone());

        let err = gate
            .run(|_| async { Ok::<u32, AuthError>(0) })
            .await
            .unwrap_err();
        assert!(err.is_invalid_or_expired_token(), "got: {err}");
        assert_eq!(stub.calls(), 0);
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
                accessToken: "new-access".into(),
                refreshToken: "new-refresh".into(),
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
        assert!(storage.get(crate::client::store::SESSION_STORAGE_KEY).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn other_errors_do_not_trigger_refresh_or_invalidate() {
        let storage = arc_storage(MemoryStore::new());
        let session: Arc<dyn SessionHandle> = Arc::new(InMemorySession::new(
            authed("ax", "rx"),
            storage.clone(),
        ));
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
                accessToken: "fresh-access".into(),
                refreshToken: "fresh-refresh".into(),
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
}
