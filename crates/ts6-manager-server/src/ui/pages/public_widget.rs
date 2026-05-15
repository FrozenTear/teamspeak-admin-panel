//! `/widget/:token` — public, anonymous widget render page (PURA-72 Slice E).
//!
//! Spec §28.1 line 4002 + 4025: the public widget HTML page is unauthenticated
//! and renders `WidgetData` client-side. Unlike every other route, this page
//! is **outside** `AppShell` — no sidebar, no header chrome, no auth gate. It
//! is intended to be embedded in a third-party `<iframe>` (the iframe-friendly
//! response headers are owned by Slice F / PURA-72-F).
//!
//! ## Wire flow
//!
//! 1. On mount, fetch `GET /api/widget/{token}/data` (Slice A — PURA-72-A).
//!    Empty-state if 404 (unknown / revoked token), error banner on transport
//!    or 5xx.
//! 2. Apply the palette resolved from [`WidgetThemeName::palette`] as inline
//!    `<style>` so the page is self-contained — no shared design-system CSS
//!    is loaded outside `AppShell`, and the embed page is meant to live in
//!    third-party DOM where global tokens are unsafe.
//! 3. Open one [`gloo_net::websocket::futures::WebSocket`] to
//!    `/api/ws?token={token}`. The hub authenticates the widget token
//!    (PURA-70) and authorises subscription only to
//!    `server:{serverConfigId}:widget` (auth.rs `WidgetPrincipal` →
//!    `AuthRequirement::WidgetToken`).
//! 4. On every inbound envelope, refetch the data. (The `notify*` payloads
//!    are rich enough to apply per-event in principle, but the spec calls
//!    for "rebuild the rendered tree on each push" and the JSON snapshot is
//!    cheap — `Cache-Control: max-age=45` already gates upstream load.)
//! 5. Reconnect on close with exponential backoff (`INITIAL_BACKOFF_MS`
//!    doubling, capped at `MAX_BACKOFF_MS`, ±25% jitter). Each successful
//!    open resets the back-off. `lastEventId` is forwarded across reconnects
//!    so the ring-buffer replay path in the hub fills the gap with the
//!    envelopes the page missed.
//!
//! ## Why a separate WS subscriber (not [`crate::client::ws::WsHub`])
//!
//! The operator hub is keyed on the JWT lifecycle and `use_ws_lifecycle`
//! re-arms on session changes. The public page has no session — its
//! credential is the URL token — so it owns its own minimal subscriber.
//! Keeping the two surfaces separate also means a redirect-bounce on JWT
//! expiry can never affect a widget that lives in a third-party iframe.

#![allow(dead_code)] // `WsState` variants are observed via the banner.

use dioxus::prelude::*;
use serde::Deserialize;
use ts6_manager_shared::widgets::{
    SpacerType, WidgetChannelNode, WidgetData, WidgetThemeName, WidgetThemePalette,
};

use crate::client::api::ApiError;
use crate::ui::components::VideoPlayer;

// ── PURA-146 (WS-8) — public video-source view.
//
// Mirrors the wire shape returned by `widgets::routes::video_sources_handler`.
// Defined here (rather than in `ts6-manager-shared`) because the public
// widget page is the only consumer in the SPA — the operator-facing
// `/video-sources` route owns its own richer DTO in
// `routes::control::video_sources`.

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PublicVideoSourcesPayload {
    #[serde(default)]
    relay_url: Option<String>,
    #[serde(default)]
    sources: Vec<PublicVideoSource>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PublicVideoSource {
    source_id: String,
    label: String,
    #[serde(default)]
    status: String,
    track: PublicTrack,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PublicTrack {
    namespace: String,
}

impl PublicVideoSource {
    /// A source is mountable when the sidecar reports an active pipeline
    /// (`starting` or `live`). `failed` / `stopped` rows are listed but
    /// not auto-selected — the operator restarting the pipeline will move
    /// them back to `live` and the next refetch picks them up.
    fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "live")
    }
}

/// Initial reconnect back-off. Mirrors the operator hub
/// (`crate::client::ws::INITIAL_BACKOFF_MS`) so the public surface follows
/// the same recovery cadence.
const INITIAL_BACKOFF_MS: u32 = 250;
const MAX_BACKOFF_MS: u32 = 8_000;

/// Connection state surfaced to the in-page banner. The widget never blocks
/// on the WS — the rendered snapshot stays visible during reconnects so an
/// embed never shows a blank frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsState {
    Connecting,
    Connected,
    Disconnected,
    /// Token rejected by the hub. The page shows the not-found state and
    /// stops trying to reconnect — re-trying with a revoked token would just
    /// hammer the upstream rate-limiter.
    Unauthorized,
}

