//! Opt-in auth-path breadcrumbs for root-causing repeat logouts.
//!
//! Build note: `web_sys::console` and `web_sys::UrlSearchParams` are
//! reachable through transitive features other workspace deps enable
//! against the shared `web-sys` activation set. If a future trim of the
//! tree drops those, the WASM build will fail; the fix is to add the
//! two feature flags to the `web-sys` entry in the root `Cargo.toml`.
//!
//! The board reported intermittent involuntary sign-outs even after the
//! [PURA-225](/PURA/issues/PURA-225) gate fix. Without logs from a real
//! session we can't tell which of the four candidate failure modes is
//! firing:
//!
//! 1. `/api/*` returns a 401 whose body the gate misclassifies.
//! 2. `POST /api/auth/refresh` fails non-401 and the data 401 strands
//!    the operator on the escape-CTA banner.
//! 3. Refresh succeeds but the rotated-pair `localStorage` write is
//!    racy or dropped.
//! 4. Backend rejects the refresh row because the deployed container
//!    lost the volume on restart.
//!
//! [`AuthDebug::is_enabled`] reads the operator-flippable knob (URL
//! `?debug=auth` OR `localStorage['ts6-manager.debug'] == 'auth'`) once
//! at module init and caches the result. [`log`] is a no-op when the
//! knob is off, so the default production build emits zero console
//! breadcrumbs.
//!
//! Wire shape:
//! ```json
//! {"ts": 1234567.89, "tag": "gate.enter", "path": "/api/servers", "access": "abcdef12"}
//! ```
//! Fields beyond `ts` + `tag` are caller-supplied. The structured JSON
//! is what the operator copies out of DevTools console for triage; the
//! `ts` field is `performance.now()` milliseconds from page load so a
//! reader can correlate gate sub-events without a wall-clock parser.

use serde::Serialize;

/// URL query-parameter knob: `?debug=auth` lights up the breadcrumbs.
pub const URL_PARAM: &str = "debug";
/// Storage knob: `localStorage.setItem('ts6-manager.debug', 'auth')` lights
/// the breadcrumbs even without the URL param. The key is namespaced under
/// the same `ts6-manager.*` prefix the persisted session uses.
pub const STORAGE_KEY: &str = "ts6-manager.debug";
/// Value the knob must hold to enable the auth-path breadcrumbs. A future
/// `widget` / `ws` value can ride the same knob without conflicting.
pub const DEBUG_AUTH: &str = "auth";

/// One emitted event. Serialised to a single console line so it copies
/// cleanly out of DevTools.
#[derive(Debug, Clone, Serialize)]
pub struct AuthEvent<'a> {
    /// Milliseconds since page-load, from `performance.now()`. `None` on
    /// non-WASM builds (the field is omitted from the JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<f64>,
    /// Event tag — e.g. `gate.enter`, `gate.401`, `refresh.post.ok`,
    /// `session.update_pair`, `storage.write`. Stable across versions so
    /// the operator can grep their console capture.
    pub tag: &'a str,
    /// Arbitrary structured payload (path, sub-code, request id, ...).
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// `true` when the operator has explicitly opted in. Cheap to call from
/// any code path — the result is cached after the first hit so we don't
/// re-parse `location.search()` on every gate run.
pub fn is_enabled() -> bool {
    is_enabled_inner()
}

/// Emit a breadcrumb if the debug knob is on. No-op otherwise.
///
/// `data` is any `Serialize`-able payload; pass `serde_json::Value::Null`
/// (or an empty struct) when the tag alone suffices.
pub fn log<T: Serialize>(tag: &str, data: T) {
    if !is_enabled() {
        return;
    }
    let value = serde_json::to_value(&data).unwrap_or(serde_json::Value::Null);
    let event = AuthEvent {
        ts: now_ms(),
        tag,
        data: value,
    };
    emit(&event);
}

/// Build a `serde_json::Value::Object` from a slice of `(name, jsonable)`
/// pairs. Tiny convenience so call sites don't have to inline the json
/// macro for one-off two-or-three-field payloads.
pub fn fields(entries: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    serde_json::Value::Object(map)
}

