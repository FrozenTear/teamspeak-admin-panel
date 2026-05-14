//! `/logs` — tailing log viewer with severity filter. PURA-73.
//!
//! - REST: `GET /api/servers/{configId}/vs/{sid}/logs?after=…&severity=…`.
//!   The route returns at most 500 lines per call and surfaces `last_pos`
//!   so the next call can page forward.
//! - WS: subscribes to `server:{configId}:logs`. Today the topic is empty
//!   on Phase 2 (PURA-70a will wire the server-notify feed); the FE
//!   complements it with a 5-second background poll so the operator
//!   doesn't have to refresh manually.
//! - Severity: a free-text substring filter passed through `?severity=`.
//!   Spec §7.16 — the upstream doesn't filter, so the filter is applied
//!   server-side on egress.
//!
//! Logs are admin-gated by [`crate::ws::hub::Hub::authorize`]; non-admin
//! callers see the 403 path's `Banner` and the page renders no live tail.

use std::collections::VecDeque;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{LogTailQuery, LogTailResponse};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

const MAX_BUFFERED_LINES: usize = 1_000;
#[cfg(target_arch = "wasm32")]
const POLL_INTERVAL_MS: u32 = 5_000;

#[component]
pub fn LogsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Logs" }
            h1 { "Logs" }
            div { class: "empty",
                div { class: "icon", "≡" }
                h3 { "No server selected" }
                p { "Add a server to tail its logs." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut severity: Signal<String> = use_signal(String::new);
    let mut lines: Signal<VecDeque<String>> = use_signal(VecDeque::new);
    let mut last_pos: Signal<Option<i64>> = use_signal(|| None::<i64>);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut paused: Signal<bool> = use_signal(|| false);

    // Initial fetch + when the severity filter changes, drop the buffer
    // and re-fetch from the server's tail.
    {
        let gate = gate.clone();
        let sev_for_effect = severity;
        use_effect(move || {
            let _ = sev_for_effect.read();
            let gate = gate.clone();
            let sev = severity.read().trim().to_string();
            spawn(async move {
                let q = LogTailQuery {
                    after: None,
                    lines: Some(200),
                    severity: if sev.is_empty() { None } else { Some(sev) },
                };
                let res = fetch_logs(gate, server_id, sid, &q).await;
                match res {
                    Ok(r) => {
                        last_pos.set(r.last_pos);
                        let mut buf = VecDeque::new();
                        for l in r.lines {
                            buf.push_back(l.text);
                        }
                        lines.set(buf);
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        });
    }

    // Background poll. Dioxus's `use_future` runs once per mount, so we
    // implement the "every 5s" cadence with a TimeoutFuture loop. On
    // pause we skip the fetch entirely.
    #[cfg(target_arch = "wasm32")]
    {
        let gate = gate.clone();
        let _ = use_future(move || {
            let gate = gate.clone();
            async move {
                use gloo_timers::future::TimeoutFuture;
                loop {
                    TimeoutFuture::new(POLL_INTERVAL_MS).await;
                    if *paused.read() {
                        continue;
                    }
                    let after = *last_pos.read();
                    let sev = severity.read().trim().to_string();
                    let q = LogTailQuery {
                        after,
                        lines: Some(200),
                        severity: if sev.is_empty() { None } else { Some(sev) },
                    };
                    if let Ok(r) = fetch_logs(gate.clone(), server_id, sid, &q).await {
                        if let Some(np) = r.last_pos {
                            last_pos.set(Some(np));
                        }
                        if !r.lines.is_empty() {
                            let mut buf = lines.write();
                            for l in r.lines {
                                buf.push_back(l.text);
                                while buf.len() > MAX_BUFFERED_LINES {
                                    buf.pop_front();
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    let lines_snap = lines.read().clone();

    rsx! {
        div { class: "crumb", "Logs · {server_name}" }
        h1 { "Logs" }

        if let Some(err) = error.read().as_ref() {
            if matches!(err, ApiError::Client { status: 403, .. }) {
                Banner { variant: BannerVariant::Warning, title: "Logs are admin-only".to_string(),
                    "Your role doesn't grant access to this server's log feed."
                }
            } else {
                Banner { variant: BannerVariant::Danger, title: "Could not load logs".to_string(),
                    "{format_error(err)}"
                }
            }
        }

        section { class: "stack-md",
            div { class: "log-toolbar",
                label { class: "log-filter",
                    span { "Severity filter" }
                    input { class: "input",
                        placeholder: "e.g. ERROR, WARNING…",
                        value: "{severity.read()}",
                        oninput: move |e| severity.set(e.value()),
                    }
                }
                button {
                    r#type: "button",
                    class: if *paused.read() { "btn btn-secondary" } else { "btn btn-ghost" },
                    onclick: move |_| {
                        let next = !*paused.read();
                        paused.set(next);
                    },
                    if *paused.read() { "Resume" } else { "Pause" }
                }
            }

            ol { class: "log-stream",
                role: "log",
                "aria-live": "off",
                "aria-label": "Recent log lines",
                if lines_snap.is_empty() {
                    li { class: "log-empty",
                        "No log lines yet. The tail will appear here as TS emits new lines."
                    }
                }
                for (i, line) in lines_snap.iter().enumerate() {
                    li { key: "{i}", class: "log-line", "{line}" }
                }
            }
        }
    }
}

async fn fetch_logs(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    query: &LogTailQuery,
) -> Result<LogTailResponse, ApiError> {
    let mut path = format!("/api/servers/{config_id}/vs/{sid}/logs?");
    let mut first = true;
    let mut push = |k: &str, v: String| {
        if !first {
            path.push('&');
        }
        path.push_str(k);
        path.push('=');
        path.push_str(&urlencoding::encode(&v));
        first = false;
    };
    if let Some(a) = query.after {
        push("after", a.to_string());
    }
    if let Some(n) = query.lines {
        push("lines", n.to_string());
    }
    if let Some(s) = query.severity.as_deref() {
        if !s.is_empty() {
            push("severity", s.to_string());
        }
    }
    api::authorized_get_json::<LogTailResponse>(&gate, &api::api_base(), &path).await
}

fn format_error(err: &ApiError) -> String {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut s = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(": ");
                s.push_str(d);
            }
            if let Some(c) = code {
                s.push_str(&format!(" (code {c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Logs unavailable in this view.".into(),
    }
}
