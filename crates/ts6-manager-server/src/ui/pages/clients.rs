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
use ts6_manager_shared::control::{ClientListItem, KickKind, KickRequest, MoveRequest};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::{WsEvent, use_ws_hub};
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

const PAGE_LEDE: &str = "Live clients on the selected server. Filter by nickname, unique ID, or channel. Kick, mute, or move without leaving the list.";

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
            ClientsChrome { server_name: None, filter: None, match_count: None }
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
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut filter: Signal<String> = use_signal(String::new);
    let mut server_changed_marker: Signal<i64> = use_signal(|| 0i64);

    // When the snapshot resolves, write it into the working copy. The
    // marker bump tells dependent effects that the resource refilled —
    // necessary because `Resource::read()` doesn't itself trigger a
    // re-run of side-effecting code.
    {
        use_effect(move || {
            match &*snapshot.read_unchecked() {
                Some(Ok(list)) => {
                    rows.set(list.clone());
                    last_error.set(None);
                    loading.set(false);
                }
                Some(Err(e)) => {
                    last_error.set(Some(e.clone()));
                    loading.set(false);
                }
                None => loading.set(true),
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
                // `on=true` → mute (revoke talker flag); `on=false` → unmute.
                let segment = if on { "mute" } else { "unmute" };
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients/{clid}/{segment}");
                match api::authorized_post_json::<_, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    None::<&()>,
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

    let all_rows = rows.read().clone();
    let query = filter.read().clone();
    let visible = filter_clients(&all_rows, &query);
    let match_count = if query.trim().is_empty() {
        None
    } else {
        Some((visible.len(), all_rows.len()))
    };

    rsx! {
        ClientsChrome {
            server_name: Some(server_name),
            filter: Some(filter),
            match_count,
        }

        if let Some(err) = last_error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load clients".to_string(),
                p { "{format_error(err)}" }
                if let Some(hint) = err.transport_hint() {
                    p { class: "banner-hint", "{hint}" }
                }
            }
        }

        section { class: "stack-md",
            if *loading.read() && all_rows.is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading clients…" }
                }
            } else {
                ClientsTable {
                    rows: visible,
                    has_any_clients: !all_rows.is_empty(),
                    filter_active: !query.trim().is_empty(),
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
}

#[derive(Props, Clone, PartialEq)]
struct ClientsChromeProps {
    server_name: Option<String>,
    filter: Option<Signal<String>>,
    match_count: Option<(usize, usize)>,
}

#[component]
fn ClientsChrome(props: ClientsChromeProps) -> Element {
    let crumb = match props.server_name.as_deref() {
        Some(name) => format!("Clients · {name}"),
        None => "Clients".into(),
    };
    rsx! {
        div { class: "crumb", "{crumb}" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Clients" }
                p { class: "page-lede", "{PAGE_LEDE}" }
            }
            if let Some(mut filter) = props.filter {
                div { class: "page-actions",
                    label { class: "sr-only", r#for: "clients-filter", "Filter clients" }
                    input {
                        id: "clients-filter",
                        class: "input list-filter",
                        r#type: "search",
                        placeholder: "Filter by nickname, unique ID, or channel",
                        value: "{filter.read()}",
                        oninput: move |e| filter.set(e.value()),
                    }
                    if let Some((shown, total)) = props.match_count {
                        span { class: "list-filter-meta", role: "status", "aria-live": "polite",
                            "{shown} of {total}"
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ClientsTableProps {
    rows: Vec<ClientListItem>,
    has_any_clients: bool,
    filter_active: bool,
    on_kick_server: EventHandler<i64>,
    on_kick_channel: EventHandler<i64>,
    on_mute: EventHandler<i64>,
    on_unmute: EventHandler<i64>,
    on_move: EventHandler<(i64, i64)>,
}

#[component]
fn ClientsTable(props: ClientsTableProps) -> Element {
    if props.rows.is_empty() {
        return if props.filter_active && props.has_any_clients {
            rsx! {
                div { class: "empty",
                    div { class: "icon", "○" }
                    h3 { "No matches" }
                    p { "Try a different search term, or clear the filter." }
                }
            }
        } else {
            rsx! {
                div { class: "empty",
                    div { class: "icon", "◆" }
                    h3 { "No clients online" }
                    p { "When a client connects, they'll appear here." }
                }
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
                        // Silenced = operator revoked the talker flag.
                        // Effective in moderated channels only.
                        let muted = r.client_is_talker == 0;
                        let on_kick_server = props.on_kick_server;
                        let on_kick_channel = props.on_kick_channel;
                        let on_mute = props.on_mute;
                        let on_unmute = props.on_unmute;
                        let on_move = props.on_move;
                        rsx! {
                            tr { key: "{clid}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{r.client_nickname}" }
                                    UniqueIdAffordance { uid: r.client_unique_identifier.clone() }
                                }
                                td { "{cid}" }
                                td {
                                    if muted {
                                        span {
                                            title: "Silenced via talker flag — effective in moderated channels only",
                                            "Silenced"
                                        }
                                    } else {
                                        "Active"
                                    }
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
                row.client_is_talker = 0;
            }
        }
        "ts:client:unmuted" => {
            if let Some(clid) = env.data.get("clid").and_then(Value::as_i64)
                && let Some(row) = rows.iter_mut().find(|r| r.clid == clid)
            {
                row.client_is_talker = 1;
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

/// Client-side list filter — nickname, unique ID, session id, channel
/// id, or database id. Empty / whitespace query matches every row.
fn filter_clients(rows: &[ClientListItem], query: &str) -> Vec<ClientListItem> {
    let needle = query.trim();
    if needle.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|row| client_matches(row, needle))
        .cloned()
        .collect()
}

fn client_matches(row: &ClientListItem, needle: &str) -> bool {
    let needle_lc = needle.to_ascii_lowercase();
    row.client_nickname
        .to_ascii_lowercase()
        .contains(&needle_lc)
        || row
            .client_unique_identifier
            .to_ascii_lowercase()
            .contains(&needle_lc)
        || row.clid.to_string().contains(needle)
        || row.cid.to_string().contains(needle)
        || row.client_database_id.to_string().contains(needle)
}

/// Unique ID under the nickname: CSS-truncated, full value on hover,
/// click copies the complete identifier.
#[component]
fn UniqueIdAffordance(uid: String) -> Element {
    let copy_uid = uid.clone();
    rsx! {
        button {
            r#type: "button",
            class: "client-uid client-uid-copy",
            title: "{uid} — click to copy",
            "aria-label": "Copy unique ID {uid}",
            onclick: move |_| copy_to_clipboard(&copy_uid),
            "{uid}"
        }
    }
}

/// Best-effort copy of `text` to the system clipboard. No-op off the
/// browser (SSR / unit tests). Mirrors `ui::pages::widgets::copy_to_clipboard`.
fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
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
        ApiError::SessionAnonymous => "Loading…".into(),
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
            client_unique_identifier: format!("uid-hash-{clid}/ABCDEFGHIJKLMNOPQRSTUV=="),
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
    fn mute_clears_talker_flag() {
        let mut rows = vec![ClientListItem {
            client_is_talker: 1,
            ..row(3)
        }];
        apply_event(
            &mut rows,
            &evt("ts:client:muted", json!({"clid": 3, "talker": false})),
        );
        assert_eq!(rows[0].client_is_talker, 0);
    }

    #[test]
    fn unmute_restores_talker_flag() {
        let mut rows = vec![ClientListItem {
            client_is_talker: 0,
            ..row(4)
        }];
        apply_event(
            &mut rows,
            &evt("ts:client:unmuted", json!({"clid": 4, "talker": true})),
        );
        assert_eq!(rows[0].client_is_talker, 1);
    }

    #[test]
    fn unrecognised_event_is_ignored() {
        let mut rows = vec![row(5)];
        apply_event(&mut rows, &evt("ts:server:edited", json!({})));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn filter_empty_query_keeps_every_row() {
        let rows = vec![row(1), row(2)];
        assert_eq!(filter_clients(&rows, "").len(), 2);
        assert_eq!(filter_clients(&rows, "   ").len(), 2);
    }

    #[test]
    fn filter_matches_nickname_case_insensitively() {
        let rows = vec![row(1), row(2)];
        let hits = filter_clients(&rows, "USER-2");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].clid, 2);
    }

    #[test]
    fn filter_matches_unique_id_substring() {
        let rows = vec![row(1), row(2)];
        let hits = filter_clients(&rows, "uid-hash-1");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].clid, 1);
    }

    #[test]
    fn filter_matches_channel_and_session_ids() {
        let mut other = row(9);
        other.cid = 42;
        let rows = vec![row(1), other];
        assert_eq!(filter_clients(&rows, "42")[0].clid, 9);
        assert_eq!(filter_clients(&rows, "109")[0].clid, 9);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let rows = vec![row(1)];
        assert!(filter_clients(&rows, "zzzz-no-such-client").is_empty());
    }

    #[test]
    fn copy_to_clipboard_is_a_noop_on_native() {
        copy_to_clipboard("anything");
    }
}