#[component]
pub fn PublicWidgetPage(token: String) -> Element {
    let token_signal = use_signal(|| token.clone());

    // Refetch trigger. Bumping it forces the resource to re-run; the WS
    // listener bumps it on every push, and the explicit "Retry" button in
    // the error state bumps it too.
    let mut refresh_token: Signal<u64> = use_signal(|| 0u64);

    let ws_state: Signal<WsState> = use_signal(|| WsState::Connecting);

    let data = {
        let token = token.clone();
        use_resource(move || {
            let _bump = *refresh_token.read();
            let token = token.clone();
            async move { fetch_widget_data(&token).await }
        })
    };

    // PURA-146 (WS-8) — public video-sources fetch. Refetches on every
    // WS push that bumps `refresh_token`, so newly-started sources
    // appear in the tab strip without a manual reload. A failure here is
    // intentionally non-fatal: the channel tree must still render even
    // when the sidecar is offline.
    let video_sources = {
        let token = token.clone();
        use_resource(move || {
            let _bump = *refresh_token.read();
            let token = token.clone();
            async move { fetch_public_video_sources(&token).await }
        })
    };

    // Spawn the WS subscriber once the first JSON snapshot arrives (we need
    // its `serverConfigId` to derive the topic). The subscriber drives
    // refetches by bumping `refresh_token` whenever the hub publishes.
    let server_config_id: Option<i64> = match &*data.read_unchecked() {
        Some(Ok(d)) => Some(d.server_config_id),
        _ => None,
    };

    {
        let token = token.clone();
        use_effect(move || {
            if let Some(server_id) = server_config_id {
                spawn_ws_listener(token.clone(), server_id, refresh_token, ws_state);
            }
        });
    }

    let _ = token_signal; // silence unused warning on native targets

    // Snapshot the latest video payload (or None while loading / on
    // error). The surface treats `None` and `Some(empty sources)`
    // identically — both render "No live video".
    let video_payload: Option<PublicVideoSourcesPayload> = match &*video_sources.read_unchecked() {
        Some(Ok(p)) => Some(p.clone()),
        _ => None,
    };

    rsx! {
        WidgetStyleBlock {}
        match &*data.read_unchecked() {
            None => rsx! { WidgetLoading {} },
            Some(Ok(data)) => rsx! {
                WidgetSurface {
                    data: data.clone(),
                    ws_state: *ws_state.read(),
                    video: video_payload.clone(),
                    on_retry: EventHandler::new(move |_| {
                        let v = *refresh_token.read();
                        refresh_token.set(v.wrapping_add(1));
                    }),
                }
            },
            Some(Err(err)) => rsx! {
                WidgetError {
                    err: err.clone(),
                    on_retry: EventHandler::new(move |_| {
                        let v = *refresh_token.read();
                        refresh_token.set(v.wrapping_add(1));
                    }),
                }
            },
        }
    }
}

