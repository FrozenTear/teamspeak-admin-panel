//! In-memory diagnostic ring buffers for the Report bug payload.
//!
//! Pure Rust — no Sentry / browser SDK. Toasts and recent WS / client
//! errors are recorded here so the operator can see (and the API can
//! attach) the last few events when they file a report.
//!
//! The rings are process-global `Mutex` slots so the WS reconnect loop
//! (a `spawn_local` task) and the Dioxus toaster can both append without
//! a context provider. WASM is single-threaded; native tests share the
//! same API and call [`reset_for_tests`] between cases.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Last N toasts kept for a bug report. Matches "last few" without
/// shipping a full session transcript.
const MAX_TOASTS: usize = 8;
/// Last N WS / authorized-client errors kept for a bug report.
const MAX_CLIENT_ERRORS: usize = 8;

/// One toast as attached to `POST /api/bug-reports`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastSnapshot {
    /// Wire variant. Danger toasts are `"error"` to match the provisional
    /// API shape; other variants keep their UI names (`info` / `success` /
    /// `warning`).
    pub variant: String,
    pub message: String,
    /// ISO-8601 timestamp (UTC, millisecond precision).
    pub at: String,
}

/// One WS or authorized-client error as attached to `POST /api/bug-reports`.
///
/// The provisional payload field is `wsErrors`; API-client failures are
/// recorded here too so a 502 / transport blip shows up even when it
/// never became a toast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientErrorSnapshot {
    pub message: String,
    /// ISO-8601 timestamp (UTC, millisecond precision).
    pub at: String,
}

static TOASTS: Mutex<VecDeque<ToastSnapshot>> = Mutex::new(VecDeque::new());
static CLIENT_ERRORS: Mutex<VecDeque<ClientErrorSnapshot>> = Mutex::new(VecDeque::new());

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn lock_ring<T>(slot: &Mutex<VecDeque<T>>) -> std::sync::MutexGuard<'_, VecDeque<T>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push_ring<T>(slot: &Mutex<VecDeque<T>>, item: T, cap: usize) {
    let mut g = lock_ring(slot);
    g.push_back(item);
    while g.len() > cap {
        g.pop_front();
    }
}

/// Record a toast that was just shown to the operator.
pub fn record_toast(variant: impl Into<String>, message: impl Into<String>) {
    push_ring(
        &TOASTS,
        ToastSnapshot {
            variant: variant.into(),
            message: message.into(),
            at: now_iso(),
        },
        MAX_TOASTS,
    );
}

/// Record a WS disconnect / parse failure or a failed authorized fetch.
pub fn record_client_error(message: impl Into<String>) {
    push_ring(
        &CLIENT_ERRORS,
        ClientErrorSnapshot {
            message: message.into(),
            at: now_iso(),
        },
        MAX_CLIENT_ERRORS,
    );
}

/// Oldest-first copy of the toast ring.
pub fn snapshot_toasts() -> Vec<ToastSnapshot> {
    lock_ring(&TOASTS).iter().cloned().collect()
}

/// Oldest-first copy of the WS / client-error ring.
pub fn snapshot_client_errors() -> Vec<ClientErrorSnapshot> {
    lock_ring(&CLIENT_ERRORS).iter().cloned().collect()
}

/// Wipe both rings. Test-only — production never resets mid-session.
#[cfg(test)]
pub fn reset_for_tests() {
    lock_ring(&TOASTS).clear();
    lock_ring(&CLIENT_ERRORS).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_ring_evicts_oldest_past_cap() {
        reset_for_tests();
        for i in 0..(MAX_TOASTS + 3) {
            record_toast("error", format!("toast-{i}"));
        }
        let snap = snapshot_toasts();
        assert_eq!(snap.len(), MAX_TOASTS);
        assert_eq!(snap[0].message, "toast-3");
        assert_eq!(
            snap[MAX_TOASTS - 1].message,
            format!("toast-{}", MAX_TOASTS + 2)
        );
        assert!(snap.iter().all(|t| t.variant == "error"));
        assert!(snap.iter().all(|t| t.at.contains('T')));
    }

    #[test]
    fn client_error_ring_evicts_oldest_past_cap() {
        reset_for_tests();
        for i in 0..(MAX_CLIENT_ERRORS + 2) {
            record_client_error(format!("ws: drop {i}"));
        }
        let snap = snapshot_client_errors();
        assert_eq!(snap.len(), MAX_CLIENT_ERRORS);
        assert_eq!(snap[0].message, "ws: drop 2");
        assert!(snap.iter().all(|e| e.at.contains('T')));
    }

    #[test]
    fn snapshots_are_empty_after_reset() {
        reset_for_tests();
        record_toast("info", "hello");
        record_client_error("boom");
        reset_for_tests();
        assert!(snapshot_toasts().is_empty());
        assert!(snapshot_client_errors().is_empty());
    }
}
