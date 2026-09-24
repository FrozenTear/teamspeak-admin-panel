//! Cross-tab exclusive lock around the refresh critical section.
//!
//! Tokens live in `localStorage`, which every tab of this origin shares.
//! The in-process mutex in [`super::RefreshGate`] only coalesces callers
//! inside one tab. A second tab that still holds the pre-rotation refresh
//! token will `POST /api/auth/refresh` with it; the server treats that as
//! replay and revokes every session for the user.
//!
//! On wasm32 the lock is `navigator.locks.request('ts6-auth-refresh')`
//! (exclusive, the default mode). Native builds and browsers without
//! `navigator.locks` (old engines, insecure contexts) take
//! [`NoopCrossTabLock`]: the critical section still runs, and the gate
//! re-reads storage before refreshing or invalidating.
//!
//! The Web Locks binding is unstable (`web_sys_unstable_apis`). Wasm builds
//! in this workspace already pass that cfg (see `.cargo/config.toml`), the
//! same way the video player reaches `WebTransport`.

use std::sync::Arc;

/// Future returned by [`CrossTabLock::acquire`]. Separate from the gate's
/// `Result`-bearing refresh future: a missing Web Locks implementation
/// yields a no-op guard instead of an error.
#[cfg(target_arch = "wasm32")]
pub(crate) type LockFuture<T> = futures::future::LocalBoxFuture<'static, T>;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type LockFuture<T> = futures::future::BoxFuture<'static, T>;

/// Name passed to `navigator.locks.request`. One exclusive lock covers every
/// tab of this origin, which is the same scope as the shared auth blob.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const REFRESH_LOCK_NAME: &str = "ts6-auth-refresh";

/// Held for the refresh critical section. Dropping the guard releases the
/// cross-tab lock. The no-op guard releases nothing.
#[must_use = "dropping the guard releases the cross-tab refresh lock"]
pub(crate) struct CrossTabGuard {
    #[cfg(target_arch = "wasm32")]
    release: Option<js_sys::Function>,
    /// Keeps the `locks.request` promise reachable until the section ends.
    #[cfg(target_arch = "wasm32")]
    _request: Option<js_sys::Promise>,
}

impl CrossTabGuard {
    pub(crate) fn noop() -> Self {
        Self {
            #[cfg(target_arch = "wasm32")]
            release: None,
            #[cfg(target_arch = "wasm32")]
            _request: None,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn web(release: js_sys::Function, request: js_sys::Promise) -> Self {
        Self {
            release: Some(release),
            _request: Some(request),
        }
    }
}

impl Drop for CrossTabGuard {
    fn drop(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(resolve) = self.release.take() {
            let _ = resolve.call0(&wasm_bindgen::JsValue::UNDEFINED);
        }
    }
}

/// Exclusive lock around the refresh critical section.
///
/// Tests and native builds use [`NoopCrossTabLock`]. The wasm runtime uses
/// the Web Locks API when `navigator.locks` is present.
pub(crate) trait CrossTabLock: Send + Sync {
    fn acquire(&self) -> LockFuture<CrossTabGuard>;
}

/// Lock that runs the critical section immediately.
///
/// Used on native (including unit tests) and as the wasm fallback when
/// `navigator.locks` is missing. The gate still re-reads storage; only the
/// mutual exclusion across tabs is skipped.
pub(crate) struct NoopCrossTabLock;

impl CrossTabLock for NoopCrossTabLock {
    fn acquire(&self) -> LockFuture<CrossTabGuard> {
        Box::pin(async { CrossTabGuard::noop() })
    }
}

pub(crate) fn platform_default() -> Arc<dyn CrossTabLock> {
    #[cfg(target_arch = "wasm32")]
    {
        Arc::new(WebLocks::new(REFRESH_LOCK_NAME))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Arc::new(NoopCrossTabLock)
    }
}

#[cfg(target_arch = "wasm32")]
struct WebLocks {
    name: &'static str,
}

#[cfg(target_arch = "wasm32")]
impl WebLocks {
    fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[cfg(target_arch = "wasm32")]
impl CrossTabLock for WebLocks {
    fn acquire(&self) -> LockFuture<CrossTabGuard> {
        let name = self.name;
        Box::pin(async move { acquire_web_lock(name).await })
    }
}

#[cfg(target_arch = "wasm32")]
async fn acquire_web_lock(name: &str) -> CrossTabGuard {
    use std::sync::atomic::{AtomicBool, Ordering};
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;

    let Some(locks) = lock_manager() else {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            crate::client::debug::log("gate.cross_tab_lock.unavailable", serde_json::Value::Null);
        }
        return CrossTabGuard::noop();
    };

    let (acquired, acquired_resolve) = deferred();
    let (release, release_resolve) = deferred();
    // If this future is cancelled before the guard is returned, resolving
    // the release promise drops the lock as soon as the browser grants it.
    let mut release_on_cancel = ReleaseOnDrop::new(release_resolve);

    let callback = Closure::once(move |_lock: JsValue| {
        let _ = acquired_resolve.call0(&JsValue::UNDEFINED);
        release
    });
    // `request` is not marked `[Throws]` in the binding. The lock name is a
    // non-empty constant, which is the only input the spec rejects.
    // The callback parameter is a typed `Function<fn(JsOption<Lock>) -> Promise>`.
    let callback_fn: &js_sys::Function<fn(js_sys::JsOption<web_sys::Lock>) -> js_sys::Promise> =
        callback.as_ref().unchecked_ref();
    let request = locks.request_with_callback(name, callback_fn);
    // `forget` hands the callback to JS. `Closure::once` drops its state
    // when the browser invokes it. The leak only survives if the document
    // dies before the grant, which is process teardown.
    callback.forget();

    if JsFuture::from(acquired).await.is_err() {
        return CrossTabGuard::noop();
    }
    CrossTabGuard::web(release_on_cancel.disarm(), request)
}

#[cfg(target_arch = "wasm32")]
fn lock_manager() -> Option<web_sys::LockManager> {
    use wasm_bindgen::{JsCast, JsValue};

    let window = web_sys::window()?;
    let navigator = window.navigator();
    let locks = js_sys::Reflect::get(navigator.as_ref(), &JsValue::from_str("locks")).ok()?;
    if locks.is_null() || locks.is_undefined() {
        return None;
    }
    locks.dyn_into::<web_sys::LockManager>().ok()
}

/// `Promise::new` runs the executor synchronously, so the resolve function
/// is available before this returns.
#[cfg(target_arch = "wasm32")]
fn deferred() -> (js_sys::Promise, js_sys::Function) {
    let mut resolve_slot = None;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        resolve_slot = Some(resolve);
    });
    let resolve = resolve_slot.expect("Promise::new invokes the executor immediately");
    (promise, resolve)
}

#[cfg(target_arch = "wasm32")]
struct ReleaseOnDrop {
    resolve: Option<js_sys::Function>,
}

#[cfg(target_arch = "wasm32")]
impl ReleaseOnDrop {
    fn new(resolve: js_sys::Function) -> Self {
        Self {
            resolve: Some(resolve),
        }
    }

    fn disarm(&mut self) -> js_sys::Function {
        self.resolve.take().expect("release resolver armed once")
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(resolve) = self.resolve.take() {
            let _ = resolve.call0(&wasm_bindgen::JsValue::UNDEFINED);
        }
    }
}
