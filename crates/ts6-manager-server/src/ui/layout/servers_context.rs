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
//! The dashboard still does its own per-mount fetch today (see
//! `ui::pages::dashboard_placeholder`); PURA-31 can collapse that onto this
//! context in a follow-up — the contract here is "shared list, refetched
//! on demand", which is the only thing the selector needs.
//!
//! Internally we hold a [`Signal<ServersData>`] state machine rather than a
//! raw `Resource<…>`. That makes the selector logic identical between
//! production (where a background `use_future` updates the signal) and
//! tests (where the harness sets the signal to a canned value with no fetch).

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::{self, ApiError};
use crate::client::dioxus::use_auth_gate;
use crate::client::session::RefreshGate;

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
/// Signal, so two consumers see the same updates.
#[derive(Clone, Copy)]
pub struct ServersContext {
    pub data: Signal<ServersData>,
}

/// Compound hook: build the [`ServersContext`], spawn the background fetch,
/// and provide the context for descendants. Designed to be called **directly**
/// from a component body (not inside another hook's closure) so the inner
/// `use_signal` / `use_future` / `use_context_provider` calls all run as
/// top-level hooks in the parent's hook list.
///
/// Fires the fetch exactly once per AppShell mount via `use_future`. A
/// refresh button + interval refresh land in Phase 2 with the rest of the
/// live-telemetry story.
pub fn mount_servers_context() -> ServersContext {
    let gate = use_auth_gate();
    let mut data: Signal<ServersData> = use_signal(|| ServersData::Loading);
    let _ = use_future(move || {
        let gate = gate.clone();
        async move {
            let next = match fetch_servers(gate).await {
                Ok(rows) => ServersData::Loaded(rows),
                Err(e) => ServersData::Error(e),
            };
            data.set(next);
        }
    });
    let ctx = ServersContext { data };
    use_context_provider(|| ctx);
    ctx
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
}
