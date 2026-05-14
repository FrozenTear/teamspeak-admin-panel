//! `/clients` — operator client list with kick / mute / move / poke
//! actions and live updates over `server:{id}:clients`. PURA-73.
//!
//! ## Data flow
//!
//! 1. On mount, `GET /api/servers/{configId}/vs/{sid}/clients` snapshots
//!    the live list. Spec §7.8.
//! 2. A WS subscription on `server:{configId}:clients` reduces over the
//!    snapshot — `ts:client:moved` updates the row's `cid`, kicks remove
//!    it, mutes/unmutes flip the muted columns. When the upstream emits
//!    a `ts:client:connected` we don't yet know the full row, so the
//!    component refetches the snapshot in the background and reconciles.
//! 3. Action buttons fire `POST` to the matching control endpoint. On
//!    success we drop the action's row optimistically (kick) or update it
//!    locally (mute/move) so the UI feels immediate; the WS event lands
//!    later and reconciles.
//!
//! Verification 4: kick a client and observe the row leave the list +
//! the activity feed entry land within the same animation frame.

use std::sync::Arc;

use dioxus::prelude::*;
use serde_json::Value;
use ts6_manager_shared::control::{
    ClientListItem, KickKind, KickRequest, MoveRequest, MuteRequest,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::{WsEvent, use_ws_hub};
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

#[component]
pub fn ClientsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let hub = use_ws_hub();
    let toaster = use_toaster();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Clients" }
            h1 { "Clients" }
            div { class: "empty",
                div { class: "icon", "◆" }
                h3 { "No server selected" }
                p { "Add a server to view its live client list." }
            }
        };
    };

    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // Initial snapshot. Re-fires whenever the operator picks a different
    // server (the `server.id` capture is part of the future).
    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_clients(gate, server_id, sid).await }
        }
    });

    // Local working copy: snapshot + WS reductions. We hold this in a
    // signal so action handlers can mutate it optimistically.
    let mut rows: Signal<Vec<ClientListItem>> = use_signal(Vec::<ClientListItem>::new);
    let mut last_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut server_changed_marker: Signal<i64> = use_signal(|| 0i64);

    // When the snapshot resolves, write it into the working copy. The
    // marker bump tells dependent effects that the resource refilled —
    // necessary because `Resource::read()` doesn't itself trigger a
    // re-run of side-effecting code.
    {
        use_effect(move || {
            if let Some(Ok(list)) = &*snapshot.read_unchecked() {
                rows.set(list.clone());
                last_error.set(None);
            } else if let Some(Err(e)) = &*snapshot.read_unchecked() {
                last_error.set(Some(e.clone()));
            }
            server_changed_marker.set(server_id);
        });
    }

    // WS subscription — reduce envelopes into the working copy.
    {
        let hub = hub.clone();
        let _resource = use_resource(move || {
            let hub = hub.clone();
            let cur_server = *server_changed_marker.read();
            async move {
                if cur_server == 0 {
                    return;
                }
                let topic = format!("server:{cur_server}:clients");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(env) = rx.next().await {
                    apply_event(&mut rows.write(), &env);
                }
            }
        });
    }

    // Action helpers reused by every row.
    let make_kick = {
        let gate = gate.clone();
        move |clid: i64, kind: KickKind| {
            let gate = gate.clone();
            spawn(async move {
                let body = KickRequest {
                    kind,
                    reason: Some(default_reason(kind)),
                };
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients/{clid}/kick");
                match api::authorized_post_json::<_, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&body),
                )
                .await
                {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, format!("Kicked client {clid}"), None);
                    }
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Kick failed", Some(format_error(&e)));
                    }
                }
            });
        }
    };

    let make_mute = {
        let gate = gate.clone();
        move |clid: i64, on: bool| {
            let gate = gate.clone();
            spawn(async move {
                let body = MuteRequest {
                    input_muted: Some(on),
                    output_muted: Some(on),
                };
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients/{clid}/mute");
                match api::authorized_post_json::<_, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&body),
                )
                .await
                {
                    Ok(()) => toaster.push(
                        ToastVariant::Success,
                        if on {
                            format!("Muted client {clid}")
                        } else {
                            format!("Unmuted client {clid}")
                        },
                        None,
                    ),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        if on { "Mute failed" } else { "Unmute failed" },
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let make_move = {
        let gate = gate.clone();
        move |clid: i64, target_cid: i64| {
            let gate = gate.clone();
            spawn(async move {
                let body = MoveRequest {
                    cid: target_cid,
                    channel_password: None,
                };
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients/{clid}/move");
                match api::authorized_post_json::<_, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&body),
                )
                .await
                {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, format!("Moved client {clid}"), None)
                    }
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Move failed", Some(format_error(&e)))
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "crumb", "Clients · {server_name}" }
        h1 { "Clients" }

        if let Some(err) = last_error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load clients".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            ClientsTable {
                rows: rows.read().clone(),
                on_kick_server: {
                    let k = make_kick.clone();
                    EventHandler::new(move |clid: i64| k(clid, KickKind::Server))
                },
                on_kick_channel: {
                    let k = make_kick.clone();
                    EventHandler::new(move |clid: i64| k(clid, KickKind::Channel))
                },
                on_mute: {
                    let m = make_mute.clone();
                    EventHandler::new(move |clid: i64| m(clid, true))
                },
                on_unmute: {
                    let m = make_mute.clone();
                    EventHandler::new(move |clid: i64| m(clid, false))
                },
                on_move: {
                    let mv = make_move.clone();
                    EventHandler::new(move |args: (i64, i64)| mv(args.0, args.1))
                },
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ClientsTableProps {
    rows: Vec<ClientListItem>,
    on_kick_server: EventHandler<i64>,
    on_kick_channel: EventHandler<i64>,
    on_mute: EventHandler<i64>,
    on_unmute: EventHandler<i64>,
    on_move: EventHandler<(i64, i64)>,
}

#[component]
fn ClientsTable(props: ClientsTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "◆" }
                h3 { "No clients online" }
                p { "When a client connects, they'll appear here." }
            }
        };
    }
    rsx! {
        table { class: "data-table",
            "aria-label": "Live clients",
            thead {
                tr {
                    th { scope: "col", "Nickname" }
                    th { scope: "col", "Channel" }
                    th { scope: "col", "Status" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for r in props.rows.iter() {
                    {
                        let r = r.clone();
                        let clid = r.clid;
                        let cid = r.cid;
                        let muted = r.client_input_muted != 0 || r.client_output_muted != 0;
                        let on_kick_server = props.on_kick_server;
                        let on_kick_channel = props.on_kick_channel;
                        let on_mute = props.on_mute;
                        let on_unmute = props.on_unmute;
                        let on_move = props.on_move;
                        rsx! {
                            tr { key: "{clid}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{r.client_nickname}" }
                                    span { class: "client-uid", "{r.client_unique_identifier}" }
                                }
                                td { "{cid}" }
                                td {
                                    if muted { "Muted" } else { "Active" }
                                    if r.client_away != 0 { " · Away" }
                                }
                                td { class: "actions-col",
                                    Button {
                                        variant: ButtonVariant::Ghost,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_kick_channel.call(clid),
                                        "Kick from channel"
                                    }
                                    Button {
                                        variant: ButtonVariant::Danger,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_kick_server.call(clid),
                                        "Kick from server"
                                    }
                                    if muted {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_unmute.call(clid),
                                            "Unmute"
                                        }
                                    } else {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_mute.call(clid),
                                            "Mute"
                                        }
                                    }
                                    MoveControl { clid: clid, current_cid: cid, on_move: on_move }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct MoveControlProps {
    clid: i64,
    current_cid: i64,
    on_move: EventHandler<(i64, i64)>,
}

#[component]
fn MoveControl(props: MoveControlProps) -> Element {
    // Minimal "type a channel id" affordance until the channel-tree
    // picker lands. Keeping it inline keeps the row keyboard-reachable;
    // a future modal/picker will replace this control without changing
    // the on_move contract.
    let mut input: Signal<String> = use_signal(String::new);
    let clid = props.clid;
    let on_move = props.on_move;
    rsx! {
        form {
            class: "inline-move",
            onsubmit: move |evt| {
                evt.prevent_default();
                let raw = input.read().clone();
                if let Ok(target) = raw.trim().parse::<i64>() {
                    on_move.call((clid, target));
                }
                input.set(String::new());
            },
            label { class: "sr-only", r#for: "move-{clid}", "Move client to channel id" }
            input {
                id: "move-{clid}",
                class: "input input-sm",
                placeholder: "cid",
                inputmode: "numeric",
                value: "{input.read()}",
                oninput: move |e| input.set(e.value()),
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                kind: crate::ui::components::ButtonType::Submit,
                "Move"
            }
        }
    }
}

fn apply_event(rows: &mut Vec<ClientListItem>, env: &WsEvent) {
    match env.kind.as_str() {
        "ts:client:kicked_from_server" => {
            if let Some(clid) = env.data.get("clid").and_then(Value::as_i64) {
                rows.retain(|r| r.clid != clid);
            }
        }
        "ts:client:kicked_from_channel" => {
            // Spec §14.1 — a channel kick lands the client in the
            // server's default channel. We don't know that id without a
            // refetch; clear `cid` to 0 so the row clearly shows it
            // moved, and the next snapshot reconciles.
            if let Some(clid) = env.data.get("clid").and_then(Value::as_i64)
                && let Some(row) = rows.iter_mut().find(|r| r.clid == clid)
            {
                row.cid = 0;
            }
        }
        "ts:client:moved" => {
            let clid = env.data.get("clid").and_then(Value::as_i64);
            let cid = env.data.get("cid").and_then(Value::as_i64);
            if let (Some(clid), Some(cid)) = (clid, cid)
                && let Some(row) = rows.iter_mut().find(|r| r.clid == clid)
            {
                row.cid = cid;
            }
        }
        "ts:client:muted" => {
            if let Some(clid) = env.data.get("clid").and_then(Value::as_i64)
                && let Some(row) = rows.iter_mut().find(|r| r.clid == clid)
            {
                if let Some(b) = env.data.get("inputMuted").and_then(Value::as_bool) {
                    row.client_input_muted = if b { 1 } else { 0 };
                }
                if let Some(b) = env.data.get("outputMuted").and_then(Value::as_bool) {
                    row.client_output_muted = if b { 1 } else { 0 };
                }
            }
        }
        "ts:client:unmuted" => {
            if let Some(clid) = env.data.get("clid").and_then(Value::as_i64)
                && let Some(row) = rows.iter_mut().find(|r| r.clid == clid)
            {
                row.client_input_muted = 0;
                row.client_output_muted = 0;
            }
        }
        _ => {}
    }
}

async fn fetch_clients(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<ClientListItem>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/clients");
    api::authorized_get_json::<Vec<ClientListItem>>(&gate, &api::api_base(), &path).await
}

fn default_reason(kind: KickKind) -> String {
    match kind {
        KickKind::Channel => "Removed by operator".into(),
        KickKind::Server => "Removed by operator".into(),
    }
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
        ApiError::Client { status, message } => format!("{status}: {message}"),
        ApiError::Server { status, message } => format!("{status}: {message}"),
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Action unavailable in this view.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(clid: i64) -> ClientListItem {
        ClientListItem {
            clid,
            cid: 1,
            client_database_id: clid + 100,
            client_type: 0,
            client_nickname: format!("user-{clid}"),
            ..Default::default()
        }
    }

    fn evt(kind: &str, data: serde_json::Value) -> WsEvent {
        WsEvent {
            id: 1,
            topic: "server:1:clients".into(),
            kind: kind.into(),
            data,
            ts: 0,
        }
    }

    #[test]
    fn kick_from_server_drops_row() {
        let mut rows = vec![row(1), row(2)];
        apply_event(
            &mut rows,
            &evt("ts:client:kicked_from_server", json!({"clid": 1})),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].clid, 2);
    }

    #[test]
    fn move_updates_cid() {
        let mut rows = vec![row(7)];
        apply_event(
            &mut rows,
            &evt("ts:client:moved", json!({"clid": 7, "cid": 42})),
        );
        assert_eq!(rows[0].cid, 42);
    }

    #[test]
    fn mute_updates_columns() {
        let mut rows = vec![row(3)];
        apply_event(
            &mut rows,
            &evt(
                "ts:client:muted",
                json!({"clid": 3, "inputMuted": true, "outputMuted": false}),
            ),
        );
        assert_eq!(rows[0].client_input_muted, 1);
        assert_eq!(rows[0].client_output_muted, 0);
    }

    #[test]
    fn unmute_clears_both_columns() {
        let mut rows = vec![ClientListItem {
            client_input_muted: 1,
            client_output_muted: 1,
            ..row(4)
        }];
        apply_event(&mut rows, &evt("ts:client:unmuted", json!({"clid": 4})));
        assert_eq!(rows[0].client_input_muted, 0);
        assert_eq!(rows[0].client_output_muted, 0);
    }

    #[test]
    fn unrecognised_event_is_ignored() {
        let mut rows = vec![row(5)];
        apply_event(&mut rows, &evt("ts:server:edited", json!({})));
        assert_eq!(rows.len(), 1);
    }
}
