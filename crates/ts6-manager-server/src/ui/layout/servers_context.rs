//! Shared `GET /api/servers` state for the AppShell chrome.
//!
//! Two surfaces under the chrome want the same server list — the desktop
//! header pill and the mobile bar both render a `ServerSelector` — and any
//! page that wants to know "which configured servers does this operator
//! have access to?" can read from the same place. Hoisting one fetch into
//! [`AppShell`] context means both variants share a single in-flight
//! request and a single cache, avoiding the desktop/mobile selector pair
//! firing two `/api/servers` calls on every authed route mount.
//!
//! The operator's current pick lives on the same context as a
//! [`Signal<Option<i64>>`] hydrated from [`crate::client::ui_prefs`]. The
//! header / mobile [`crate::ui::components::ServerSelector`] pair write
//! that signal (and persist it); the dashboard reads it so a header pick
//! refetches KPIs for that `configId` without a second `GET /api/servers`.
//!
//! Internally we hold a [`Signal<ServersData>`] state machine rather than a
//! raw `Resource<…>`. That makes the selector logic identical between
//! production (where a reactive `use_resource` updates the signal) and
//! tests (where the harness sets the signal to a canned value with no fetch).

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::ui_prefs::load_selected_server_id;

/// Three-state load model for the `/api/servers` list.
#[derive(Clone, Debug, PartialEq)]
pub enum ServersData {
    Loading,
    Loaded(Vec<ServerSummary>),
    Error(ApiError),
}

impl ServersData {
    /// Convenience for the selector — `&[ServerSummary]` for any state that
    /// has a list to render, empty slice otherwise.
    pub fn rows(&self) -> &[ServerSummary] {
        match self {
            ServersData::Loaded(v) => v.as_slice(),
            ServersData::Loading | ServersData::Error(_) => &[],
        }
    }
}

/// Shape stashed in Dioxus context. Cloning shares the same underlying
/// Signals, so two consumers see the same updates.
#[derive(Clone, Copy)]
pub struct ServersContext {
    pub data: Signal<ServersData>,
    /// Operator's current pick — the same id the header selector persists
    /// through [`crate::client::ui_prefs`]. `None` means no selection
    /// (never picked, or a stale id was cleared). Sharing the signal lets
    /// the dashboard refetch when the pick changes without polling
    /// `localStorage` or issuing a second `/api/servers` call.
    pub selected: Signal<Option<i64>>,
}