#[component]
fn WidgetStyleBlock() -> Element {
    // Inline minimal CSS so the public page is self-contained even when the
    // SPA's global stylesheets aren't loaded (third-party iframe contexts
    // sometimes strip them). Per-theme palette values are applied via the
    // `style` attribute on the root container, not here.
    rsx! {
        style {
            r#"
            .pw-root {{
                font-family: 'Segoe UI', 'Helvetica Neue', Arial, sans-serif;
                color: var(--pw-text-primary);
                background: var(--pw-background);
                border: 1px solid var(--pw-border);
                border-radius: 10px;
                width: 400px;
                max-width: 100%;
                box-sizing: border-box;
                overflow: hidden;
            }}
            /* WS-8 — wider footprint when the video pane is visible. The
               16:9 canvas + tab strip needs ~440 px to feel comfortable;
               the tree pane sits beside it at the original 400 px wide. */
            .pw-root--with-video {{
                width: 880px;
            }}
            .pw-panes {{
                display: flex;
                flex-direction: row;
                align-items: stretch;
                gap: 0;
            }}
            .pw-root--with-video .pw-tree-pane {{
                flex: 0 0 400px;
                min-width: 0;
                border-left: 1px solid var(--pw-border);
            }}
            .pw-root:not(.pw-root--with-video) .pw-tree-pane {{
                flex: 1 1 auto;
            }}
            /* Mobile / narrow viewports — stack panes vertically so the
               canvas isn't squashed. Matches the 390 px iPhone-13 width
               called out in the FE-PAGES QA viewport set. */
            @media (max-width: 720px) {{
                .pw-root--with-video {{ width: 400px; }}
                .pw-panes {{ flex-direction: column; }}
                .pw-root--with-video .pw-tree-pane {{
                    flex: 1 1 auto;
                    border-left: none;
                    border-top: 1px solid var(--pw-border);
                }}
            }}
            .pw-video-pane {{
                flex: 1 1 auto;
                min-width: 0;
                background: #000;
                display: flex;
                flex-direction: column;
            }}
            .pw-video-tabs {{
                display: flex;
                flex-wrap: wrap;
                gap: 4px;
                padding: 6px 8px;
                background: var(--pw-header-bg);
                border-bottom: 1px solid var(--pw-border);
            }}
            .pw-video-tab {{
                font-size: 11px;
                font-weight: 600;
                padding: 4px 10px;
                border-radius: 12px;
                border: 1px solid var(--pw-border);
                background: var(--pw-background-secondary);
                color: var(--pw-text-primary);
                cursor: pointer;
            }}
            .pw-video-tab.is-active {{
                background: var(--pw-accent);
                color: #fff;
                border-color: var(--pw-accent);
            }}
            .pw-video-tab:disabled {{
                opacity: 0.55;
                cursor: not-allowed;
            }}
            .pw-video-tab-dead {{
                color: var(--pw-text-secondary);
                font-weight: 400;
            }}
            .pw-video-stage {{
                position: relative;
                flex: 1 1 auto;
                background: #000;
                min-height: 240px;
                display: flex;
                align-items: center;
                justify-content: center;
            }}
            .pw-video-empty {{
                color: var(--pw-text-secondary);
                font-size: 12px;
                padding: 24px;
                text-align: center;
            }}
            .pw-video-stage .video-player {{
                width: 100%;
            }}
            .pw-video-unmute {{
                position: absolute;
                inset: 0;
                margin: auto;
                width: max-content;
                height: max-content;
                display: flex;
                align-items: center;
                gap: 8px;
                padding: 10px 16px;
                border-radius: 999px;
                border: 1px solid rgba(255, 255, 255, 0.4);
                background: rgba(0, 0, 0, 0.55);
                color: #fff;
                font-size: 13px;
                font-weight: 600;
                cursor: pointer;
            }}
            .pw-video-unmute:hover {{
                background: rgba(0, 0, 0, 0.7);
            }}
            .pw-video-unmute-icon {{
                font-size: 16px;
            }}
            .pw-header {{
                background: var(--pw-header-bg);
                padding: 14px;
                border-bottom: 1px solid var(--pw-border);
            }}
            .pw-header-row {{
                display: flex;
                align-items: center;
                justify-content: space-between;
                gap: 12px;
            }}
            .pw-server-name {{
                font-weight: 700;
                font-size: 15px;
                color: var(--pw-accent);
                margin: 0;
                overflow: hidden;
                text-overflow: ellipsis;
                white-space: nowrap;
            }}
            .pw-online-badge {{
                background: var(--pw-client-color);
                color: #fff;
                font-size: 10px;
                font-weight: 700;
                letter-spacing: 0.5px;
                padding: 2px 6px;
                border-radius: 3px;
                white-space: nowrap;
            }}
            .pw-stats {{
                font-size: 11px;
                color: var(--pw-text-secondary);
                margin-top: 4px;
            }}
            .pw-tree {{
                padding: 14px;
                background: var(--pw-background);
                font-size: 12px;
            }}
            .pw-tree ul {{
                list-style: none;
                margin: 0;
                padding: 0;
            }}
            .pw-channel {{
                display: flex;
                align-items: center;
                gap: 4px;
                padding: 3px 0;
                line-height: 22px;
                color: var(--pw-text-primary);
            }}
            .pw-channel-icon {{
                color: var(--pw-accent);
                font-weight: 700;
            }}
            .pw-channel-name {{
                flex: 1;
                overflow: hidden;
                text-overflow: ellipsis;
                white-space: nowrap;
            }}
            .pw-channel-count {{
                color: var(--pw-text-secondary);
                font-size: 10px;
            }}
            .pw-channel-lock {{
                color: var(--pw-text-secondary);
            }}
            .pw-client {{
                display: flex;
                align-items: center;
                gap: 6px;
                line-height: 18px;
                font-size: 11px;
                color: var(--pw-client-color);
            }}
            .pw-client-dot {{
                width: 6px;
                height: 6px;
                border-radius: 50%;
                background: var(--pw-client-color);
                display: inline-block;
            }}
            .pw-client-flag {{
                color: var(--pw-text-secondary);
                font-size: 10px;
                margin-left: 4px;
            }}
            .pw-spacer {{
                color: var(--pw-text-secondary);
                font-size: 11px;
                font-weight: 600;
                letter-spacing: 0.5px;
                line-height: 22px;
                padding: 2px 0;
            }}
            .pw-spacer-line {{
                height: 0;
                border-top: 1px solid var(--pw-border);
                margin: 9px 0;
            }}
            .pw-spacer-line.dotted {{ border-top-style: dotted; }}
            .pw-spacer-line.dashed {{ border-top-style: dashed; }}
            .pw-spacer.center {{ text-align: center; }}
            .pw-spacer.right  {{ text-align: right; }}
            .pw-footer {{
                font-size: 9px;
                opacity: 0.6;
                color: var(--pw-text-secondary);
                border-top: 1px solid var(--pw-border);
                padding: 8px 14px;
                text-align: center;
            }}
            .pw-banner {{
                font-size: 11px;
                color: var(--pw-text-secondary);
                background: var(--pw-background-secondary);
                padding: 6px 14px;
                border-bottom: 1px solid var(--pw-border);
            }}
            .pw-empty {{
                padding: 32px 14px;
                text-align: center;
                color: var(--pw-text-secondary);
                font-size: 13px;
            }}
            .pw-error {{
                padding: 24px 14px;
                text-align: center;
                color: var(--pw-text-secondary);
                font-size: 13px;
            }}
            .pw-error button {{
                margin-top: 10px;
                font-size: 12px;
                padding: 4px 12px;
                border-radius: 4px;
                border: 1px solid var(--pw-border);
                background: var(--pw-background-secondary);
                color: var(--pw-text-primary);
                cursor: pointer;
            }}
            "#
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetSurfaceProps {
    data: WidgetData,
    ws_state: WsState,
    /// PURA-146 (WS-8) — live video sources for the widget's server.
    /// `None` while the public REST surface is still loading; the panel
    /// renders "No live video" on either `None` or an empty sources
    /// vector once `relay_url` is missing.
    #[props(default)]
    video: Option<PublicVideoSourcesPayload>,
    on_retry: EventHandler<()>,
}

#[component]
fn WidgetSurface(props: WidgetSurfaceProps) -> Element {
    let theme = WidgetThemeName::parse_or_default(&props.data.theme).palette();
    let style = palette_css_vars(&theme);
    // The root expands to `pw-root--with-video` when we have any source
    // payload to render — the wider layout makes room for the 16:9 canvas
    // beside the channel tree. The CSS media query in `WidgetStyleBlock`
    // collapses it back to a column on mobile viewports.
    let has_any_video = matches!(&props.video, Some(p) if !p.sources.is_empty());
    let root_class = if has_any_video {
        "pw-root pw-root--with-video"
    } else {
        "pw-root"
    };
    rsx! {
        div { class: "{root_class}", style: "{style}",
            WidgetHeader { server: props.data.server.clone() }
            // Reconnect banner — non-blocking, hidden on `Connected` per spec
            // §28.4 ("hide on recovery"). On `Unauthorized` we render the
            // empty/not-found state instead so the surface mirrors what the
            // server sent the user.
            match props.ws_state {
                WsState::Connected | WsState::Connecting => rsx! { "" },
                WsState::Disconnected => rsx! {
                    div { class: "pw-banner",
                        role: "status",
                        "aria-live": "polite",
                        "Reconnecting…"
                    }
                },
                WsState::Unauthorized => rsx! {
                    div { class: "pw-banner",
                        role: "status",
                        "aria-live": "polite",
                        "Live updates unavailable."
                    }
                },
            }
            div { class: "pw-panes",
                // Video pane is omitted entirely when there are zero sources
                // AND no payload at all — keeps the original PURA-72 layout
                // intact when video is unconfigured.
                if has_any_video {
                    {
                        let payload = props.video.clone().unwrap_or_else(|| PublicVideoSourcesPayload {
                            relay_url: None,
                            sources: Vec::new(),
                        });
                        rsx! { WidgetVideoPane { payload: payload } }
                    }
                }
                section { class: "pw-tree-pane",
                    WidgetTree {
                        channels: props.data.channels.clone(),
                        show_clients: props.data.show_clients,
                        show_channel_tree: props.data.show_channel_tree,
                    }
                }
            }
            div { class: "pw-footer", "TS6 WebUI Widget" }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetVideoPaneProps {
    payload: PublicVideoSourcesPayload,
}

/// PURA-146 (WS-8) — video pane beside the channel tree. Renders the
/// most-recently-added active source by default; a tab strip lets the
/// viewer switch between sources when more than one is streaming.
/// Audio is muted until the viewer taps the unmute overlay (browser
/// autoplay policy on third-party embeds).
#[component]
fn WidgetVideoPane(props: WidgetVideoPaneProps) -> Element {
    let payload = props.payload.clone();
    let sources = payload.sources.clone();
    let relay_url = payload.relay_url.clone();

    // Prefer the most-recently-added active source. The REST surface
    // returns rows sorted by `id ASC` (see
    // `repos::video_sources::list_for_server`), so the last active row
    // is the most recent.
    let default_index: usize = sources
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.is_active())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut selected: Signal<usize> = use_signal(|| default_index);
    let mut unmuted: Signal<bool> = use_signal(|| false);

    // Clamp `selected` if the source list shrank since the last render
    // (e.g. operator deleted the row). `peek` avoids the read+set
    // deadlock pattern from PURA-132.
    let max_idx = sources.len().saturating_sub(1);
    if *selected.peek() > max_idx {
        selected.set(max_idx);
        unmuted.set(false);
    }

    if sources.is_empty() {
        return rsx! {
            section { class: "pw-video-pane",
                div { class: "pw-video-empty",
                    role: "status",
                    "aria-live": "polite",
                    "No live video"
                }
            }
        };
    }

    let Some(active) = sources.get(*selected.read()) else {
        return rsx! {
            section { class: "pw-video-pane",
                div { class: "pw-video-empty",
                    role: "status",
                    "aria-live": "polite",
                    "No live video"
                }
            }
        };
    };

    let multi_source = sources.len() > 1;
    let active_label = active.label.clone();
    let active_status = active.status.clone();
    let active_namespace = active.track.namespace.clone();

    rsx! {
        section { class: "pw-video-pane",
            if multi_source {
                div { class: "pw-video-tabs",
                    "role": "tablist",
                    "aria-label": "Live video sources",
                    for (idx, src) in sources.iter().enumerate() {
                        {
                            let label = src.label.clone();
                            let is_active = idx == *selected.read();
                            let tab_class = if is_active {
                                "pw-video-tab is-active"
                            } else {
                                "pw-video-tab"
                            };
                            let dead = !src.is_active();
                            rsx! {
                                button {
                                    key: "{src.source_id}",
                                    class: "{tab_class}",
                                    r#type: "button",
                                    "role": "tab",
                                    "aria-selected": if is_active { "true" } else { "false" },
                                    disabled: dead,
                                    onclick: move |_| {
                                        // Switching source remounts VideoPlayer with a
                                        // different namespace; the audio handshake is
                                        // tied to the new mount, so reset to muted to
                                        // preserve autoplay-policy compatibility.
                                        selected.set(idx);
                                        unmuted.set(false);
                                    },
                                    "{label}"
                                    if dead {
                                        span { class: "pw-video-tab-dead", " (offline)" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "pw-video-stage", "aria-label": "{active_label}",
                match relay_url.clone() {
                    Some(url) if active.is_active() => {
                        let muted = !*unmuted.read();
                        // Re-key on (source_id, muted) so Dioxus mounts a
                        // fresh `VideoPlayer` whenever either changes —
                        // tab switch OR unmute toggle both need the
                        // session loop to restart with new params.
                        let player_key = format!("{}|{}", active_namespace, muted);
                        rsx! {
                            VideoPlayer {
                                key: "{player_key}",
                                relay_url: url,
                                namespace: active_namespace.clone(),
                                autoplay: true,
                                muted: muted,
                            }
                            if !*unmuted.read() {
                                button {
                                    class: "pw-video-unmute",
                                    r#type: "button",
                                    "aria-label": "Unmute audio",
                                    onclick: move |_| unmuted.set(true),
                                    span { class: "pw-video-unmute-icon", "🔊" }
                                    span { class: "pw-video-unmute-text", "Tap to unmute" }
                                }
                            }
                        }
                    }
                    Some(_) => rsx! {
                        // Source is listed but `failed` / `stopped` — show
                        // the label + status without trying to subscribe.
                        div { class: "pw-video-empty",
                            role: "status",
                            "aria-live": "polite",
                            "Stream {active_status}"
                        }
                    },
                    None => rsx! {
                        // `relay_url` missing → operator hasn't configured
                        // `MOQ_PUBLIC_URL`. The channel tree still renders.
                        div { class: "pw-video-empty",
                            role: "status",
                            "aria-live": "polite",
                            "No live video"
                        }
                    },
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetHeaderProps {
    server: ts6_manager_shared::widgets::WidgetServer,
}

#[component]
fn WidgetHeader(props: WidgetHeaderProps) -> Element {
    rsx! {
        header { class: "pw-header",
            div { class: "pw-header-row",
                h1 { class: "pw-server-name", title: "{props.server.name}",
                    "{props.server.name}"
                }
                span { class: "pw-online-badge", "ONLINE" }
            }
            div { class: "pw-stats",
                "{props.server.clients_online}/{props.server.max_clients} users · {format_uptime(props.server.uptime_seconds)} uptime"
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetTreeProps {
    channels: Vec<WidgetChannelNode>,
    show_clients: bool,
    show_channel_tree: bool,
}

#[component]
fn WidgetTree(props: WidgetTreeProps) -> Element {
    if !props.show_channel_tree {
        return rsx! { "" };
    }
    if props.channels.is_empty() {
        return rsx! {
            div { class: "pw-empty", "No channels to display." }
        };
    }
    rsx! {
        div { class: "pw-tree",
            ul {
                for node in props.channels.iter() {
                    {
                        let node = node.clone();
                        let show_clients = props.show_clients;
                        rsx! { WidgetNode { node: node, depth: 0, show_clients: show_clients } }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetNodeProps {
    node: WidgetChannelNode,
    depth: u32,
    show_clients: bool,
}

#[component]
fn WidgetNode(props: WidgetNodeProps) -> Element {
    let WidgetNodeProps {
        node,
        depth,
        show_clients,
    } = props;

    if node.is_spacer {
        return rsx! { WidgetSpacer { node: node } };
    }

    let indent_px = (depth as i32) * 16;
    let trim = (36u32).saturating_sub(depth.saturating_mul(2));
    let display_name = truncate_chars(&node.name, trim as usize);
    let count = if show_clients && !node.clients.is_empty() {
        node.clients.len()
    } else {
        0
    };
    rsx! {
        li {
            div { class: "pw-channel", style: "padding-left: {indent_px}px",
                span { class: "pw-channel-icon", "#" }
                span { class: "pw-channel-name", title: "{node.name}", "{display_name}" }
                if node.has_password {
                    span { class: "pw-channel-lock", "aria-label": "password protected", "🔒" }
                }
                if count > 0 {
                    span { class: "pw-channel-count", "{count}" }
                }
            }
            if show_clients {
                for c in node.clients.iter() {
                    {
                        let nick = truncate_chars(&c.nickname, (32u32).saturating_sub(depth.saturating_mul(2)) as usize);
                        let dx = indent_px + 14;
                        let is_away = c.is_away;
                        let is_muted = c.is_muted;
                        rsx! {
                            div { class: "pw-client", style: "padding-left: {dx}px",
                                span { class: "pw-client-dot", "aria-hidden": "true" }
                                span { "{nick}" }
                                if is_away { span { class: "pw-client-flag", "[away]" } }
                                if is_muted { span { class: "pw-client-flag", "[muted]" } }
                            }
                        }
                    }
                }
            }
            if !node.children.is_empty() {
                ul {
                    for child in node.children.iter() {
                        {
                            let child = child.clone();
                            rsx! { WidgetNode { node: child, depth: depth + 1, show_clients: show_clients } }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetSpacerProps {
    node: WidgetChannelNode,
}

#[component]
fn WidgetSpacer(props: WidgetSpacerProps) -> Element {
    let node = props.node;
    match node.spacer_type {
        SpacerType::Line => rsx! { div { class: "pw-spacer-line" } },
        SpacerType::Dotline => rsx! { div { class: "pw-spacer-line dotted" } },
        SpacerType::Dashline => rsx! { div { class: "pw-spacer-line dashed" } },
        SpacerType::Center => rsx! {
            div { class: "pw-spacer center", "{node.spacer_text}" }
        },
        SpacerType::Right => rsx! {
            div { class: "pw-spacer right", "{node.spacer_text}" }
        },
        SpacerType::Left | SpacerType::None => rsx! {
            div { class: "pw-spacer", "{node.spacer_text}" }
        },
    }
}

#[component]
fn WidgetLoading() -> Element {
    // Render the skeleton against the default palette so a brief flash of
    // unstyled content doesn't leak into the iframe before the JSON arrives.
    let style = palette_css_vars(&WidgetThemePalette::DARK);
    rsx! {
        div { class: "pw-root", style: "{style}",
            div { class: "pw-empty",
                role: "status",
                "aria-live": "polite",
                "Loading widget…"
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct WidgetErrorProps {
    err: ApiError,
    on_retry: EventHandler<()>,
}

#[component]
fn WidgetError(props: WidgetErrorProps) -> Element {
    let style = palette_css_vars(&WidgetThemePalette::DARK);
    let (title, body) = describe_error(&props.err);
    rsx! {
        div { class: "pw-root", style: "{style}",
            div { class: "pw-error",
                role: "alert",
                strong { "{title}" }
                p { "{body}" }
                if matches!(&props.err, ApiError::Client { status: 404, .. }) {
                    p { class: "pw-channel-count",
                        "The token may have been rotated or revoked. Ask the operator for a fresh widget URL."
                    }
                } else {
                    button {
                        r#type: "button",
                        onclick: move |_| props.on_retry.call(()),
                        "Retry"
                    }
                }
            }
        }
    }
}

fn describe_error(err: &ApiError) -> (&'static str, String) {
    match err {
        ApiError::Client { status: 404, .. } => (
            "Widget not found",
            "We couldn't find a widget for this URL.".into(),
        ),
        ApiError::Client { status, message } => {
            ("Couldn't load widget", format!("{status}: {message}"))
        }
        ApiError::Server { .. } => (
            "Couldn't load widget",
            "The panel returned an unexpected error. Try again in a moment.".into(),
        ),
        ApiError::BadGateway { error, details, .. } => {
            let mut body = error.clone();
            if let Some(d) = details.as_deref().filter(|s| !s.is_empty()) {
                body.push_str(": ");
                body.push_str(d);
            }
            ("TeamSpeak unavailable", body)
        }
        ApiError::Transport(_) => (
            "Couldn't reach the panel",
            "Check the panel's network reachability and try again.".into(),
        ),
        ApiError::Deserialise(_) => (
            "Unexpected response",
            "The panel returned data the widget couldn't parse.".into(),
        ),
        ApiError::Unauthorized(_) => (
            "Widget not available",
            "This widget URL is no longer valid.".into(),
        ),
        // PURA-232 — the public widget route does not go through the
        // session gate, so this arm should never fire here. Map it to a
        // generic loading message for defence-in-depth.
        ApiError::SessionAnonymous => ("Loading widget…", "Please wait a moment.".into()),
        ApiError::UnsupportedTarget => (
            "Widget unavailable in this view",
            "Public widgets only render in the browser build.".into(),
        ),
    }
}

fn palette_css_vars(p: &WidgetThemePalette) -> String {
    format!(
        "--pw-background:{};--pw-background-secondary:{};--pw-border:{};--pw-text-primary:{};--pw-text-secondary:{};--pw-accent:{};--pw-client-color:{};--pw-header-bg:{};",
        p.background,
        p.background_secondary,
        p.border,
        p.text_primary,
        p.text_secondary,
        p.accent,
        p.client_color,
        p.header_bg,
    )
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
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

// ── Networking — separate WASM/native impls so the page type-checks
//    against both the SSR-snapshot harness and the dx-CLI WASM build.

// PURA-146 (WS-8) — fetch the public video-sources view. Errors are
// non-fatal at the call site: the channel tree still renders when the
// sidecar is unreachable or the operator has not configured a public
// relay URL.
#[cfg(target_arch = "wasm32")]
async fn fetch_public_video_sources(token: &str) -> Result<PublicVideoSourcesPayload, ApiError> {
    use crate::client::api::classify_response;
    use gloo_net::http::Request;
    let url = format!("/api/widget/{token}/video-sources");
    let resp = Request::get(&url)
        .send()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    classify_response(status, &body)
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_public_video_sources(_token: &str) -> Result<PublicVideoSourcesPayload, ApiError> {
    Err(ApiError::UnsupportedTarget)
}

#[cfg(target_arch = "wasm32")]
async fn fetch_widget_data(token: &str) -> Result<WidgetData, ApiError> {
    use crate::client::api::classify_response;
    use gloo_net::http::Request;
    let url = format!("/api/widget/{token}/data");
    let resp = Request::get(&url)
        .send()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    classify_response(status, &body)
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_widget_data(_token: &str) -> Result<WidgetData, ApiError> {
    Err(ApiError::UnsupportedTarget)
}

#[cfg(target_arch = "wasm32")]
fn spawn_ws_listener(
    token: String,
    server_config_id: i64,
    refresh_token: Signal<u64>,
    ws_state: Signal<WsState>,
) {
    let mut refresh_token = refresh_token;
    let mut ws_state = ws_state;
    wasm_bindgen_futures::spawn_local(async move {
        let topic = format!("server:{server_config_id}:widget");
        let url = match build_ws_url(&token) {
            Some(u) => u,
            None => {
                ws_state.set(WsState::Unauthorized);
                return;
            }
        };
        let mut last_event_id: Option<u64> = None;
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        loop {
            ws_state.set(WsState::Connecting);
            let socket = match gloo_net::websocket::futures::WebSocket::open(&url) {
                Ok(s) => s,
                Err(_) => {
                    ws_state.set(WsState::Disconnected);
                    sleep_backoff(&mut backoff_ms).await;
                    continue;
                }
            };
            ws_state.set(WsState::Connected);
            backoff_ms = INITIAL_BACKOFF_MS;
            let exit = drive_socket(socket, &topic, &mut last_event_id, &mut refresh_token).await;
            match exit {
                DriveExit::Reconnect => {
                    ws_state.set(WsState::Disconnected);
                    sleep_backoff(&mut backoff_ms).await;
                }
                DriveExit::Unauthorized => {
                    ws_state.set(WsState::Unauthorized);
                    return;
                }
            }
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_ws_listener(
    _token: String,
    _server_config_id: i64,
    _refresh_token: Signal<u64>,
    _ws_state: Signal<WsState>,
) {
    // SSR / unit tests never open a socket. The page renders the static
    // snapshot and the banner stays in `Connecting` indefinitely — fine for
    // server-rendered HTML, the WASM hydration replaces it.
}

#[cfg(target_arch = "wasm32")]
enum DriveExit {
    Reconnect,
    Unauthorized,
}

#[cfg(target_arch = "wasm32")]
async fn drive_socket(
    mut socket: gloo_net::websocket::futures::WebSocket,
    topic: &str,
    last_event_id: &mut Option<u64>,
    refresh_token: &mut Signal<u64>,
) -> DriveExit {
    use futures::SinkExt;
    use futures::stream::StreamExt;
    use gloo_net::websocket::Message;

    // Emit the subscribe frame. Format mirrors `client::ws::OutFrame::Subscribe`.
    let frame = serde_json::json!({
        "kind": "subscribe",
        "topic": topic,
        "lastEventId": last_event_id,
    });
    if socket.send(Message::Text(frame.to_string())).await.is_err() {
        return DriveExit::Reconnect;
    }
    while let Some(msg) = socket.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(env) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(id) = env.get("id").and_then(|v| v.as_u64()) {
                        if id != 0 {
                            *last_event_id = Some(id);
                        }
                    }
                    // The hub closes the socket itself on auth failure, so a
                    // text frame whose `kind` is `"dropped"` with reason
                    // `"unauthorized"` is the only in-band signal we honour.
                    if env.get("type").and_then(|v| v.as_str()) == Some("dropped")
                        && env
                            .get("data")
                            .and_then(|d| d.get("reason"))
                            .and_then(|v| v.as_str())
                            == Some("unauthorized")
                    {
                        return DriveExit::Unauthorized;
                    }
                    let v = *refresh_token.read();
                    refresh_token.set(v.wrapping_add(1));
                }
            }
            Ok(Message::Bytes(_)) => {}
            Err(_) => return DriveExit::Reconnect,
        }
    }
    DriveExit::Reconnect
}

#[cfg(target_arch = "wasm32")]
fn build_ws_url(token: &str) -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let proto = location.protocol().ok()?;
    let host = location.host().ok()?;
    let scheme = if proto == "https:" { "wss" } else { "ws" };
    Some(format!(
        "{scheme}://{host}/api/ws?token={}",
        urlencoding::encode(token)
    ))
}

#[cfg(target_arch = "wasm32")]
async fn sleep_backoff(backoff_ms: &mut u32) {
    use gloo_timers::future::TimeoutFuture;
    let r = js_sys::Math::random() as f32;
    let jitter_pct = 0.75 + r * 0.5;
    let delay = ((*backoff_ms as f32) * jitter_pct).round() as u32;
    TimeoutFuture::new(delay).await;
    *backoff_ms = (*backoff_ms).saturating_mul(2).min(MAX_BACKOFF_MS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_short_input_passthrough() {
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn truncate_chars_appends_ellipsis_when_over_limit() {
        // Limit of 4 means we keep 3 chars + ellipsis.
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }

    #[test]
    fn truncate_chars_zero_limit_returns_empty() {
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn truncate_chars_handles_multi_byte_codepoints() {
        // Counts chars, not bytes — emoji should not count as 4.
        assert_eq!(truncate_chars("ab🦀cd", 4), "ab🦀…");
    }

    #[test]
    fn format_uptime_renders_days_path() {
        assert_eq!(format_uptime(86_400 + 3600), "1d 01h");
    }

    #[test]
    fn format_uptime_seconds_path() {
        assert_eq!(format_uptime(45), "45s");
    }

    #[test]
    fn palette_css_vars_emits_eight_slots() {
        let css = palette_css_vars(&WidgetThemePalette::DARK);
        for slot in [
            "--pw-background:",
            "--pw-background-secondary:",
            "--pw-border:",
            "--pw-text-primary:",
            "--pw-text-secondary:",
            "--pw-accent:",
            "--pw-client-color:",
            "--pw-header-bg:",
        ] {
            assert!(css.contains(slot), "missing slot {slot} in: {css}");
        }
    }

    #[test]
    fn describe_error_404_uses_not_found_copy() {
        let err = ApiError::Client {
            status: 404,
            message: "Not found".into(),
        };
        let (title, _) = describe_error(&err);
        assert_eq!(title, "Widget not found");
    }

    #[test]
    fn describe_error_502_includes_details() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1153),
            details: Some("invalid serverID".into()),
        };
        let (_, body) = describe_error(&err);
        assert!(body.contains("invalid serverID"), "got: {body}");
    }
}
