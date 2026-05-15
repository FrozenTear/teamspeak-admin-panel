//! `/servers` — list configured TS6 connections + add-server CTA.
//!
//! Replaces [PURA-213](/PURA/issues/PURA-213)'s catch-all destination for
//! the dashboard empty-state CTA ("Add a server" on `dashboard_placeholder.rs`)
//! and the header [`ServerSelector`](crate::ui::components::ServerSelector)
//! footer ("Manage servers…"). Both link targets stayed `/servers` after
//! PURA-213 shipped, so adding an explicit `#[route("/servers")]` is the
//! whole wire-up — no callers need to change.
//!
//! Data comes from the shared [`ServersContext`](crate::ui::layout::ServersContext)
//! mounted by [`AppShell`](crate::ui::layout::AppShell), the same source the
//! header selector reads. Re-using that hoisted resource means visiting
//! `/servers` does not fire a second `GET /api/servers` when the chrome
//! already has the list in hand.
//!
//! ## Scope (PURA-218)
//!
//! Phase 1 ships the read-side surface only:
//!
//! - List of `ServerSummary` rows (name, host, transport, status).
//! - Empty / loading / error states matching the dashboard pattern.
//! - "Add a server" CTA → `/setup`. The setup wizard guards itself with
//!   `GET /api/setup/status.needsSetup`; once any user exists it redirects
//!   to `/login`, so this link only does the right thing for first-run
//!   panels. A follow-up ticket rewrites the wizard into a slim add-server
//!   form usable post-bootstrap.
//!
//! Edit + delete affordances are deferred — `PATCH/DELETE /api/servers/:id`
//! aren't implemented in `routes/servers.rs` yet (it documents them as
//! "Out of scope" under PURA-22 phase 1). Showing disabled buttons would
//! ship a half-finished feature, so the page exposes only the columns it
//! can render truthfully today, with a follow-up issue tracking the rest.

use dioxus::prelude::*;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::ApiError;
use crate::client::dioxus::use_session;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::{ServersData, use_servers_context};
use crate::ui::routes::Route;

#[component]
pub fn ServersIndexPage() -> Element {
    // AppShell already redirects anonymous sessions to /login; render
    // nothing for the frame between auth state change and effect firing.
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }

    let ctx = use_servers_context();
    let snap = ctx.data.read().clone();

    rsx! {
        div { class: "crumb", "Servers" }
        h1 { "Servers" }

        section { class: "stack-md",
            { match snap {
                ServersData::Loading => rsx! { ServersIndexSkeleton {} },
                ServersData::Error(err) => rsx! { ServersIndexError { error: err } },
                ServersData::Loaded(rows) if rows.is_empty() => rsx! { ServersIndexEmpty {} },
                ServersData::Loaded(rows) => rsx! { ServersTable { rows: rows } },
            } }
        }
    }
}

#[component]
fn ServersIndexSkeleton() -> Element {
    rsx! {
        div { class: "servers-loading",
            span { class: "sr-only", role: "status", "aria-live": "polite",
                "Loading server list…"
            }
            div { class: "servers-loading-rows", "aria-hidden": "true",
                for _ in 0..3 {
                    div { class: "skeleton skeleton-line wide" }
                }
            }
        }
    }
}