/// Short, distinguishable form of a bearer / refresh token. Emitted
/// instead of the raw value so a console capture pasted into the issue
/// thread does not leak credentials.
///
/// **PURA-233** — the v1.0.10 instrumentation emitted the first 8
/// characters only. Every HS256 JWT minted by [`crate::auth::jwt`]
/// begins with the same `eyJ0eXAi` header prefix (the base64 encoding
/// of `{"typ":"JWT",`), so every breadcrumb showed an identical access
/// prefix regardless of whether a refresh had actually rotated the
/// token. That made the four candidate failure modes
/// (`[PURA-225](/PURA/issues/PURA-225)` list) indistinguishable from a
/// pasted console capture — you could not tell #1 (rotation race) from
/// #3 (rotation dropped) from #4 (rotation succeeded but the upstream
/// rejects the new access).
///
/// The new format keeps the same 8-character budget but takes 4 from
/// the head and 4 from the tail, separated by `..`, so two different
/// JWTs that share the standard header prefix still surface as
/// different breadcrumbs (`eyJ0..A1b2` vs. `eyJ0..C3d4`). Tokens
/// shorter than 8 characters fall through to the original "return
/// verbatim" branch — the contract for non-JWT-shaped values stays
/// unchanged.
pub fn short_token(s: &str) -> String {
    if s.len() >= 8 {
        let head = &s[..4];
        let tail = &s[s.len() - 4..];
        format!("{head}..{tail}")
    } else {
        s.to_string()
    }
}

/// Snapshot a monotonic millisecond timestamp the caller can pass back
/// to [`elapsed_ms`] later to compute a duration. Returns `None` when
/// the debug knob is off OR when `performance.now()` is unavailable so
/// callers can avoid any work in that branch.
pub fn now_ms_for_duration() -> Option<f64> {
    if !is_enabled() {
        return None;
    }
    now_ms()
}

/// Elapsed milliseconds since `start`. Returns `0.0` when either end
/// of the timestamp pair is missing — the caller is free to serialise
/// the result regardless; a missing-start is the breadcrumb for "we
/// didn't capture a start because debug was off when the start arm
/// ran".
pub fn elapsed_ms(start: Option<f64>) -> f64 {
    match (start, now_ms()) {
        (Some(a), Some(b)) => (b - a).max(0.0),
        _ => 0.0,
    }
}

#[cfg(target_arch = "wasm32")]
fn is_enabled_inner() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(detect_enabled)
}

#[cfg(target_arch = "wasm32")]
fn detect_enabled() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    if let Ok(search) = window.location().search()
        && let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search)
        && params.get(URL_PARAM).as_deref() == Some(DEBUG_AUTH)
    {
        return true;
    }
    if let Ok(Some(storage)) = window.local_storage()
        && let Ok(Some(value)) = storage.get_item(STORAGE_KEY)
        && value == DEBUG_AUTH
    {
        return true;
    }
    false
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> Option<f64> {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
}

#[cfg(target_arch = "wasm32")]
fn emit(event: &AuthEvent<'_>) {
    let payload = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    web_sys::console::log_2(
        &wasm_bindgen::JsValue::from_str("[ts6-manager:auth]"),
        &wasm_bindgen::JsValue::from_str(&payload),
    );
}

// ---------------------------------------------------------------------------
// Non-WASM (SSR + native tests): the debug module type-checks and the
// `log` function is callable, but events are silently dropped. The
// instrumentation is browser-side by design — server-side observability
// uses `tracing` (see backend instrumentation in `auth/extractors.rs`
// and `auth/routes.rs`).

