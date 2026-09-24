//! `/clients` — operator client list with kick / talk-grant / move / poke
//! actions and live updates over `server:{id}:clients`. PURA-73.
//!
//! ## Data flow
//!
//! 1. On mount, `GET /api/servers/{configId}/vs/{sid}/clients` snapshots
//!    the live list. Spec §7.8.
//! 2. A WS subscription on `server:{configId}:clients` reduces over the
//!    snapshot — `ts:client:moved` updates the row's `cid`, kicks remove
//!    it, `ts:client:muted` / `ts:client:unmuted` flip `client_is_talker`
//!    (the moderated-channel talk grant those endpoints change). They do
//!    not carry mic or speaker mute. When the upstream emits
//!    a `ts:client:connected` we don't yet know the full row, so the
//!    component refetches the snapshot in the background and reconciles.
//!    The move-user picker also subscribes to `server:{configId}:channels`
//!    and refetches so create / rename / delete stay in the destination list.
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
use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem, KickKind, KickRequest};

use crate::client::api::{self, ApiError};
use crate::client::clients::{self, ClientMoveOutcome};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::ws::{WsEvent, use_ws_hub};
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

const PAGE_LEDE: &str = "Live clients on the selected server. Filter by nickname, unique ID, or channel. Kick, grant or revoke talk, or move without leaving the list.";

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
    // Move-user is `clientmove` (`check_write`: moderator or admin).
    // Channel ↑/↓ reorder is a different control and is not on this page.
    let can_move_clients = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin") || u.role.eq_ignore_ascii_case("moderator"))
        .unwrap_or(false);

    // Initial snapshot. Re-fires whenever the operator picks a different
    // server (the `server.id` capture is part of the future).
    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_clients(gate, server_id, sid).await }
        }
    });
    let mut channel_snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_channels(gate, server_id, sid).await }
        }
    });

    // Local working copy: snapshot + WS reductions. We hold this in a
    // signal so action handlers can mutate it optimistically.
    let mut rows: Signal<Vec<ClientListItem>> = use_signal(Vec::<ClientListItem>::new);
    let mut channels: Signal<Vec<ChannelTreeNode>> = use_signal(Vec::new);
    let mut last_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let filter: Signal<String> = use_signal(String::new);
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
    {
        use_effect(move || {
            if let Some(Ok(list)) = &*channel_snapshot.read_unchecked() {
                channels.set(list.clone());
            }
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

    // Channel create / rename / delete land on `server:{id}:channels`.
    // Without this, the move picker keeps the list from first load and a
    // later channel is missing (or a deleted one is still offered).
    {
        let hub = hub.clone();
        let _channels_live = use_resource(move || {
            let hub = hub.clone();
            let cur_server = *server_changed_marker.read();
            async move {
                if cur_server == 0 {
                    return;
                }
                let topic = format!("server:{cur_server}:channels");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(_env) = rx.next().await {
                    channel_snapshot.restart();
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

    let make_talk = {
        let gate = gate.clone();
        move |clid: i64, revoke: bool| {
            let gate = gate.clone();
            spawn(async move {
                // `revoke` posts `/mute`, which clears `client_is_talker`.
                // The other branch posts `/unmute`, which tries to set it.
                // Neither edits `client_input_muted` / `client_output_muted`.
                let segment = if revoke { "mute" } else { "unmute" };
                let path = format!("/api/servers/{server_id}/vs/{sid}/clients/{clid}/{segment}");
                match api::authorized_post_json::<_, ()>(
                    &gate,
                    &api::api_base(),
                    &path,
                    None::<&()>,
                )
                .await
                {
                    Ok(()) => {
                        let (title, detail) = talk_flag_success(revoke, clid);
                        toaster.push(ToastVariant::Success, title, Some(detail));
                    }
                    Err(e) => {
                        let (title, detail) = talk_flag_error(revoke, &e);
                        toaster.push(ToastVariant::Danger, title, Some(detail));
                    }
                }
            });
        }
    };

    let make_move = {
        let gate = gate.clone();
        move |clid: i64, target_cid: i64| {
            let gate = gate.clone();
            let current_cid = rows.read().iter().find(|r| r.clid == clid).map(|r| r.cid);
            let nick = rows
                .read()
                .iter()
                .find(|r| r.clid == clid)
                .map(|r| r.client_nickname.clone())
                .unwrap_or_else(|| format!("client {clid}"));
            let channel_list = channels.read().clone();
            let channel = super::client_move::channel_label(&channel_list, target_cid);
            if current_cid == Some(target_cid) {
                let (variant, title, detail) = super::client_move::client_move_toast(
                    &ClientMoveOutcome::AlreadyThere,
                    &nick,
                    &channel,
                );
                toaster.push(variant, title, detail);
                return;
            }
            spawn(async move {
                let outcome: ClientMoveOutcome =
                    clients::move_client(gate, server_id, sid, clid, target_cid)
                        .await
                        .into();
                if matches!(outcome, ClientMoveOutcome::Moved)
                    && let Some(row) = rows.write().iter_mut().find(|r| r.clid == clid)
                {
                    row.cid = target_cid;
                }
                let (variant, title, detail) =
                    super::client_move::client_move_toast(&outcome, &nick, &channel);
                toaster.push(variant, title, detail);
            });
        }
    };

    let (channels_loaded, channels_error) = match channel_snapshot.read().as_ref() {
        None => (false, None),
        Some(Ok(_)) => (true, None),
        Some(Err(e)) => (true, Some(format_error(e))),
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
                        let m = make_talk.clone();
                        EventHandler::new(move |clid: i64| m(clid, true))
                    },
                    on_unmute: {
                        let m = make_talk.clone();
                        EventHandler::new(move |clid: i64| m(clid, false))
                    },
                    on_move: {
                        let mv = make_move.clone();
                        EventHandler::new(move |args: (i64, i64)| mv(args.0, args.1))
                    },
                    channels: channels.read().clone(),
                    channels_loaded: channels_loaded,
                    channels_error: channels_error,
                    can_move: can_move_clients,
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
    channels: Vec<ChannelTreeNode>,
    channels_loaded: bool,
    channels_error: Option<String>,
    can_move: bool,
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
                        let voice = super::client_voice::ClientVoiceState::from_client(&r);
                        // Mute / Unmute posts the talker-flag endpoints.
                        // `client_is_talker == 0` is the revoked grant, not
                        // a mic or speaker mute (that is `voice` above).
                        let talker_revoked = r.client_is_talker == 0;
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
                                td {
                                    if props.channels.is_empty() {
                                        "{cid}"
                                    } else {
                                        "{super::client_move::channel_label(&props.channels, cid)}"
                                    }
                                }
                                td {
                                    span { class: "client-flags",
                                        if voice.is_muted() {
                                            super::client_voice::VoiceFlagTags { state: voice }
                                        } else {
                                            "Active"
                                        }
                                        if show_talker_grant(r.client_is_talker) {
                                            span {
                                                class: "client-flag",
                                                title: "Granted talker. Only changes who may speak in a moderated channel. Not a microphone mute.",
                                                "talker"
                                            }
                                        }
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
                                    if talker_revoked {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            title: Some(GRANT_TALK_TITLE.into()),
                                            onclick: move |_| on_unmute.call(clid),
                                            "{GRANT_TALK}"
                                        }
                                    } else {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            title: Some(REVOKE_TALK_TITLE.into()),
                                            onclick: move |_| on_mute.call(clid),
                                            "{REVOKE_TALK}"
                                        }
                                    }
                                    if props.can_move {
                                        MoveControl {
                                            clid: clid,
                                            current_cid: cid,
                                            channels: props.channels.clone(),
                                            channels_loaded: props.channels_loaded,
                                            channels_error: props.channels_error.clone(),
                                            on_move: on_move,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MovePickerPhase {
    Loading,
    Error,
    Unavailable,
    NoOther,
    Ready,
}

/// Failed fetches are an error, including when the list is empty. "No
/// other channel" is only the successful case where every remaining row
/// is the user's current channel (or a spacer).
fn move_picker_phase(
    loaded: bool,
    fetch_failed: bool,
    channels_empty: bool,
    targets_empty: bool,
) -> MovePickerPhase {
    if !loaded {
        MovePickerPhase::Loading
    } else if fetch_failed {
        MovePickerPhase::Error
    } else if channels_empty {
        MovePickerPhase::Unavailable
    } else if targets_empty {
        MovePickerPhase::NoOther
    } else {
        MovePickerPhase::Ready
    }
}

#[derive(Props, Clone, PartialEq)]
struct MoveControlProps {
    clid: i64,
    current_cid: i64,
    channels: Vec<ChannelTreeNode>,
    channels_loaded: bool,
    channels_error: Option<String>,
    on_move: EventHandler<(i64, i64)>,
}

#[component]
fn MoveControl(props: MoveControlProps) -> Element {
    // Pick a channel by name. This posts `clientmove`. It is not the
    // Channels ↑/↓ reorder control.
    let targets = super::client_move::move_target_options(&props.channels, props.current_cid);
    let mut selected: Signal<i64> = use_signal(|| 0i64);
    let clid = props.clid;
    let on_move = props.on_move;
    let phase = move_picker_phase(
        props.channels_loaded,
        props.channels_error.is_some(),
        props.channels.is_empty(),
        targets.is_empty(),
    );
    if phase == MovePickerPhase::Loading {
        return rsx! { span { class: "muted", "Loading channels…" } };
    }
    if let Some(err) = props.channels_error.as_ref() {
        return rsx! {
            span { class: "muted", role: "alert", "Could not load channels. {err}" }
        };
    }
    if phase == MovePickerPhase::Unavailable {
        return rsx! { span { class: "muted", "Channel list unavailable" } };
    }
    if phase == MovePickerPhase::NoOther {
        return rsx! { span { class: "muted", "No other channel" } };
    }
    rsx! {
        form {
            class: "inline-move",
            onsubmit: move |evt| {
                evt.prevent_default();
                let target = *selected.read();
                if target == 0 {
                    return;
                }
                on_move.call((clid, target));
                selected.set(0);
            },
            super::client_move::ChannelDestinationField {
                id: "move-{clid}".to_string(),
                targets: targets,
                selected: *selected.read(),
                on_change: EventHandler::new(move |cid: i64| selected.set(cid)),
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                kind: crate::ui::components::ButtonType::Submit,
                "Move user"
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
        // Payloads are `{clid, talker}` from the talker-flag endpoints.
        // They do not include `client_input_muted` / `client_output_muted`.
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

async fn fetch_channels(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
) -> Result<Vec<ChannelTreeNode>, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
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

const REVOKE_TALK: &str = "Revoke talk";
const GRANT_TALK: &str = "Grant talk";
const REVOKE_TALK_TITLE: &str = "Clear the talker flag. Only changes who may speak in a moderated channel. Does not mute the microphone or speakers. In an ordinary channel the client can still speak.";
const GRANT_TALK_TITLE: &str = "Set the talker flag. Only changes who may speak in a moderated channel. Does not mute the microphone or speakers. TeamSpeak rejects this in an ordinary channel (error 1538).";

/// TeamSpeak `1538 invalid parameter` when `client_is_talker=1` is sent
/// for a client who is not in a moderated channel.
const UNMODERATED_CHANNEL: i64 = 1538;

/// The grant is the meaningful state. `0` is the default for almost every
/// client and is not itself a mute.
fn show_talker_grant(client_is_talker: i64) -> bool {
    client_is_talker != 0
}

fn talk_flag_success(revoke: bool, clid: i64) -> (String, String) {
    if revoke {
        (
            format!("Revoked talk for client {clid}"),
            "Only affects a moderated channel. Microphone and speaker mute are unchanged.".into(),
        )
    } else {
        (
            format!("Granted talk for client {clid}"),
            "Only affects a moderated channel. TeamSpeak rejects granting talk in an ordinary channel (error 1538). Microphone and speaker mute are unchanged.".into(),
        )
    }
}

fn talk_flag_error(revoke: bool, err: &ApiError) -> (String, String) {
    if upstream_code(err) == Some(UNMODERATED_CHANNEL) {
        let title = if revoke {
            "Revoke talk refused"
        } else {
            "Grant talk refused"
        };
        (
            title.into(),
            "TeamSpeak error 1538 (invalid parameter). This likely means the channel isn't moderated, or it needs talk power.".into(),
        )
    } else {
        let title = if revoke {
            "Revoke talk failed"
        } else {
            "Grant talk failed"
        };
        (title.into(), format_error(err))
    }
}

fn upstream_code(err: &ApiError) -> Option<i64> {
    match err {
        ApiError::BadGateway { code, .. } => *code,
        _ => None,
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
    fn mute_clears_talker_flag_and_leaves_mic_flags() {
        let mut rows = vec![ClientListItem {
            client_is_talker: 1,
            client_input_muted: 0,
            client_output_muted: 0,
            client_input_hardware: 1,
            client_output_hardware: 1,
            ..row(3)
        }];
        apply_event(
            &mut rows,
            &evt("ts:client:muted", json!({"clid": 3, "talker": false})),
        );
        assert_eq!(rows[0].client_is_talker, 0);
        assert_eq!(rows[0].client_input_muted, 0);
        assert_eq!(rows[0].client_output_muted, 0);
        let voice = crate::ui::pages::client_voice::ClientVoiceState::from_client(&rows[0]);
        assert!(!voice.is_muted());
    }

    #[test]
    fn unmute_restores_talker_flag_and_leaves_mic_flags() {
        let mut rows = vec![ClientListItem {
            client_is_talker: 0,
            client_input_muted: 1,
            client_output_muted: 0,
            client_input_hardware: 1,
            client_output_hardware: 1,
            ..row(4)
        }];
        apply_event(
            &mut rows,
            &evt("ts:client:unmuted", json!({"clid": 4, "talker": true})),
        );
        assert_eq!(rows[0].client_is_talker, 1);
        assert_eq!(rows[0].client_input_muted, 1);
        let voice = crate::ui::pages::client_voice::ClientVoiceState::from_client(&rows[0]);
        assert!(voice.is_muted());
        assert_eq!(
            voice
                .tags()
                .into_iter()
                .map(|tag| tag.label)
                .collect::<Vec<_>>(),
            ["mic muted"]
        );
    }

    #[test]
    fn talker_chip_only_when_the_grant_is_set() {
        assert!(show_talker_grant(1));
        assert!(!show_talker_grant(0));
    }

    #[test]
    fn grant_and_revoke_toasts_do_not_say_muted() {
        let (revoke_title, revoke_detail) = talk_flag_success(true, 9);
        let (grant_title, grant_detail) = talk_flag_success(false, 9);
        assert_eq!(revoke_title, "Revoked talk for client 9");
        assert_eq!(grant_title, "Granted talk for client 9");
        for text in [revoke_title, revoke_detail, grant_title, grant_detail] {
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("muted"), "{text}");
            assert!(!lower.contains("unmute"), "{text}");
        }
    }

    #[test]
    fn error_1538_explains_an_unmoderated_channel() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1538),
            details: Some("invalid parameter".into()),
        };
        let (title, detail) = talk_flag_error(false, &err);
        assert_eq!(title, "Grant talk refused");
        assert_eq!(
            detail,
            "TeamSpeak error 1538 (invalid parameter). This likely means the channel isn't moderated, or it needs talk power."
        );
    }

    #[test]
    fn other_upstream_errors_stay_generic() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(2568),
            details: Some("insufficient client permissions".into()),
        };
        let (title, detail) = talk_flag_error(true, &err);
        assert_eq!(title, "Revoke talk failed");
        assert!(detail.contains("2568"));
        assert!(detail.contains("insufficient client permissions"));
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

    #[test]
    fn failed_channel_fetch_is_an_error_not_an_empty_picker() {
        assert_eq!(
            move_picker_phase(true, true, true, true),
            MovePickerPhase::Error
        );
        assert_eq!(
            move_picker_phase(true, true, false, false),
            MovePickerPhase::Error
        );
        assert_eq!(
            move_picker_phase(false, true, true, true),
            MovePickerPhase::Loading
        );
        assert_eq!(
            move_picker_phase(true, false, false, true),
            MovePickerPhase::NoOther
        );
        assert_eq!(
            move_picker_phase(true, false, true, true),
            MovePickerPhase::Unavailable
        );
        assert_eq!(
            move_picker_phase(true, false, false, false),
            MovePickerPhase::Ready
        );
    }
}
