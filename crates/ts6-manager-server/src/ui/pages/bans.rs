//! `/bans` — list / add / remove bans. PURA-73.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{BanCreateRequest, BanCreated, BanListItem};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::use_ws_hub;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

#[component]
pub fn BansPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let hub = use_ws_hub();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Bans" }
            h1 { "Bans" }
            div { class: "empty",
                div { class: "icon", "⊘" }
                h3 { "No server selected" }
                p { "Add a server to manage bans." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let mut bans_resource = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_bans(gate, server_id, sid).await }
        }
    });
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut bans: Signal<Vec<BanListItem>> = use_signal(Vec::new);
    {
        use_effect(move || match &*bans_resource.read_unchecked() {
            Some(Ok(rows)) => {
                bans.set(rows.clone());
                error.set(None);
            }
            Some(Err(e)) => error.set(Some(e.clone())),
            None => {}
        });
    }

    // WS — bans publish on the `clients` topic per the route comment in
    // routes/control/bans.rs.
    {
        let hub = hub.clone();
        let _resource = use_resource(move || {
            let hub = hub.clone();
            async move {
                let topic = format!("server:{server_id}:clients");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(env) = rx.next().await {
                    if matches!(env.kind.as_str(), "ts:ban:added" | "ts:ban:deleted") {
                        bans_resource.restart();
                    }
                }
            }
        });
    }

    let mut form_ip: Signal<String> = use_signal(String::new);
    let mut form_uid: Signal<String> = use_signal(String::new);
    let mut form_name: Signal<String> = use_signal(String::new);
    let mut form_reason: Signal<String> = use_signal(String::new);
    let mut form_duration: Signal<String> = use_signal(String::new);
    let mut form_busy: Signal<bool> = use_signal(|| false);

    let on_create = {
        let gate = gate.clone();
        move |_| {
            if *form_busy.read() {
                return;
            }
            let ip = trim_to_option(&form_ip.read());
            let uid = trim_to_option(&form_uid.read());
            let name = trim_to_option(&form_name.read());
            if ip.is_none() && uid.is_none() && name.is_none() {
                toaster.push(
                    ToastVariant::Warning,
                    "Provide at least one matcher",
                    Some("Set an IP, UID, or name to create a ban.".into()),
                );
                return;
            }
            let reason = trim_to_option(&form_reason.read());
            let duration = form_duration
                .read()
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0);
            let req = BanCreateRequest {
                ip,
                uid,
                my_ts_id: None,
                name,
                reason,
                duration,
            };
            let gate = gate.clone();
            let toaster = toaster;
            form_busy.set(true);
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/bans");
                let res = api::authorized_post_json::<_, BanCreated>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                form_busy.set(false);
                match res {
                    Ok(BanCreated { banid }) => {
                        toaster.push(ToastVariant::Success, format!("Ban #{banid} added"), None);
                        form_ip.set(String::new());
                        form_uid.set(String::new());
                        form_name.set(String::new());
                        form_reason.set(String::new());
                        form_duration.set(String::new());
                        bans_resource.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not add ban",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let make_delete = {
        let gate = gate.clone();
        move |banid: i64| {
            let gate = gate.clone();
            let toaster = toaster;
            spawn(async move {
                let path = format!("/api/servers/{server_id}/vs/{sid}/bans/{banid}");
                match api::authorized_delete(&gate, &api::api_base(), &path).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, format!("Ban #{banid} removed"), None);
                        bans_resource.restart();
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not remove ban",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "crumb", "Bans · {server_name}" }
        h1 { "Bans" }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load bans".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            form { class: "ban-create",
                "aria-label": "Add a ban",
                onsubmit: move |evt| { evt.prevent_default(); on_create.clone()(()); },
                div { class: "form-row",
                    label { r#for: "ban-ip", "IP" }
                    input { id: "ban-ip", class: "input", placeholder: "e.g. 203.0.113.4",
                        value: "{form_ip.read()}",
                        oninput: move |e| form_ip.set(e.value()),
                    }
                }
                div { class: "form-row",
                    label { r#for: "ban-uid", "UID" }
                    input { id: "ban-uid", class: "input", placeholder: "TS unique identifier",
                        value: "{form_uid.read()}",
                        oninput: move |e| form_uid.set(e.value()),
                    }
                }
                div { class: "form-row",
                    label { r#for: "ban-name", "Name" }
                    input { id: "ban-name", class: "input", placeholder: "Nickname pattern",
                        value: "{form_name.read()}",
                        oninput: move |e| form_name.set(e.value()),
                    }
                }
                div { class: "form-row",
                    label { r#for: "ban-reason", "Reason" }
                    input { id: "ban-reason", class: "input", placeholder: "Audit text",
                        value: "{form_reason.read()}",
                        oninput: move |e| form_reason.set(e.value()),
                    }
                }
                div { class: "form-row",
                    label { r#for: "ban-duration", "Duration (seconds, 0 = permanent)" }
                    input { id: "ban-duration", class: "input", placeholder: "0",
                        inputmode: "numeric",
                        value: "{form_duration.read()}",
                        oninput: move |e| form_duration.set(e.value()),
                    }
                }
                Button { variant: ButtonVariant::Primary, kind: ButtonType::Submit, loading: *form_busy.read(),
                    "Add ban"
                }
            }

            BansTable { rows: bans.read().clone(), on_delete: EventHandler::new(make_delete) }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct BansTableProps {
    rows: Vec<BanListItem>,
    on_delete: EventHandler<i64>,
}

#[component]
fn BansTable(props: BansTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "⊘" }
                h3 { "No active bans" }
                p { "Bans will appear here as they're added." }
            }
        };
    }
    rsx! {
        table { class: "data-table",
            "aria-label": "Active bans",
            thead {
                tr {
                    th { scope: "col", "ID" }
                    th { scope: "col", "Match" }
                    th { scope: "col", "Reason" }
                    th { scope: "col", "Duration" }
                    th { scope: "col", "Invoker" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for r in props.rows.iter() {
                    {
                        let r = r.clone();
                        let banid = r.banid;
                        let on_delete = props.on_delete;
                        rsx! {
                            tr { key: "{banid}",
                                td { "{banid}" }
                                td { class: "match-cell",
                                    if !r.ip.is_empty() { div { "IP: {r.ip}" } }
                                    if !r.uid.is_empty() { div { "UID: {r.uid}" } }
                                    if !r.name.is_empty() { div { "Name: {r.name}" } }
                                    if !r.lastnickname.is_empty() { div { class: "muted", "last seen as {r.lastnickname}" } }
                                }
                                td { "{r.reason}" }
                                td {
                                    if r.duration == 0 { "Permanent" } else { "{r.duration}s" }
                                }
                                td { "{r.invokername}" }
                                td { class: "actions-col",
                                    Button {
                                        variant: ButtonVariant::Danger,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_delete.call(banid),
                                        "Remove"
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

fn trim_to_option(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

async fn fetch_bans(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<BanListItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/bans");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
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
        ApiError::UnsupportedTarget => "Ban list unavailable in this view.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_collapses_whitespace_to_none() {
        assert_eq!(trim_to_option(""), None);
        assert_eq!(trim_to_option("   "), None);
        assert_eq!(trim_to_option("  ok  "), Some("ok".into()));
    }
}