#[cfg(not(target_arch = "wasm32"))]
fn is_enabled_inner() -> bool {
    test_override::is_enabled()
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> Option<f64> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn emit(event: &AuthEvent<'_>) {
    test_override::push(event);
}

// ---------------------------------------------------------------------------
// Test seam: native unit tests for the gate/session instrumentation
// (the bulk of the regression coverage for this knob) need a way to
// flip the knob on without a browser. `test_override` exposes a
// thread-local override + capture buffer that the cfg(test) builds opt
// into; in production native builds it is a no-op.

#[cfg(not(target_arch = "wasm32"))]
pub mod test_override {
    use super::AuthEvent;
    use std::cell::RefCell;

    thread_local! {
        static ENABLED: RefCell<bool> = const { RefCell::new(false) };
        static CAPTURE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn is_enabled() -> bool {
        ENABLED.with(|e| *e.borrow())
    }

    pub(super) fn push(event: &AuthEvent<'_>) {
        let line = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
        CAPTURE.with(|c| c.borrow_mut().push(line));
    }

    /// Turn the knob on for the current thread. Drops the captured
    /// buffer first so a test that re-enables sees only its own
    /// emissions.
    pub fn enable() {
        ENABLED.with(|e| *e.borrow_mut() = true);
        CAPTURE.with(|c| c.borrow_mut().clear());
    }

    pub fn disable() {
        ENABLED.with(|e| *e.borrow_mut() = false);
    }

    /// Drain every captured breadcrumb emitted on the current thread
    /// since `enable()` (or the last `drain()`).
    pub fn drain() -> Vec<String> {
        CAPTURE.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_is_silent_when_disabled() {
        // Knob default is OFF — no captures.
        test_override::disable();
        let _ = test_override::drain();
        log("gate.enter", fields(&[("path", "/api/x".into())]));
        log("gate.401", serde_json::Value::Null);
        let captured = test_override::drain();
        assert!(captured.is_empty(), "default-off knob leaked: {captured:?}");
    }

    #[test]
    fn log_emits_structured_breadcrumb_when_enabled() {
        test_override::enable();
        log(
            "refresh.post.ok",
            fields(&[
                ("request_id", "req-1".into()),
                ("status", 200.into()),
                ("duration_ms", 42.into()),
            ]),
        );
        let captured = test_override::drain();
        test_override::disable();
        assert_eq!(captured.len(), 1, "expected one line, got: {captured:?}");
        let line = &captured[0];
        assert!(line.contains(r#""tag":"refresh.post.ok""#), "{line}");
        assert!(line.contains(r#""request_id":"req-1""#), "{line}");
        assert!(line.contains(r#""status":200"#), "{line}");
        assert!(line.contains(r#""duration_ms":42"#), "{line}");
    }

    #[test]
    fn short_token_truncates_but_preserves_shape() {
        // PURA-233 — head 4 + tail 4 with a `..` separator so two
        // different JWTs that share the standard `eyJ0eXAi` header
        // prefix surface as different breadcrumbs.
        assert_eq!(short_token("0123456789abcdef"), "0123..cdef");
        assert_eq!(short_token("abc"), "abc"); // shorter than 8 — keep as-is
        assert_eq!(short_token(""), "");
    }

    /// PURA-233 — pin the diagnostic contract that the
    /// [`short_token`] output for two distinct JWTs which share the
    /// canonical `eyJ0eXAi` header prefix MUST surface as distinct
    /// breadcrumbs. The v1.0.10 implementation truncated to the first
    /// 8 bytes only, which made every JWT's breadcrumb collapse to
    /// the same string and stranded the board on the `/servers`
    /// Session-expired loop with no way to tell rotation candidates
    /// apart from a pasted console capture.
    #[test]
    fn short_token_distinguishes_jwts_with_shared_header_prefix() {
        // Canonical HS256 JWT header (`{"typ":"JWT","alg":"HS256"}`)
        // base64url-encodes to `eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9`,
        // so the first ~36 bytes of any JWT minted by `jwt::mint_*`
        // are byte-identical. The diagnostic contract is "different
        // tokens get different breadcrumbs"; two JWTs that differ
        // only in their signature segment must NOT collapse onto the
        // same `short_token` output.
        let jwt_old = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.AAAAOLD";
        let jwt_new = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.AAAANEW";
        assert_ne!(
            short_token(jwt_old),
            short_token(jwt_new),
            "two JWTs differing only in signature MUST yield distinct breadcrumbs"
        );
        // Belt-and-braces: the format must include the tail so an
        // operator pasting a capture can correlate `gate.enter` and
        // `session.update_pair` lines without seeing the raw token.
        assert!(short_token(jwt_old).ends_with("AOLD"));
        assert!(short_token(jwt_new).ends_with("ANEW"));
    }

    #[test]
    fn fields_builds_a_json_object_for_caller() {
        let v = fields(&[("a", 1.into()), ("b", "two".into())]);
        let obj = v.as_object().expect("object");
        assert_eq!(obj.get("a"), Some(&serde_json::Value::from(1)));
        assert_eq!(obj.get("b"), Some(&serde_json::Value::from("two")));
    }

    #[test]
    fn enable_then_disable_stops_capture() {
        test_override::enable();
        log("first", serde_json::Value::Null);
        test_override::disable();
        log("second-must-be-dropped", serde_json::Value::Null);
        let captured = test_override::drain();
        assert_eq!(captured.len(), 1, "expected only the pre-disable line");
        assert!(captured[0].contains(r#""tag":"first""#));
    }
}
