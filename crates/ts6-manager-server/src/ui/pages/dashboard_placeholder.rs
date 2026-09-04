//! `/` route — operator dashboard.
//!
//! Wires the chrome to the live counts route shipped in
//! [PURA-23](/PURA/issues/PURA-23):
//! `GET /api/servers/:configId/vs/:sid/dashboard` (spec §7.19). The fetch
//! flows through [`crate::client::api::authorized_get_json`] so the
//! single-flight refresh gate handles `401 Invalid or expired token`
//! transparently — the dashboard never sees a stale-token race.
//!
//! Selection (PURA-31 follow-up): KPIs track the header selector's pick
//! via [`crate::ui::layout::ServersContext`]. The list itself is the
//! chrome's already-fetched `GET /api/servers` cache — this page does
//! **not** re-fetch it. Changing the pick restarts the KPI resource for
//! that `configId`. Virtual-server id stays pinned to
//! [`super::active_server::DEFAULT_VIRTUAL_SERVER_ID`] (`1`) until a live
//! vs-picker exists (spec §4.2.5). Access control stays on the API;
//! this surface does not implement `RequireServerAccess`.
//!
//! Render states (per the issue's "empty / loading / error / 502-from-TS"
//! contract):
//! - **Loading**: skeleton blocks sized to the KPI grid so the chrome
//!   doesn't reflow when data arrives (list still loading, or KPI fetch
//!   in flight after a pick).
//! - **No servers**: empty-state nudging the operator to `/servers`.
//! - **No selection**: servers exist but the header pick is empty / stale.
//! - **Loaded**: KPI grid with formatted online users, channels, uptime,
//!   bandwidth, ping, and packet loss.
//! - **Error**: surface-scoped `Banner` carrying the spec §7.0.2
//!   `{ error, code, details }` envelope verbatim when the upstream is the
//!   TS WebQuery (`502`); a generic copy otherwise. The same Banner
//!   pattern covers a failed chrome `/api/servers` load.
//!
//! Auth gating + logout still live in `AppShell` / `Header`; this component
//! is rendered only when a session exists.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::dashboard::DashboardData;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::{ServersData, use_servers_context};
use crate::ui::pages::active_server;

/// Outcome of a dashboard load. Loading is `None` on the resource itself —
/// only `Ok` payload variants live here. The `Ready` payload is boxed so
/// the enum's footprint stays small (`ServerSummary` + `DashboardData`
/// together carry a chunk of strings + chrono timestamps).
#[derive(Clone, Debug)]
enum DashboardLoaded {
    /// Chrome list is still in flight — render the same skeleton as a
    /// KPI fetch so the grid does not pop in later.
    WaitingOnList,
    NoServers,
    NoSelection,
    Ready(Box<DashboardReadyPayload>),
}

#[derive(Clone, Debug)]
struct DashboardReadyPayload {
    server: ServerSummary,
    data: DashboardData,
}

#[component]
pub fn DashboardPlaceholder() -> Element {
    let session = use_session();
    let user = match &*session.state.read() {
        AuthState::Authenticated { user, .. } => user.clone(),
        // AppShell already redirects on Anonymous; render nothing as a guard
        // for the brief frame between state change and effect firing.
        AuthState::Anonymous => return rsx! { "" },
    };

    let gate = use_auth_gate();
    let servers_ctx = use_servers_context();

    // Dioxus 0.7: `use_resource` re-runs whenever a tracked signal it
    // depends on changes. Reading the chrome list + selected id inside
    // the closure means a header pick (or list arrival) cancels any
    // in-flight KPI fetch and starts a new one for that `configId`.
    // A refresh button + interval refresh stay Phase-2.
    let dashboard = use_resource(move || {
        let gate = gate.clone();
        let list = servers_ctx.data.read().clone();
        let selected_id = *servers_ctx.selected.read();
        async move { fetch_dashboard(gate, list, selected_id).await }
    });

    rsx! {
        div { class: "crumb", "Dashboard" }
        h1 { "Welcome, {user.display_name}" }

        section { class: "stack-md",
            { match &*dashboard.read_unchecked() {
                // Initial render + WASM in-flight, or chrome list still
                // arriving: skeleton stand-in.
                None | Some(Ok(DashboardLoaded::WaitingOnList)) => {
                    rsx! { DashboardSkeleton {} }
                }
                Some(Ok(DashboardLoaded::NoServers)) => rsx! { DashboardEmpty {} },
                Some(Ok(DashboardLoaded::NoSelection)) => rsx! { DashboardNoSelection {} },
                Some(Ok(DashboardLoaded::Ready(payload))) => rsx! {
                    DashboardReady {
                        config_name: payload.server.name.clone(),
                        host: payload.server.host.clone(),
                        data: payload.data.clone(),
                    }
                },
                Some(Err(err)) => {
                    let (title, body) = error_copy(err);
                    let hint = err.transport_hint().map(str::to_string);
                    rsx! {
                        DashboardErrorView {
                            title: title.to_string(),
                            body: body,
                            transport_hint: hint,
                        }
                    }
                }
            } }
        }
    }
}