/// Compound hook: build the [`ServersContext`], spawn the background fetch,
/// and provide the context for descendants. Designed to be called **directly**
/// from a component body (not inside another hook's closure) so the inner
/// `use_signal` / `use_resource` / `use_context_provider` calls all run as
/// top-level hooks in the parent's hook list.
///
/// The fetch is gated on `session.ready` **and** `is_authenticated()` so
/// it self-heals across the Anonymous → Authenticated transition. The
/// first poll on a hard-refresh of `/servers` can land before
/// `App`'s post-mount `rehydrate_from_storage` `use_effect` upgrades the
/// session signal; without the gate, the gate's anonymous short-circuit
/// would cache a synthetic `Unauthorized` and the page would render
/// "Session expired" the moment the chrome appeared
/// ([PURA-232](/PURA/issues/PURA-232)).
///
/// Dioxus 0.7 `use_future` is **not** reactive — it spawns once on first
/// render. Reading a memo inside that one-shot closure does not restart
/// the task after rehydrate, so ServerSelector stayed on "Loading
/// servers…" forever while pages saw an empty list ("No server selected").
/// `use_resource` is the same hook the dashboard already uses for a
/// signal-driven refetch. A refresh button + interval refresh land in
/// Phase 2 with the rest of the live-telemetry story.
pub fn mount_servers_context() -> ServersContext {
    let gate = use_auth_gate();
    let session = use_session();
    let mut data: Signal<ServersData> = use_signal(|| ServersData::Loading);
    let selected: Signal<Option<i64>> = use_signal({
        let storage = session.storage.clone();
        move || load_selected_server_id(&*storage)
    });

    // PURA-232 — memoise the authed bit so token rotations
    // (`session.update_pair` from the refresh gate) don't cancel and
    // restart the in-flight fetch. The memo's value only flips on
    // Anonymous ↔ Authenticated transitions, which is exactly the
    // signal we want the resource to react to.
    let is_authed = use_memo(move || session.state.read().is_authenticated());

    // Read `ready` + the authed memo *inside* the resource so Dioxus 0.7
    // cancels and re-spawns after rehydrate (same class as #19's AppShell
    // gate). `use_future` cannot do this — it is fire-and-forget.
    let _ = use_resource(move || {
        let gate = gate.clone();
        let ready = *session.ready.read();
        let authed = is_authed();
        async move {
            if !should_fetch_servers(ready, authed) {
                // Stay in Loading. The route guard (`AppShell`) bounces
                // to /login if the session never materialises; if it
                // does (rehydrate / post-login), this resource re-runs
                // with both bits set and fires the real fetch.
                data.set(ServersData::Loading);
                return;
            }
            let next = match fetch_servers(gate).await {
                Ok(rows) => ServersData::Loaded(rows),
                // PURA-232 — extra belt-and-braces. The gate now emits
                // `SessionAnonymous` instead of a server-401 envelope on
                // its own short-circuit; even if a future refactor
                // tweaks the upstream surface, the page must not render
                // this as a fatal error. The resource re-runs once
                // `ready`/`is_authed` flip, so this is no longer a
                // terminal state after hard-refresh.
                Err(ApiError::SessionAnonymous) => ServersData::Loading,
                Err(e) => ServersData::Error(e),
            };
            data.set(next);
        }
    });
    let ctx = ServersContext { data, selected };
    use_context_provider(|| ctx);
    ctx
}

/// Chrome `/api/servers` fetch predicate.
///
/// First paint is Anonymous and not-ready (PURA-129). Fetching then
/// short-circuits as `SessionAnonymous`. Combined with a non-reactive
/// `use_future`, that left ServerSelector on "Loading servers…" after a
/// hard refresh even though the operator still had a valid session blob.
pub fn should_fetch_servers(ready: bool, authenticated: bool) -> bool {
    ready && authenticated
}

/// Pull the shared [`ServersContext`] from context. Panics if no provider
/// is mounted upstream — the AppShell always provides one before any
/// authenticated child renders.
pub fn use_servers_context() -> ServersContext {
    use_context::<ServersContext>()
}

async fn fetch_servers(gate: Arc<RefreshGate>) -> Result<Vec<ServerSummary>, ApiError> {
    let base = api::api_base();
    api::authorized_get_json(&gate, &base, "/api/servers").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fixture(id: i64, name: &str) -> ServerSummary {
        let now = Utc::now();
        ServerSummary {
            id,
            name: name.into(),
            host: "ts.example.com".into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: None,
            has_ssh_credentials: false,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
            last_seen_at: None,
        }
    }

    #[test]
    fn rows_returns_loaded_payload() {
        let d = ServersData::Loaded(vec![fixture(1, "Primary"), fixture(2, "Backup")]);
        assert_eq!(d.rows().len(), 2);
        assert_eq!(d.rows()[0].name, "Primary");
    }

    #[test]
    fn rows_returns_empty_during_loading_or_error() {
        assert!(ServersData::Loading.rows().is_empty());
        let err = ServersData::Error(ApiError::Transport("boom".into()));
        assert!(err.rows().is_empty());
    }

    #[test]
    fn fetch_waits_for_rehydrate_and_an_authenticated_session() {
        assert!(
            !should_fetch_servers(false, false),
            "first paint is Anonymous and not-ready — do not hit /api/servers"
        );
        assert!(
            !should_fetch_servers(false, true),
            "ready-false authed is the SSR harness path, not a live fetch"
        );
        assert!(
            !should_fetch_servers(true, false),
            "rehydrate finished with no blob — stay Loading; AppShell bounces"
        );
        assert!(
            should_fetch_servers(true, true),
            "rehydrate finished with a blob — fire GET /api/servers"
        );
    }
}
