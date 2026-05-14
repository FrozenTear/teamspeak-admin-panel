//! `/video-sources` — operator surface for the MoQ video pipeline.
//!
//! Lists every `video_source` row scoped to the operator's currently-active
//! server (mirrors `clients.rs` / `bans.rs` server-scoping via the shared
//! [`active_server::resolve`]). The page is the FE-PAGES side of WS-7
//! (PURA-145): consumes WS-6's `/api/video-sources` REST surface and the
//! `server:{id}:video_sources` push topic.
//!
//! ## Data flow
//!
//! 1. On mount, `GET /api/video-sources` snapshots the visible list, then
//!    we filter to the active server.
//! 2. A WS subscription on `server:{configId}:video_sources` reduces the
//!    snapshot — `video_source:created` adds a row, `video_source:update`
//!    refreshes its status / frame counters, `video_source:deleted` drops it.
//! 3. The "Add source" modal POSTs to `/api/video-sources`. We don't push an
//!    optimistic row from the response — the WS-6 route emits a
//!    `video_source:created` envelope synchronously, so by the time POST
//!    returns the WS reduction has already landed the row.
//! 4. The "Stop" per-row button DELETEs and removes the row optimistically;
//!    the WS push reconciles if the manager backed out.
//! 5. The "Preview" per-row button mounts WS-5's [`VideoPlayer`] in a side
//!    drawer pointed at the sidecar's WebTransport endpoint. The relay URL
//!    defaults to `https://<page-hostname>:4443/anon` — the sidecar's local
//!    listener. A future surface can swap this when `MOQ_PUBLIC_URL` lands
//!    in a public-config endpoint.

#![allow(dead_code)] // server-side renders never exercise the hooks below.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use serde_json::Value;
use ts6_manager_shared::video_sources as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::client::video_sources as api;
use crate::client::ws::{WsEvent, use_ws_hub};
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant, VideoPlayer,
};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