/// Pure chrome → KPI decision. Unit-tested without a Dioxus runtime so we
/// can pin "use the selected id, never the first granted row, never a
/// second `/api/servers` fetch".
#[derive(Clone, Debug, PartialEq)]
enum DashboardSelection {
    WaitingOnList,
    NoServers,
    NoSelection,
    /// Boxed so the enum stays small — `ServerSummary` is ~208 bytes
    /// (`large_enum_variant` on rustc 1.95 / clippy `-D warnings`).
    Selected(Box<ServerSummary>),
}

fn selection_from_context(list: &ServersData, selected_id: Option<i64>) -> DashboardSelection {
    match list {
        ServersData::Loading => DashboardSelection::WaitingOnList,
        // List errors are turned into a Banner by `fetch_dashboard` before
        // this helper is asked to pick a row. Treat them as "not ready"
        // so a failed chrome load can never resolve to a Selected row.
        ServersData::Error(_) => DashboardSelection::WaitingOnList,
        ServersData::Loaded(rows) if rows.is_empty() => DashboardSelection::NoServers,
        ServersData::Loaded(_) => match active_server::resolve_selected(list, selected_id) {
            Some(server) => DashboardSelection::Selected(Box::new(server)),
            None => DashboardSelection::NoSelection,
        },
    }
}

async fn fetch_dashboard(
    gate: Arc<RefreshGate>,
    list: ServersData,
    selected_id: Option<i64>,
) -> Result<DashboardLoaded, ApiError> {
    if let ServersData::Error(err) = &list {
        return Err(err.clone());
    }

    match selection_from_context(&list, selected_id) {
        DashboardSelection::WaitingOnList => Ok(DashboardLoaded::WaitingOnList),
        DashboardSelection::NoServers => Ok(DashboardLoaded::NoServers),
        DashboardSelection::NoSelection => Ok(DashboardLoaded::NoSelection),
        DashboardSelection::Selected(server) => {
            // Phase-1 pin: no live virtual-server picker yet. Spec §4.2.5
            // defaults `virtualServerId` to 1; swap
            // [`active_server::DEFAULT_VIRTUAL_SERVER_ID`] when a picker lands.
            let path = format!(
                "/api/servers/{}/vs/{}/dashboard",
                server.id,
                active_server::DEFAULT_VIRTUAL_SERVER_ID
            );
            let data: DashboardData =
                api::authorized_get_json(&gate, &api::api_base(), &path).await?;
            Ok(DashboardLoaded::Ready(Box::new(DashboardReadyPayload {
                server: *server,
                data,
            })))
        }
    }
}

