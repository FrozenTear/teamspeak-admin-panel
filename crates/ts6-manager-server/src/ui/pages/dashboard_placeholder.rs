//! `/` route — operator dashboard.
//!
//! Wires the chrome to the live counts route shipped in
//! [PURA-23](/PURA/issues/PURA-23):
//! `GET /api/servers/:configId/vs/:sid/dashboard` (spec §7.19). The fetch
//! flows through [`crate::client::api::authorized_get_json`] so the
//! single-flight refresh gate handles `401 Invalid or expired token`
//! transparently — the dashboard never sees a stale-token race.
//!
//! Phase-1 selection rules (PURA-31):
//! - The "selected server" is the first row from `GET /api/servers`. The
//!   header server-selector is being wired to a live source in a sibling
//!   ticket; until that selection state is exposed via context, defaulting
//!   to the first granted row keeps the dashboard testable end-to-end.
//! - The virtual-server id defaults to `1` per spec §4.2.5. A future
//!   `serverlist`-backed picker (Phase 2) replaces the constant.
//!
//! Render states (per the issue's "empty / loading / error / 502-from-TS"
//! contract):
//! - **Loading**: skeleton blocks sized to the KPI grid so the chrome
//!   doesn't reflow when data arrives.
//! - **No servers**: empty-state nudging the operator to `/servers`.
//! - **Loaded**: KPI grid with formatted online users, channels, uptime,
//!   bandwidth, ping, and packet loss.
//! - **Error**: surface-scoped `Banner` carrying the spec §7.0.2
//!   `{ error, code, details }` envelope verbatim when the upstream is the
//!   TS WebQuery (`502`); a generic copy otherwise.
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

/// Spec §4.2.5 — `virtualServerId` defaults to `1`. Surface-level constant
/// so the day a vs-picker exists, the only change is replacing the value.
const DEFAULT_VIRTUAL_SERVER_ID: i64 = 1;

/// Outcome of a dashboard load. Loading is `None` on the resource itself —
/// only `Ok` payload variants live here. The `Ready` payload is boxed so
/// the enum's footprint stays small (`ServerSummary` + `DashboardData`
/// together carry a chunk of strings + chrono timestamps).
#[derive(Clone, Debug)]
enum DashboardLoaded {
    NoServers,
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

    // Dioxus 0.7 quirk: `use_resource` re-runs whenever a tracked signal it
    // depends on changes. We don't track any signals inside the future, so
    // this fires exactly once per mount — which is the contract Phase 1
    // wants (no polling). A `refresh` button + interval refresh land in the
    // separate Phase-2 ticket per the issue's "Out of scope" list.
    let dashboard = use_resource(move || {
        let gate = gate.clone();
        async move { fetch_dashboard(gate).await }
    });

    rsx! {
        div { class: "crumb", "Dashboard" }
        h1 { "Welcome, {user.display_name}" }

        section { class: "stack-md",
            { match &*dashboard.read_unchecked() {
                // Initial render + WASM in-flight: skeleton stand-in.
                None => rsx! { DashboardSkeleton {} },
                Some(Ok(DashboardLoaded::NoServers)) => rsx! { DashboardEmpty {} },
                Some(Ok(DashboardLoaded::Ready(payload))) => rsx! {
                    DashboardReady {
                        config_name: payload.server.name.clone(),
                        host: payload.server.host.clone(),
                        data: payload.data.clone(),
                    }
                },
                Some(Err(err)) => {
                    let (title, body) = error_copy(err);
                    rsx! {
                        DashboardErrorView { title: title.to_string(), body: body }
                    }
                }
            } }
        }
    }
}

async fn fetch_dashboard(gate: Arc<RefreshGate>) -> Result<DashboardLoaded, ApiError> {
    let base = api::api_base();
    let servers: Vec<ServerSummary> =
        api::authorized_get_json(&gate, &base, "/api/servers").await?;

    let Some(server) = servers.into_iter().next() else {
        return Ok(DashboardLoaded::NoServers);
    };

    let path = format!(
        "/api/servers/{}/vs/{}/dashboard",
        server.id, DEFAULT_VIRTUAL_SERVER_ID
    );
    let data: DashboardData = api::authorized_get_json(&gate, &base, &path).await?;
    Ok(DashboardLoaded::Ready(Box::new(DashboardReadyPayload {
        server,
        data,
    })))
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
        dl { class: "dashboard-kpis",
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
            dt { class: "kpi-label", "{props.label}" }
            dd { class: "kpi-value", "{props.value}" }
            div { class: "kpi-hint", "{props.hint}" }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DashboardErrorViewProps {
    title: String,
    body: String,
}

#[component]
fn DashboardErrorView(props: DashboardErrorViewProps) -> Element {
    rsx! {
        Banner { variant: BannerVariant::Danger, title: props.title,
            "{props.body}"
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
        assert!(!body.contains(": "), "empty details slipped into body: {body}");
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
}