#[component]
fn ServersIndexEmpty() -> Element {
    rsx! {
        div { class: "empty",
            div { class: "icon", "⬢" }
            h3 { "No servers configured" }
            p { "Add a TeamSpeak server to start managing it from this panel." }
            div { class: "actions",
                a { class: "btn btn-primary", href: "/setup", "Add a server" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServersIndexErrorProps {
    error: ApiError,
}

#[component]
fn ServersIndexError(props: ServersIndexErrorProps) -> Element {
    let (title, body) = error_copy(&props.error);
    let show_signin_cta = props.error.is_unauthorized();
    rsx! {
        Banner { variant: BannerVariant::Danger, title: title.to_string(),
            "{body}"
            if show_signin_cta {
                div { class: "banner-actions",
                    SignInAgainButton {}
                }
            }
        }
    }
}

/// PURA-225 — escape-hatch CTA rendered next to any 401 banner on an
/// authenticated surface. Clears the persisted session and navigates to
/// `/login` so an operator whose access token is expired but whose refresh
/// path is wedged (transient 5xx / proxy strip / unknown 401 sub-code) has
/// a one-click path back to a working state instead of being stranded on
/// "Session expired" with no affordance. AppShell's auth-gate effect will
/// also fire on the signal flip, but the explicit `nav.replace` removes
/// the dependency on whether `use_effect` re-runs on the same render.
#[component]
fn SignInAgainButton() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let on_click = move |_| {
        let next = current_authed_path();
        session.replace(AuthState::Anonymous);
        nav.replace(Route::LoginPage { next: Some(next) });
    };
    rsx! {
        button {
            r#type: "button",
            class: "btn btn-primary",
            onclick: on_click,
            "Sign in again"
        }
    }
}

/// Best-effort `?next=` capture for the sign-in-again handler. On WASM we
/// read `window.location.pathname + search`; on SSR (where this never
/// actually fires from a user click) we fall back to `/servers`. The
/// global `current_path()` in `ui::layout` is module-private so this
/// duplicates the minimal slice we need.
fn current_authed_path() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            let mut out = loc.pathname().unwrap_or_else(|_| "/".into());
            if let Ok(search) = loc.search()
                && !search.is_empty()
            {
                out.push_str(&search);
            }
            return out;
        }
        "/".into()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "/servers".into()
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServersTableProps {
    rows: Vec<ServerSummary>,
}

#[component]
fn ServersTable(props: ServersTableProps) -> Element {
    rsx! {
        div { class: "servers-toolbar",
            a { class: "btn btn-primary", href: "/setup", "Add a server" }
        }
        table { class: "data-table",
            "aria-label": "Configured TeamSpeak servers",
            thead {
                tr {
                    th { scope: "col", "Name" }
                    th { scope: "col", "Host" }
                    th { scope: "col", "Transport" }
                    th { scope: "col", "SSH" }
                    th { scope: "col", "Status" }
                }
            }
            tbody {
                for s in props.rows.iter() {
                    {
                        let s = s.clone();
                        rsx! {
                            tr { key: "{s.id}",
                                td { class: "server-name", "{s.name}" }
                                td { class: "server-host", "{s.host}" }
                                td { "{transport_label(&s)}" }
                                td { "{ssh_label(&s)}" }
                                td {
                                    if s.enabled {
                                        span { class: "pill pill-ok", "Enabled" }
                                    } else {
                                        span { class: "pill pill-muted", "Disabled" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn transport_label(s: &ServerSummary) -> String {
    let scheme = if s.use_https { "https" } else { "http" };
    format!("{scheme}://{}:{}", s.host, s.webquery_port)
}

fn ssh_label(s: &ServerSummary) -> &'static str {
    if s.has_ssh_credentials {
        "Configured"
    } else {
        "—"
    }
}

fn error_copy(err: &ApiError) -> (&'static str, String) {
    match err {
        ApiError::Unauthorized(_) => (
            "Session expired",
            "Sign in again to view configured servers.".into(),
        ),
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut body = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                body.push_str(": ");
                body.push_str(d);
            }
            if let Some(c) = code {
                body.push_str(&format!(" (code {c})"));
            }
            ("Could not load servers", body)
        }
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            ("Could not load servers", format!("{status}: {message}"))
        }
        ApiError::Transport(m) => ("Could not load servers", format!("Transport error: {m}")),
        ApiError::Deserialise(m) => (
            "Unexpected response",
            format!("/api/servers returned an unexpected payload: {m}"),
        ),
        ApiError::UnsupportedTarget => (
            "Server list unavailable",
            "This view does not support the live server list.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use crate::ui::layout::ServersContext;
    use chrono::Utc;
    use std::sync::Arc;
    use ts6_manager_shared::auth::UserInfo;

    fn fixture(id: i64, name: &str, host: &str) -> ServerSummary {
        let now = Utc::now();
        ServerSummary {
            id,
            name: name.into(),
            host: host.into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: Some("admin".into()),
            has_ssh_credentials: true,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn transport_label_renders_https_prefix_when_use_https_true() {
        let s = fixture(1, "Primary", "ts.example.com");
        assert_eq!(transport_label(&s), "https://ts.example.com:10080");
    }

    #[test]
    fn transport_label_renders_http_prefix_when_use_https_false() {
        let mut s = fixture(1, "Primary", "ts.example.com");
        s.use_https = false;
        assert_eq!(transport_label(&s), "http://ts.example.com:10080");
    }

    #[test]
    fn ssh_label_reflects_configured_credentials_flag() {
        let mut s = fixture(1, "Primary", "ts.example.com");
        assert_eq!(ssh_label(&s), "Configured");
        s.has_ssh_credentials = false;
        assert_eq!(ssh_label(&s), "—");
    }

    #[test]
    fn error_copy_for_unauthorized_uses_session_expired_phrasing() {
        let (title, body) = error_copy(&ApiError::Unauthorized("expired".into()));
        assert_eq!(title, "Session expired");
        assert!(body.contains("Sign in"));
    }

    #[test]
    fn error_copy_for_bad_gateway_inlines_upstream_envelope() {
        let (_, body) = error_copy(&ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1153),
            details: Some("invalid serverID".into()),
        });
        assert!(body.contains("TeamSpeak API Error"));
        assert!(body.contains("invalid serverID"));
        assert!(body.contains("1153"));
    }

    // ── SSR markup contract ────────────────────────────────────────────────

    /// Synthetic single-route Router so [`ServersIndexPage`] (and the
    /// PURA-225 `SignInAgainButton` inside its 401 banner) can call
    /// `use_navigator()` during SSR without panicking. The production
    /// `Route::ServersIndexPage` lives under `#[layout(AppShell)]` which
    /// would pull in the whole chrome; the test only needs the navigator
    /// context, so we mount a stand-in enum with the same route shape.
    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum ServersIndexTestRoute {
        #[route("/")]
        TestHarness {},
    }

    thread_local! {
        static TEST_DATA: std::cell::RefCell<Option<ServersData>> =
            const { std::cell::RefCell::new(None) };
    }

    #[component]
    #[allow(non_snake_case)]
    fn TestHarness() -> Element {
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "rsoot".into(),
                    display_name: "Robert Soot".into(),
                    role: "admin".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
        });
        use_context_provider(|| provide_auth_gate(session));
        let data = TEST_DATA
            .with(|d| d.borrow().clone())
            .unwrap_or(ServersData::Loading);
        use_context_provider(|| ServersContext {
            data: Signal::new(data),
        });
        rsx! { ServersIndexPage {} }
    }

    fn render_with_state(data: ServersData) -> String {
        TEST_DATA.with(|d| *d.borrow_mut() = Some(data));
        let mut dom = VirtualDom::new(|| rsx! { Router::<ServersIndexTestRoute> {} });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        TEST_DATA.with(|d| *d.borrow_mut() = None);
        html
    }

    #[test]
    fn empty_state_renders_add_server_cta_pointing_at_setup() {
        let html = render_with_state(ServersData::Loaded(Vec::new()));
        assert!(html.contains("No servers configured"), "got: {html}");
        assert!(
            html.contains(r#"href="/setup""#),
            "empty-state CTA must link to /setup: {html}"
        );
    }

    #[test]
    fn loaded_state_renders_one_row_per_server_with_host_and_transport() {
        let rows = vec![
            fixture(1, "Primary", "ts1.example.com"),
            fixture(2, "Backup", "ts2.example.com"),
        ];
        let html = render_with_state(ServersData::Loaded(rows));
        assert!(html.contains("Primary"));
        assert!(html.contains("ts1.example.com"));
        assert!(html.contains("Backup"));
        assert!(html.contains("ts2.example.com"));
        assert!(
            html.contains("https://ts1.example.com:10080"),
            "transport column must surface scheme://host:port: {html}"
        );
        // Toolbar Add-server affordance still visible on non-empty list.
        assert!(
            html.contains(r#"href="/setup""#),
            "non-empty list still surfaces /setup CTA in toolbar: {html}"
        );
    }

    #[test]
    fn loading_state_announces_via_sr_only_status_region() {
        let html = render_with_state(ServersData::Loading);
        assert!(
            html.contains(r#"role="status""#),
            "loading state needs a live region for AT: {html}"
        );
        assert!(html.contains("Loading server list"));
    }

    #[test]
    fn error_state_renders_danger_banner_with_upstream_copy() {
        let html = render_with_state(ServersData::Error(ApiError::Transport(
            "connection refused".into(),
        )));
        // Banner danger styling produces the `banner-danger` class.
        assert!(
            html.contains("banner-danger") || html.contains("Could not load"),
            "danger banner expected for error state: {html}"
        );
        assert!(html.contains("connection refused"));
    }

    /// PURA-225 — when the `/api/servers` fetch is a 401, the danger
    /// banner MUST surface a primary "Sign in again" CTA so the operator
    /// has a one-click escape from the stuck-with-banner state. Asserting
    /// the literal label + a primary-button class pins the affordance so
    /// a future banner refactor that drops the CTA flags as a regression.
    #[test]
    fn unauthorized_error_state_renders_sign_in_again_cta() {
        let html = render_with_state(ServersData::Error(ApiError::Unauthorized(
            "Invalid or expired token".into(),
        )));
        assert!(
            html.contains("Sign in again"),
            "401 banner must include a `Sign in again` CTA: {html}"
        );
        assert!(
            html.contains("btn-primary"),
            "CTA must render as a primary button so it's the obvious next \
             action, not an inline link inside the body copy: {html}"
        );
    }

    /// PURA-225 — non-401 error states (transport / 5xx / 502) MUST NOT
    /// render the sign-in CTA. Those are recoverable on retry and showing
    /// "Sign in again" would mislead the operator into logging out for a
    /// transient backend hiccup.
    #[test]
    fn non_unauthorized_error_state_omits_sign_in_again_cta() {
        for err in [
            ApiError::Transport("connection refused".into()),
            ApiError::Server {
                status: 502,
                message: "bad gateway".into(),
            },
            ApiError::Server {
                status: 500,
                message: "boom".into(),
            },
        ] {
            let html = render_with_state(ServersData::Error(err.clone()));
            assert!(
                !html.contains("Sign in again"),
                "non-401 error {err:?} must NOT render the sign-in CTA: {html}"
            );
        }
    }
}