#[component]
pub fn VideoSourcesPage() -> Element {
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
            div { class: "crumb", "Video sources" }
            h1 { "Video sources" }
            div { class: "empty",
                div { class: "icon", "▶" }
                h3 { "No server selected" }
                p { "Add a server to start streaming video into it." }
            }
        };
    };

    let server_id = server.id;
    let server_name = server.name.clone();

    // Local working copy: snapshot reduced by WS envelopes. We keep
    // rows keyed by id so reductions stay O(1).
    let mut rows: Signal<BTreeMap<i64, RowState>> = use_signal(BTreeMap::<i64, RowState>::new);
    let mut last_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload_marker: Signal<u64> = use_signal(|| 0u64);
    let mut show_create: Signal<bool> = use_signal(|| false);
    let mut preview_for: Signal<Option<PreviewTarget>> = use_signal(|| None::<PreviewTarget>);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload_marker.read();
            async move { api::list_sources(gate).await }
        }
    });

    // Reduce snapshot into the keyed map (filtered to the active server).
    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(list)) => {
            let mut map = BTreeMap::new();
            for v in list.iter().filter(|v| v.server_id == server_id) {
                map.insert(v.id, RowState::from_view(v.clone()));
            }
            rows.set(map);
            last_error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            last_error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    // WS subscription on the per-server topic. Reductions run inside the
    // page; the WS hub keeps the receiver wired across reconnects.
    {
        let hub = hub.clone();
        let _ = use_resource(move || {
            let hub = hub.clone();
            let cur = server_id;
            async move {
                if cur == 0 {
                    return;
                }
                let topic = format!("server:{cur}:video_sources");
                let mut handle = hub.subscribe(topic).await;
                let Some(mut rx) = handle.take_receiver() else {
                    return;
                };
                let _drop_guard = handle;
                use futures::stream::StreamExt;
                while let Some(env) = rx.next().await {
                    apply_event(&mut rows.write(), &env, cur);
                }
            }
        });
    }

    let on_stop = {
        let gate = gate.clone();
        let toaster = toaster;
        move |id: i64| {
            // Optimistic remove — WS push reconciles if the DELETE fails.
            let removed = rows.with_mut(|r| r.remove(&id));
            let gate = gate.clone();
            spawn(async move {
                match api::delete_source(gate, id).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, format!("Stopped source #{id}"), None);
                    }
                    Err(e) => {
                        if let Some(restore) = removed {
                            rows.with_mut(|r| {
                                r.insert(id, restore);
                            });
                        }
                        toaster.push(ToastVariant::Danger, "Stop failed", Some(format_error(&e)));
                    }
                }
            });
        }
    };

    let mut bump = move || reload_marker.with_mut(|n| *n += 1);

    // Polite live region — screen readers announce changes in the
    // (total, live) tuple as the WS reducer lands envelopes. Visually
    // hidden via the `sr-only` token; the text rebuilds on every
    // reduction so a transition from `starting → live` reaches a
    // non-sighted operator without depending on a row-level diff.
    let row_count = rows.read().len();
    let live_count = rows.read().values().filter(|r| r.status == "live").count();
    let live_summary = format!("{row_count} video source(s), {live_count} live.");

    rsx! {
        div { class: "crumb", "Video sources · {server_name}" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Video sources" }
                p { class: "page-lede",
                    "Stream a YouTube, RTMP, or HLS URL into the MoQ sidecar so it can be embedded as a widget. Status updates land live; the preview drawer renders the canvas player from any row."
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: move |_| show_create.set(true),
                    "+ Add source"
                }
            }
        }
        div {
            class: "sr-only",
            role: "status",
            "aria-live": "polite",
            "{live_summary}"
        }

        if let Some(err) = last_error.read().as_ref() {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load video sources".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading video sources…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", "▶" }
                    h3 { "No video sources yet" }
                    p { "Add a YouTube or RTMP URL above." }
                    div { class: "actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: move |_| show_create.set(true),
                            "+ Add source"
                        }
                    }
                }
            } else {
                SourcesTable {
                    rows: rows.read().values().cloned().collect::<Vec<RowState>>(),
                    on_preview: EventHandler::new({
                        move |row: RowState| {
                            preview_for.set(Some(PreviewTarget {
                                id: row.id,
                                label: row.label.clone(),
                                source_id: row.source_id.clone(),
                            }));
                        }
                    }),
                    on_stop: EventHandler::new({
                        let mut on_stop = on_stop.clone();
                        move |id: i64| on_stop(id)
                    }),
                }
            }
        }

        if *show_create.read() {
            CreateSourceModal {
                server_id: server_id,
                on_close: EventHandler::new(move |_: ()| show_create.set(false)),
                on_created: EventHandler::new({
                    let toaster = toaster;
                    move |v: wire::VideoSourceView| {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Started {}", v.label),
                            None,
                        );
                        show_create.set(false);
                        // The WS-6 route emits `video_source:created`
                        // synchronously, so the WS reduction will land
                        // the row. Bump the snapshot marker as a belt-
                        // and-braces refetch if the socket is offline.
                        bump();
                    }
                }),
            }
        }

        if let Some(target) = preview_for.read().clone() {
            PreviewDrawer {
                target: target,
                on_close: EventHandler::new(move |_: ()| preview_for.set(None)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Row state — view + latest stats
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RowState {
    pub id: i64,
    pub source_id: String,
    pub label: String,
    pub url: String,
    pub preset: String,
    pub status: String,
    pub video_frames: u64,
    pub audio_frames: u64,
    pub video_alive: bool,
    pub audio_alive: bool,
}

impl RowState {
    fn from_view(v: wire::VideoSourceView) -> Self {
        Self {
            id: v.id,
            source_id: v.source_id,
            label: v.label,
            url: v.url,
            preset: v.preset,
            status: v.status,
            video_frames: 0,
            audio_frames: 0,
            video_alive: false,
            audio_alive: false,
        }
    }

    fn apply_update(&mut self, u: &wire::VideoSourceUpdate) {
        self.status = u.status.clone();
        self.label = u.label.clone();
        self.preset = u.preset.clone();
        self.video_frames = u.video.frames_published;
        self.audio_frames = u.audio.frames_published;
        self.video_alive = u.video.ffmpeg_alive;
        self.audio_alive = u.audio.ffmpeg_alive;
    }
}

fn apply_event(rows: &mut BTreeMap<i64, RowState>, env: &WsEvent, expected_server: i64) {
    match env.kind.as_str() {
        "video_source:created" => {
            if let Ok(view) = serde_json::from_value::<wire::VideoSourceView>(env.data.clone()) {
                if view.server_id == expected_server {
                    rows.entry(view.id)
                        .and_modify(|r| {
                            // Keep current live stats on re-emit.
                            r.label = view.label.clone();
                            r.status = view.status.clone();
                            r.preset = view.preset.clone();
                            r.url = view.url.clone();
                            r.source_id = view.source_id.clone();
                        })
                        .or_insert_with(|| RowState::from_view(view));
                }
            }
        }
        "video_source:update" => {
            if let Ok(u) = serde_json::from_value::<wire::VideoSourceUpdate>(env.data.clone()) {
                if u.server_id == expected_server {
                    if let Some(row) = rows.get_mut(&u.id) {
                        row.apply_update(&u);
                    }
                }
            }
        }
        "video_source:deleted" => {
            // `data` is `{ id, source_id }`. Match on id when present;
            // fall back to source_id for forward compatibility.
            if let Some(id) = env.data.get("id").and_then(Value::as_i64) {
                rows.remove(&id);
            } else if let Some(sid) = env.data.get("source_id").and_then(Value::as_str) {
                let key = rows
                    .iter()
                    .find(|(_, r)| r.source_id == sid)
                    .map(|(k, _)| *k);
                if let Some(k) = key {
                    rows.remove(&k);
                }
            }
        }
        _ => {
            // Unknown envelope kinds are ignored — the WS hub may add new
            // ones over time without breaking this surface.
        }
    }
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct SourcesTableProps {
    rows: Vec<RowState>,
    on_preview: EventHandler<RowState>,
    on_stop: EventHandler<i64>,
}

#[component]
fn SourcesTable(props: SourcesTableProps) -> Element {
    rsx! {
        table { class: "data-table", "aria-label": "Video sources",
            thead {
                tr {
                    th { scope: "col", "Label" }
                    th { scope: "col", "URL" }
                    th { scope: "col", "Preset" }
                    th { scope: "col", "Status" }
                    th { scope: "col", "Frames" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for r in props.rows.iter() {
                    {
                        let row = r.clone();
                        let id = row.id;
                        let badge = status_badge(&row.status);
                        let on_preview = props.on_preview;
                        let on_stop = props.on_stop;
                        let row_for_preview = row.clone();
                        rsx! {
                            tr { key: "{id}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{row.label}" }
                                    span { class: "client-uid", "{row.source_id}" }
                                }
                                td {
                                    span { class: "muted",
                                        title: "{row.url}",
                                        "{truncate_url(&row.url)}"
                                    }
                                }
                                td { "{row.preset}" }
                                td {
                                    span { class: "{badge.class}", "{badge.label}" }
                                    if row.video_alive {
                                        span { class: "muted", " · video ✓" }
                                    }
                                    if row.audio_alive {
                                        span { class: "muted", " · audio ✓" }
                                    }
                                }
                                td {
                                    "v {row.video_frames} · a {row.audio_frames}"
                                }
                                td { class: "actions-col",
                                    Button {
                                        variant: ButtonVariant::Secondary,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_preview.call(row_for_preview.clone()),
                                        "Preview"
                                    }
                                    Button {
                                        variant: ButtonVariant::Danger,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_stop.call(id),
                                        "Stop"
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

struct Badge {
    label: &'static str,
    class: &'static str,
}

fn status_badge(status: &str) -> Badge {
    // Reuse the music-bot badge token vocabulary so the design language
    // stays coherent across operator surfaces (PURA-124's `bot-badge--*`).
    match status {
        "live" => Badge {
            label: "Live",
            class: "bot-badge bot-badge--play",
        },
        "starting" => Badge {
            label: "Starting…",
            class: "bot-badge bot-badge--pending",
        },
        "failed" => Badge {
            label: "Failed",
            class: "bot-badge bot-badge--off",
        },
        _ => Badge {
            label: "Unknown",
            class: "bot-badge bot-badge--off",
        },
    }
}

fn truncate_url(url: &str) -> String {
    const MAX: usize = 56;
    if url.chars().count() <= MAX {
        url.to_string()
    } else {
        let mut s: String = url.chars().take(MAX).collect();
        s.push('…');
        s
    }
}

// ---------------------------------------------------------------------------
// Create modal
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct CreateSourceModalProps {
    server_id: i64,
    on_close: EventHandler<()>,
    on_created: EventHandler<wire::VideoSourceView>,
}

#[component]
fn CreateSourceModal(props: CreateSourceModalProps) -> Element {
    let gate = use_auth_gate();
    let mut url: Signal<String> = use_signal(String::new);
    let mut label: Signal<String> = use_signal(String::new);
    let mut preset: Signal<String> = use_signal(|| "720p".to_string());
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let server_id = props.server_id;
    let on_close = props.on_close;
    let on_created = props.on_created;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed_url = url.read().trim().to_string();
        let trimmed_label = label.read().trim().to_string();
        if trimmed_url.is_empty() {
            error.set(Some("URL is required.".into()));
            return;
        }
        if trimmed_label.is_empty() {
            error.set(Some("Label is required.".into()));
            return;
        }
        let chosen_preset = preset.read().clone();
        submitting.set(true);
        error.set(None);
        let body = wire::CreateVideoSourceRequest {
            url: trimmed_url,
            label: trimmed_label,
            preset: Some(chosen_preset),
            server_id: Some(server_id),
        };
        let gate = gate.clone();
        spawn(async move {
            match api::create_source(gate, &body).await {
                Ok(v) => {
                    submitting.set(false);
                    on_created.call(v);
                }
                Err(e) => {
                    submitting.set(false);
                    error.set(Some(format_error(&e)));
                }
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "create-video-source-title",
                div { class: "modal-header",
                    h2 { id: "create-video-source-title", "Add video source" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not start source".to_string(),
                            "{msg}"
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "URL" }
                        input {
                            class: "input",
                            value: "{url.read()}",
                            placeholder: "https://www.youtube.com/watch?v=… or rtmp://…",
                            oninput: move |e| url.set(e.value()),
                            required: true,
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Label" }
                        input {
                            class: "input",
                            value: "{label.read()}",
                            placeholder: "Lobby camera",
                            oninput: move |e| label.set(e.value()),
                            required: true,
                            maxlength: "256",
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Preset" }
                        select {
                            class: "input",
                            value: "{preset.read()}",
                            onchange: move |e| preset.set(e.value()),
                            for p in wire::KNOWN_PRESETS.iter() {
                                option { value: "{p}", "{p}" }
                            }
                        }
                    }
                }
                div { class: "modal-actions",
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        "Start streaming"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Preview drawer — mounts the WS-5 VideoPlayer
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq)]
struct PreviewTarget {
    id: i64,
    label: String,
    source_id: String,
}

#[derive(Props, Clone, PartialEq)]
struct PreviewDrawerProps {
    target: PreviewTarget,
    on_close: EventHandler<()>,
}

#[component]
fn PreviewDrawer(props: PreviewDrawerProps) -> Element {
    let on_close = props.on_close;
    let label = props.target.label.clone();
    let source_id = props.target.source_id.clone();
    let relay_url = default_relay_url();
    rsx! {
        div { class: "drawer-backdrop", onclick: move |_| on_close.call(()),
            div {
                class: "drawer",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "video-preview-title",
                "aria-busy": "true",
                div { class: "drawer-header",
                    h2 { id: "video-preview-title", "Preview · {label}" }
                    button {
                        r#type: "button",
                        class: "btn btn-ghost btn-sm",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "✕"
                    }
                }
                div { class: "drawer-body stack-md",
                    p { class: "muted",
                        "Relay: ", code { "{relay_url}" }, " · namespace: ", code { "{source_id}" }
                    }
                    VideoPlayer {
                        relay_url: relay_url,
                        namespace: source_id,
                        autoplay: true,
                    }
                }
                div { class: "drawer-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        onclick: move |_| on_close.call(()),
                        "Close"
                    }
                }
            }
        }
    }
}

/// Default WebTransport relay URL for the operator preview. Uses the
/// current page hostname with the sidecar's default WT port (4443) and the
/// `anon` namespace path (matches `docs/ts6-fixture.md`). When a public
/// config endpoint surfaces `MOQ_PUBLIC_URL` this is the one place to
/// swap.
fn default_relay_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(w) = web_sys::window() {
            if let Ok(host) = w.location().hostname() {
                if !host.is_empty() {
                    return format!("https://{host}:4443/anon");
                }
            }
        }
    }
    "https://127.0.0.1:4443/anon".to_string()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

    fn env(kind: &str, data: Value) -> WsEvent {
        WsEvent {
            id: 1,
            topic: "server:1:video_sources".into(),
            kind: kind.into(),
            data,
            ts: 0,
        }
    }

    #[test]
    fn created_envelope_inserts_row_for_matching_server() {
        let mut rows = BTreeMap::new();
        let payload = json!({
            "id": 7,
            "source_id": "src-7",
            "label": "Lobby cam",
            "url": "https://example.com/x.m3u8",
            "preset": "720p",
            "server_id": 1,
            "status": "starting",
            "track": {"namespace": "src-7", "video": "video", "audio": "audio"},
            "created_by_user_id": null,
            "created_at": "2026-05-14T00:00:00Z"
        });
        apply_event(&mut rows, &env("video_source:created", payload), 1);
        let row = rows.get(&7).expect("row inserted");
        assert_eq!(row.source_id, "src-7");
        assert_eq!(row.status, "starting");
    }

    #[test]
    fn created_envelope_ignored_for_other_server() {
        let mut rows = BTreeMap::new();
        let payload = json!({
            "id": 7, "source_id": "src-7", "label": "x", "url": "u",
            "preset": "720p", "server_id": 99, "status": "starting",
            "track": {"namespace": "src-7", "video": "video", "audio": "audio"},
            "created_by_user_id": null,
            "created_at": "2026-05-14T00:00:00Z"
        });
        apply_event(&mut rows, &env("video_source:created", payload), 1);
        assert!(rows.is_empty(), "row from wrong server must be dropped");
    }

    #[test]
    fn update_refreshes_status_and_counters() {
        let mut rows = BTreeMap::new();
        rows.insert(
            7,
            RowState {
                id: 7,
                source_id: "src-7".into(),
                label: "old".into(),
                url: "u".into(),
                preset: "720p".into(),
                status: "starting".into(),
                video_frames: 0,
                audio_frames: 0,
                video_alive: false,
                audio_alive: false,
            },
        );
        let payload = json!({
            "id": 7, "source_id": "src-7", "label": "Cam A", "preset": "1080p",
            "server_id": 1, "status": "live",
            "video": {"frames_published": 100, "bytes_published": 0, "ffmpeg_alive": true},
            "audio": {"frames_published": 50, "bytes_published": 0, "ffmpeg_alive": true}
        });
        apply_event(&mut rows, &env("video_source:update", payload), 1);
        let row = rows.get(&7).expect("row present");
        assert_eq!(row.status, "live");
        assert_eq!(row.label, "Cam A");
        assert_eq!(row.preset, "1080p");
        assert_eq!(row.video_frames, 100);
        assert_eq!(row.audio_frames, 50);
        assert!(row.video_alive);
    }

    #[test]
    fn deleted_envelope_removes_row_by_id() {
        let mut rows = BTreeMap::new();
        rows.insert(
            7,
            RowState {
                id: 7,
                source_id: "src-7".into(),
                label: "x".into(),
                url: "u".into(),
                preset: "720p".into(),
                status: "live".into(),
                video_frames: 0,
                audio_frames: 0,
                video_alive: false,
                audio_alive: false,
            },
        );
        apply_event(
            &mut rows,
            &env(
                "video_source:deleted",
                json!({"id": 7, "source_id": "src-7"}),
            ),
            1,
        );
        assert!(rows.is_empty(), "row should be gone after delete");
    }

    #[test]
    fn unknown_kind_is_noop() {
        let mut rows = BTreeMap::new();
        apply_event(&mut rows, &env("video_source:future", json!({})), 1);
        assert!(rows.is_empty());
    }

    #[test]
    fn truncate_url_keeps_short_urls_as_is() {
        assert_eq!(truncate_url("https://x.test/a"), "https://x.test/a");
    }

    #[test]
    fn truncate_url_clips_long_urls_with_ellipsis() {
        let long: String = "a".repeat(100);
        let out = truncate_url(&long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 57);
    }

    #[test]
    fn status_badge_maps_known_states() {
        assert_eq!(status_badge("live").label, "Live");
        assert_eq!(status_badge("starting").label, "Starting…");
        assert_eq!(status_badge("failed").label, "Failed");
        assert_eq!(status_badge("???").label, "Unknown");
    }
}
