//! `/server-info` — read-only `serverinfo` snapshot. PURA-73.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::ServerInfoResponse;

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

#[component]
pub fn ServerInfoPage() -> Element {
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
            div { class: "crumb", "Server info" }
            h1 { "Server info" }
            div { class: "empty",
                div { class: "icon", "⊙" }
                h3 { "No server selected" }
                p { "Add a server to inspect its serverinfo." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let host = server.host.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_info(gate, server_id, sid).await }
        }
    });

    rsx! {
        div { class: "crumb", "Server info · {server_name}" }
        h1 { "Server info" }
        section { class: "stack-md",
            { match &*resource.read_unchecked() {
                None => rsx! { ServerInfoSkeleton {} },
                Some(Ok(info)) => rsx! {
                    ServerInfoCard { info: info.clone(), config_name: server_name.clone(), host: host.clone() }
                },
                Some(Err(err)) => rsx! {
                    Banner { variant: BannerVariant::Danger, title: "Could not load server info".to_string(),
                        p { "{format_error(err)}" }
                        if let Some(hint) = err.transport_hint() {
                            p { class: "banner-hint", "{hint}" }
                        }
                    }
                }
            } }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServerInfoCardProps {
    info: ServerInfoResponse,
    config_name: String,
    host: String,
}

#[component]
fn ServerInfoCard(props: ServerInfoCardProps) -> Element {
    let i = props.info;
    rsx! {
        div { class: "info-grid",
            InfoRow { label: "Name".to_string(), value: i.virtualserver_name.clone() }
            InfoRow { label: "Host".to_string(), value: props.host.clone(), hint: Some(props.config_name.clone()) }
            InfoRow { label: "Platform".to_string(), value: i.virtualserver_platform.clone() }
            InfoRow { label: "Version".to_string(), value: i.virtualserver_version.clone() }
            InfoRow { label: "Slots".to_string(), value: i.virtualserver_maxclients.to_string() }
            InfoRow { label: "Uptime".to_string(), value: format_uptime(i.virtualserver_uptime) }
            InfoRow {
                label: "Avg ping".to_string(),
                value: format!("{:.1} ms", i.virtualserver_total_ping),
            }
            InfoRow {
                label: "Packet loss".to_string(),
                value: format!("{:.2}%", i.virtualserver_total_packetloss_total),
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct InfoRowProps {
    label: String,
    value: String,
    #[props(default)]
    hint: Option<String>,
}

#[component]
fn InfoRow(props: InfoRowProps) -> Element {
    rsx! {
        div { class: "info-row",
            dl { class: "info-pair",
                dt { class: "info-label", "{props.label}" }
                dd { class: "info-value", "{props.value}" }
            }
            if let Some(h) = props.hint {
                p { class: "info-hint", "{h}" }
            }
        }
    }
}

#[component]
fn ServerInfoSkeleton() -> Element {
    rsx! {
        div { class: "info-grid", "aria-busy": "true",
            for _ in 0..6 {
                div { class: "info-row",
                    div { class: "skeleton skeleton-line short" }
                    div { class: "skeleton skeleton-line tall" }
                }
            }
        }
    }
}

async fn fetch_info(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<ServerInfoResponse, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/info");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

fn format_uptime(secs: i64) -> String {
    if secs < 0 {
        return "—".to_string();
    }
    let secs = secs as u64;
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
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".to_string(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Server info unavailable in this view.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_handles_units() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(125), "2m 05s");
        assert_eq!(format_uptime(3725), "1h 02m");
        assert_eq!(format_uptime(90_061), "1d 01h");
        assert_eq!(format_uptime(-1), "—");
    }
}