#[component]
fn DashboardSkeleton() -> Element {
    rsx! {
        div { class: "dashboard-loading",
            // Single visually-hidden announcement so screen readers learn
            // that data is loading. The shimmer blocks themselves are
            // marked aria-hidden so AT users don't get a stream of empty
            // div announcements.
            span { class: "sr-only",
                role: "status",
                "aria-live": "polite",
                "Loading dashboard data…"
            }
            div { class: "dashboard-meta-skeleton", "aria-hidden": "true",
                div { class: "skeleton skeleton-line wide" }
                div { class: "skeleton skeleton-line narrow" }
            }
            div { class: "dashboard-kpis", "aria-hidden": "true",
                for _ in 0..6 {
                    div { class: "kpi",
                        div { class: "skeleton skeleton-line short" }
                        div { class: "skeleton skeleton-line tall" }
                    }
                }
            }
        }
    }
}

#[component]
fn DashboardEmpty() -> Element {
    rsx! {
        div { class: "empty",
            div { class: "icon", "⬢" }
            h3 { "No TeamSpeak servers configured yet" }
            p {
                "Add the WebQuery credentials for your TS6 instance so the "
                "dashboard can surface live counts, bandwidth, and uptime."
            }
            div { class: "actions",
                a { class: "btn btn-primary", href: "/servers", "Add a server" }
            }
        }
    }
}

#[component]
fn DashboardNoSelection() -> Element {
    rsx! {
        div { class: "empty",
            div { class: "icon", "⬢" }
            h3 { "No server selected" }
            p {
                "Pick a TeamSpeak instance from the header selector to load "
                "live counts, bandwidth, and uptime."
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DashboardReadyProps {
    /// `server_connections.name` — the operator-chosen label for the
    /// configured TS instance. Surfaced as a tooltip on the host so the
    /// header heading stays focused on the live `serverName`.
    config_name: String,
    /// `server_connections.host` — useful diagnostic in the meta strip.
    host: String,
    data: DashboardData,
}

#[component]
fn DashboardReady(props: DashboardReadyProps) -> Element {
    let DashboardReadyProps {
        config_name,
        host,
        data,
    } = props;
    rsx! {
        div { class: "dashboard-meta",
            div { class: "dashboard-meta-name", "{data.server_name}" }
            div { class: "dashboard-meta-tech",
                span { "{data.platform}" }
                span { class: "dot", "·" }
                span { "TeamSpeak {data.version}" }
                span { class: "dot", "·" }
                span { class: "config-host", title: "{config_name}", "{host}" }
            }
        }
        // PURA-61: parent is a `<div>` (not `<dl>`) so we can host a sibling
        // `<p class="kpi-hint">` per card without violating axe's
        // `definition-list` rule (which forbids non-`<dt>`/`<dd>`/`<div>`
        // children of `<dl>`, and inside a wrapping `<div>` forbids `<div>`
        // siblings of `<dt>`/`<dd>`). The dt/dd pair lives in its own
        // `<dl class="kpi-pair">` per KPI so the screen-reader announcement
        // contract ("Online users — 4 / 32") is preserved.
        div { class: "dashboard-kpis",
            DashboardKpi {
                label: "Online users",
                value: format_clients(data.online_users, data.max_clients),
                hint: format!("{} of {}", data.online_users, data.max_clients),
            }
            DashboardKpi {
                label: "Channels",
                value: format!("{}", data.channel_count),
                hint: "Includes spacers".to_string(),
            }
            DashboardKpi {
                label: "Uptime",
                value: format_uptime(data.uptime),
                hint: format!("{} seconds", data.uptime),
            }
            DashboardKpi {
                label: "Bandwidth in",
                value: format_bytes_per_sec(data.bandwidth.incoming),
                hint: "Last 1s sample".to_string(),
            }
            DashboardKpi {
                label: "Bandwidth out",
                value: format_bytes_per_sec(data.bandwidth.outgoing),
                hint: "Last 1s sample".to_string(),
            }
            DashboardKpi {
                label: "Ping",
                value: format_ping(data.ping),
                hint: format!("{:.1}% packet loss", data.packetloss),
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DashboardKpiProps {
    label: &'static str,
    value: String,
    hint: String,
}

#[component]
fn DashboardKpi(props: DashboardKpiProps) -> Element {
    rsx! {
        div { class: "kpi",
            dl { class: "kpi-pair",
                dt { class: "kpi-label", "{props.label}" }
                dd { class: "kpi-value", "{props.value}" }
            }
            p { class: "kpi-hint", "{props.hint}" }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DashboardErrorViewProps {
    title: String,
    body: String,
    /// PURA-211 — when the upstream error is a transport-class failure
    /// (`code == -1` per spec §10.5), the banner appends a one-line
    /// loopback hint pointing the operator at the most common operator
    /// misconfiguration: WebQuery bound to 127.0.0.1 only while the
    /// stored host is a public DNS name. Empty when the hint does not
    /// apply (upstream upstream-code error, session expired, etc.).
    #[props(default)]
    transport_hint: Option<String>,
}

#[component]
fn DashboardErrorView(props: DashboardErrorViewProps) -> Element {
    rsx! {
        Banner { variant: BannerVariant::Danger, title: props.title,
            p { class: "dashboard-error-body", "{props.body}" }
            if let Some(hint) = props.transport_hint.as_deref() {
                p { class: "dashboard-error-hint", "{hint}" }
            }
        }
    }
}

/// Decide what banner copy fits the API error. The spec §7.0.2 envelope
/// keys are surfaced verbatim for the 502 path so an operator can paste the
/// `details` field straight into a bug report.
fn error_copy(err: &ApiError) -> (&'static str, String) {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut body = error.clone();
            if let Some(d) = details.as_deref().filter(|s| !s.is_empty()) {
                body.push_str(": ");
                body.push_str(d);
            }
            if let Some(c) = code {
                body.push_str(&format!(" (code {c})"));
            }
            ("Could not reach TeamSpeak", body)
        }
        ApiError::Unauthorized(_) => (
            "Session expired",
            "Your session ended. Sign in again to view live counts.".into(),
        ),
        // PURA-232 — see comment on the matching arm in
        // `ui/pages/servers_index.rs::error_copy`.
        ApiError::SessionAnonymous => (
            "Loading your dashboard…",
            "Session is initialising, this should clear in a moment.".into(),
        ),
        ApiError::Client { status, message } => (
            "Dashboard request rejected",
            format!("{status}: {message}"),
        ),
        ApiError::Server { .. } | ApiError::Transport(_) => (
            "Dashboard temporarily unavailable",
            "We could not reach the panel API. Retry in a moment, or check the panel logs if this persists.".into(),
        ),
        ApiError::Deserialise(m) => (
            "Unexpected response shape",
            format!("The dashboard endpoint returned data in an unexpected shape: {m}"),
        ),
        ApiError::UnsupportedTarget => (
            "Live data unavailable in this view",
            "The dashboard is only wired to live counts in the browser build.".into(),
        ),
    }
}

// ── Formatters ──────────────────────────────────────────────────────────

fn format_clients(online: u32, max: u32) -> String {
    format!("{online} / {max}")
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let s = secs % 60;
    if mins < 60 {
        return format!("{mins}m {s:02}s");
    }
    let hours = mins / 60;
    let m = mins % 60;
    if hours < 24 {
        return format!("{hours}h {m:02}m");
    }
    let days = hours / 24;
    let h = hours % 24;
    format!("{days}d {h:02}h")
}

fn format_bytes_per_sec(bps: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let v = bps as f64;
    if v < KIB {
        format!("{bps} B/s")
    } else if v < MIB {
        format!("{:.1} KiB/s", v / KIB)
    } else if v < GIB {
        format!("{:.1} MiB/s", v / MIB)
    } else {
        format!("{:.2} GiB/s", v / GIB)
    }
}

fn format_ping(ms: f64) -> String {
    if !ms.is_finite() {
        return "—".into();
    }
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.0} ms", ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_clients_renders_online_over_max() {
        assert_eq!(format_clients(4, 32), "4 / 32");
    }

    #[test]
    fn format_uptime_seconds_path() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(45), "45s");
    }

    #[test]
    fn format_uptime_minutes_pads_seconds() {
        assert_eq!(format_uptime(60), "1m 00s");
        assert_eq!(format_uptime(125), "2m 05s");
    }

    #[test]
    fn format_uptime_hours_pads_minutes() {
        assert_eq!(format_uptime(3600), "1h 00m");
        assert_eq!(format_uptime(3725), "1h 02m");
    }

    #[test]
    fn format_uptime_days_pads_hours() {
        assert_eq!(format_uptime(86_400), "1d 00h");
        assert_eq!(format_uptime(90_061), "1d 01h");
    }

    #[test]
    fn format_bytes_picks_unit_at_each_threshold() {
        assert_eq!(format_bytes_per_sec(0), "0 B/s");
        assert_eq!(format_bytes_per_sec(512), "512 B/s");
        assert_eq!(format_bytes_per_sec(1024), "1.0 KiB/s");
        assert_eq!(format_bytes_per_sec(2_500_000), "2.4 MiB/s");
    }

    #[test]
    fn format_ping_keeps_decimal_under_ten_ms() {
        assert_eq!(format_ping(2.5), "2.5 ms");
        assert_eq!(format_ping(42.7), "43 ms");
    }

    #[test]
    fn format_ping_handles_non_finite() {
        assert_eq!(format_ping(f64::NAN), "—");
        assert_eq!(format_ping(f64::INFINITY), "—");
    }

    #[test]
    fn error_copy_for_bad_gateway_includes_details_and_code() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1153),
            details: Some("invalid serverID".into()),
        };
        let (title, body) = error_copy(&err);
        assert_eq!(title, "Could not reach TeamSpeak");
        assert!(body.contains("TeamSpeak API Error"), "got: {body}");
        assert!(body.contains("invalid serverID"), "got: {body}");
        assert!(body.contains("(code 1153)"), "got: {body}");
    }

    #[test]
    fn error_copy_for_bad_gateway_omits_empty_details() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: None,
            details: Some("".into()),
        };
        let (_, body) = error_copy(&err);
        assert!(
            !body.contains(": "),
            "empty details slipped into body: {body}"
        );
    }

    #[test]
    fn error_copy_for_unauthorized_uses_session_expired_copy() {
        let err = ApiError::Unauthorized("Invalid or expired token".into());
        let (title, _) = error_copy(&err);
        assert_eq!(title, "Session expired");
    }

    #[test]
    fn error_copy_for_transport_uses_temp_unavailable_copy() {
        let err = ApiError::Transport("net::ERR".into());
        let (title, _) = error_copy(&err);
        assert_eq!(title, "Dashboard temporarily unavailable");
    }

    fn fixture(id: i64, name: &str) -> ServerSummary {
        let now = chrono::Utc::now();
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
    fn selection_waits_while_chrome_list_is_loading() {
        assert_eq!(
            selection_from_context(&ServersData::Loading, Some(1)),
            DashboardSelection::WaitingOnList
        );
    }

    #[test]
    fn selection_is_no_servers_when_list_is_empty() {
        assert_eq!(
            selection_from_context(&ServersData::Loaded(Vec::new()), None),
            DashboardSelection::NoServers
        );
    }

    #[test]
    fn selection_is_no_selection_when_pick_is_missing_or_stale() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary"), fixture(9, "Backup")]);
        assert_eq!(
            selection_from_context(&list, None),
            DashboardSelection::NoSelection
        );
        assert_eq!(
            selection_from_context(&list, Some(99)),
            DashboardSelection::NoSelection
        );
    }

    #[test]
    fn selection_uses_the_picked_row_not_the_first_granted() {
        let list = ServersData::Loaded(vec![fixture(7, "Primary"), fixture(9, "Backup")]);
        match selection_from_context(&list, Some(9)) {
            DashboardSelection::Selected(server) => {
                assert_eq!(server.id, 9);
                assert_eq!(server.name, "Backup");
            }
            other => panic!("expected Selected(Backup), got {other:?}"),
        }
    }

    #[test]
    fn kpi_path_pins_phase1_virtual_server_id() {
        assert_eq!(active_server::DEFAULT_VIRTUAL_SERVER_ID, 1);
    }
}
