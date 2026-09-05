//! `/music-bots/:bot_id` — per-bot operator surface.
//!
//! Renders a single bot's connection state, channel, now-playing card,
//! and queue snapshot. Subscribes to the SSE event stream so reductions
//! land without polling — the underlying [`web_sys::EventSource`] also
//! handles reconnect + last-event-id for free. The stream is opened with
//! `?token=` (browsers cannot set `Authorization` on `EventSource`);
//! Play / transport success also bump the REST snapshot reload so Now
//! Playing does not depend on SSE alone.
//!
//! Playback control buttons (resume / pause / skip / stop) dispatch to
//! the audio-control REST surface (PURA-126). Buttons disable
//! themselves when the bot isn't `InChannel`/`Playing` — the backend
//! would 404 anyway, but disabling client-side keeps the chrome honest.

use dioxus::prelude::*;
use ts6_manager_shared::music_bots as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::music_bots as mb;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::pages::music_bots::shared::{
    audio_source_host, bot_in_channel, effective_playback_state, format_duration, format_error,
    parse_audio_source, source_glyph, state_badge_class, state_label, track_display_title,
    track_title_is_placeholder,
};
use crate::ui::routes::Route;

#[component]
pub fn BotDetailPage(bot_id: u64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut detail: Signal<Option<wire::MusicBotDetail>> = use_signal(|| None);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { mb::get_bot(gate, bot).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(d)) => {
            detail.set(Some(d.clone()));
            error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    // SSE subscription — push events into the detail signal so the page
    // reflects state changes without polling. Held in an `Rc<RefCell>` so
    // the underlying `EventSource` closes on unmount (`BotEventStream`'s
    // `Drop`). Opened only after the first REST snapshot lands, and only
    // when the session has an access JWT: events that arrive before
    // `detail` is `Some` used to be dropped, and browsers cannot set
    // `Authorization` on `EventSource` so the URL carries `?token=`
    // (same query name as `/api/ws`). Library / playlist events still
    // trigger a refetch because the snapshot doesn't carry that data.
    let sse_hold: std::rc::Rc<std::cell::RefCell<Option<mb::BotEventStream>>> =
        use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(None)));

    use_effect({
        let sse_hold = sse_hold.clone();
        let session = session.clone();
        move || {
            if sse_hold.borrow().is_some() {
                return;
            }
            let access = match &*session.state.read() {
                AuthState::Authenticated { access, .. } if !access.is_empty() => access.clone(),
                _ => return,
            };
            // Wait for the first snapshot so early events aren't applied
            // against `None` (and dropped). `peek` after the first Some
            // is unnecessary — the armed hold short-circuits above.
            if detail.peek().is_none() {
                let _ = detail.read();
                return;
            }
            let mut reload = reload;
            let mut detail = detail;
            let stream = mb::open_bot_event_source(bot, &access, move |ev| {
                detail.with_mut(|d| {
                    let Some(snap) = d.as_mut() else { return };
                    apply_event(snap, &ev);
                });
                if matches!(
                    ev,
                    wire::BotEventWire::LibraryChanged | wire::BotEventWire::PlaylistChanged { .. }
                ) {
                    reload.with_mut(|n| *n += 1);
                }
            });
            *sse_hold.borrow_mut() = Some(stream);
        }
    });

    let bump = move || reload.with_mut(|n| *n += 1);

    let on_connect = {
        let gate = gate.clone();
        let mut bump = bump;
        move |_| {
            let gate = gate.clone();
            spawn(async move {
                match mb::connect_bot(gate, bot).await {
                    Ok(()) => toaster.push(ToastVariant::Success, "Connecting", None),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Connect failed",
                        Some(format_error(&e)),
                    ),
                }
                bump();
            });
        }
    };

    let on_disconnect = {
        let gate = gate.clone();
        let mut bump = bump;
        move |_| {
            let gate = gate.clone();
            spawn(async move {
                match mb::disconnect_bot(gate, bot).await {
                    Ok(()) => toaster.push(ToastVariant::Success, "Disconnecting", None),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Disconnect failed",
                        Some(format_error(&e)),
                    ),
                }
                bump();
            });
        }
    };

    let mut join_input: Signal<String> = use_signal(String::new);
    // Drives `disabled` on the submit button so the form can't fire a
    // validation toast that contradicts the bot's connected-state badge.
    let join_channel_id = use_memo(move || join_input.read().trim().parse::<u64>().ok());
    let on_join = {
        let gate = gate.clone();
        let mut bump = bump;
        move |evt: FormEvent| {
            evt.prevent_default();
            let Some(channel_id) = *join_channel_id.read() else {
                return;
            };
            let gate = gate.clone();
            spawn(async move {
                match mb::join_channel(gate, bot, channel_id).await {
                    Ok(()) => toaster.push(
                        ToastVariant::Success,
                        format!("Joining channel {channel_id}"),
                        None,
                    ),
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Join failed", Some(format_error(&e)))
                    }
                }
                bump();
            });
            join_input.set(String::new());
        }
    };

    let on_leave = {
        let gate = gate.clone();
        let mut bump = bump;
        move |_| {
            let gate = gate.clone();
            spawn(async move {
                match mb::leave_channel(gate, bot).await {
                    Ok(()) => toaster.push(ToastVariant::Success, "Leaving channel", None),
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Leave failed", Some(format_error(&e)))
                    }
                }
                bump();
            });
        }
    };

    let snap = detail.read().clone();
    let crumb_name = snap
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| format!("Bot {}", bot.0));
    // Handshake auto-join leaves the FSM on `Connected` while `channel_id`
    // is set. Collapse that onto `InChannel` so the badge, channel copy,
    // and transport cannot disagree.
    let shown_state = snap
        .as_ref()
        .map(|d| effective_playback_state(d.state, d.channel_id));
    let in_a_channel = snap
        .as_ref()
        .is_some_and(|d| bot_in_channel(d.state, d.channel_id));
    rsx! {
        div { class: "crumb",
            Link { to: Route::BotsIndexPage {}, "Music bots" }
            " · "
            "{crumb_name}"
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load bot".to_string(),
                "{format_error(err)}"
            }
        }

        if *loading.read() && snap.is_none() {
            div { class: "card", aria_busy: "true",
                p { class: "muted", "Loading bot…" }
            }
        } else if let Some(d) = snap {
            section { class: "page-header",
                div { class: "page-title-block",
                    h1 { "{d.name}" }
                    p { class: "page-lede",
                        span { class: state_badge_class(shown_state.unwrap_or(d.state)),
                            "{state_label(shown_state.unwrap_or(d.state))}"
                        }
                        " · {d.server_addr}"
                        if in_a_channel {
                            if let Some(cid) = d.channel_id {
                                " · channel {cid}"
                            }
                        }
                    }
                }
                div { class: "page-actions",
                    Link {
                        to: Route::MusicLibraryPage { bot_id },
                        class: "btn btn-ghost",
                        "Library"
                    }
                    Link {
                        to: Route::MusicPlaylistsPage { bot_id },
                        class: "btn btn-ghost",
                        "Playlists"
                    }
                    Link {
                        to: Route::RadioStationsPage { bot_id },
                        class: "btn btn-ghost",
                        "Radio"
                    }
                }
            }

            section { class: "stack-md",
                div { class: "card",
                    h3 { "Connection" }
                    div { class: "card-actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            size: ButtonSize::Small,
                            disabled: matches!(d.state, wire::BotState::Connected | wire::BotState::InChannel | wire::BotState::Playing | wire::BotState::Connecting),
                            onclick: on_connect,
                            "Connect"
                        }
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            disabled: matches!(d.state, wire::BotState::Disconnected | wire::BotState::Disconnecting),
                            onclick: on_disconnect,
                            "Disconnect"
                        }
                    }
                    form {
                        class: "card-row",
                        onsubmit: on_join,
                        label { class: "sr-only", r#for: "bot-channel-input", "Channel id" }
                        input {
                            id: "bot-channel-input",
                            class: "input input-sm",
                            placeholder: "Channel id",
                            inputmode: "numeric",
                            value: "{join_input.read()}",
                            oninput: move |e| join_input.set(e.value()),
                        }
                        Button {
                            variant: ButtonVariant::Primary,
                            size: ButtonSize::Small,
                            kind: ButtonType::Submit,
                            disabled: join_channel_id.read().is_none(),
                            "Join channel"
                        }
                        Button {
                            variant: ButtonVariant::Ghost,
                            size: ButtonSize::Small,
                            disabled: !in_a_channel,
                            onclick: on_leave,
                            "Leave"
                        }
                    }
                }

                PlayNowComposer { bot_id, state: d.state, reload }

                PlayerCard { bot_id, detail, reload }

                QueueCard { bot_id, detail }
            }
        }
    }
}

/// PURA-344 — the "Player card": now-playing widget + transport controls
/// fused into one perceptual unit (Gestalt common region — the transport
/// acts on the track shown directly above it). Reads the live `detail`
/// signal so SSE reductions land without a prop round-trip.
#[component]
fn PlayerCard(
    bot_id: u64,
    detail: Signal<Option<wire::MusicBotDetail>>,
    reload: Signal<u64>,
) -> Element {
    let Some(d) = detail.read().clone() else {
        return rsx! {};
    };
    let playing = matches!(d.state, wire::BotState::Playing);
    let eq_class = if playing { "np-eq is-playing" } else { "np-eq" };
    // PURA-349 — whole-percent fill for the now-playing progress bar.
    // `None` when the play clock or duration is unknown; in that case
    // the equalizer is shown instead of a bar that can't be filled.
    let progress_pct = track_progress(&d).map(|ratio| (ratio * 100.0).round() as u32);
    // PURA-352 — raw values for the interactive seek slider. Both are
    // `Some` exactly when `progress_pct` is (`track_progress` requires a
    // known duration > 0 and a play clock), so the seekable bar renders
    // on the same condition as the read-only one.
    let duration_secs = d.now_playing.as_ref().and_then(|t| t.duration_secs);
    let elapsed_secs = d.now_playing_elapsed_secs.unwrap_or(0);

    // PURA-352 — seek dispatch. `oninput` fires continuously while the
    // operator drags, so it only moves the fill optimistically; the
    // actual `POST /seek` is sent once on `onchange` (drag release or a
    // track click). On failure the optimistic position is reverted and
    // the SSE stream stays the authoritative reconciler.
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let mut detail = detail;
    let on_seek_input = move |e: FormEvent| {
        // Drag in progress — move the fill optimistically, no POST yet.
        if let Ok(secs) = e.value().parse::<u64>() {
            detail.with_mut(|opt| {
                if let Some(s) = opt.as_mut() {
                    s.now_playing_elapsed_secs = Some(secs);
                }
            });
        }
    };
    let on_seek_commit = move |e: FormEvent| {
        let Ok(secs) = e.value().parse::<u64>() else {
            return;
        };
        let prev = detail.read().clone();
        detail.with_mut(|opt| {
            if let Some(s) = opt.as_mut() {
                s.now_playing_elapsed_secs = Some(secs);
            }
        });
        let gate = gate.clone();
        spawn(async move {
            if let Err(err) = mb::seek(gate, bot, secs).await {
                detail.set(prev);
                toaster.push(
                    ToastVariant::Danger,
                    "Seek failed".to_string(),
                    Some(format_error(&err)),
                );
            }
        });
    };

    rsx! {
        div { class: "card player-card",
            div { class: "np-eyebrow", "Now playing" }
            // THE-927 + warm-retry — transient resolving pill. Lives
            // between `BotEventWire::Resolving` (chat `!play yt:<query>`
            // or an in-pipeline warm retry) and `FirstFrameOnWire` /
            // NowPlaying / error / idle. Sits above the now-playing row
            // so it's visible regardless of whether a previous track
            // still occupies the title strip. First attempt is quiet
            // ("resolving…"); the automatic second warm attempt stamps
            // `retrying` / `resolvingRetrying` and the copy becomes
            // "resolving / retrying…".
            if let Some(query) = d.resolving_query.as_deref() {
                div {
                    class: if d.resolving_retrying {
                        "np-resolving np-resolving--retrying"
                    } else {
                        "np-resolving"
                    },
                    role: "status",
                    "aria-live": "polite",
                    span { class: "np-resolving__spinner", aria_hidden: "true" }
                    span { class: "np-resolving__label", "{resolving_status_label(d.resolving_retrying)}" }
                    span { class: "np-resolving__query", title: "{query}", "{query}" }
                }
            }
            if let Some(track) = d.now_playing.as_ref() {
                div { class: "now-playing",
                    if progress_pct.is_none() {
                        span { class: eq_class, aria_hidden: "true",
                            span {} span {} span {}
                        }
                    }
                    div { class: "np-body",
                        div {
                            class: "np-title",
                            title: "{track_display_title(track)}",
                            "aria-live": "polite",
                            "{track_display_title(track)}"
                        }
                        div { class: "np-meta",
                            span { class: "np-glyph", aria_hidden: "true",
                                "{source_glyph(&track.source)}"
                            }
                            span { "{audio_source_host(&track.source)}" }
                            if let Some(by) = track.requested_by.as_deref() {
                                span { class: "np-dot", "·" }
                                span { "requested by {by}" }
                            }
                            span { class: "np-dot", "·" }
                            span { "{d.name}" }
                        }
                        div { class: "np-duration", "{format_duration(track.duration_secs)}" }
                        // PURA-349 — left-anchored playback progress.
                        // Advances only on a ~1 Hz `Progress` SSE tick
                        // (no client-side timer); falls back to the
                        // equalizer above when it can't be filled.
                        // PURA-352 — the bar is now seekable: a transparent
                        // range input sits over the fill so a click or drag
                        // scrubs the track. The fill underneath shows the
                        // position; `oninput` tracks the drag optimistically
                        // and `onchange` posts the seek.
                        if let Some(pct) = progress_pct {
                            div {
                                class: "np-progress np-progress--seekable",
                                role: "progressbar",
                                "aria-label": "Playback progress",
                                "aria-valuemin": "0",
                                "aria-valuemax": "100",
                                "aria-valuenow": "{pct}",
                                div {
                                    class: "np-progress__fill",
                                    style: "width: {pct}%;",
                                }
                                input {
                                    class: "np-progress__seek",
                                    r#type: "range",
                                    min: "0",
                                    max: "{duration_secs.unwrap_or(0)}",
                                    step: "1",
                                    value: "{elapsed_secs}",
                                    "aria-label": "Seek within the current track",
                                    oninput: on_seek_input,
                                    onchange: on_seek_commit,
                                }
                            }
                        }
                    }
                }
            } else {
                div { class: "now-playing now-playing--empty",
                    p { class: "muted", "Nothing playing" }
                    p { class: "muted small",
                        "Use Play now above or enqueue a track to start."
                    }
                }
            }
            // PURA-270 — surface the cause of a failed track. The backend
            // drops a failed bot back to `Connected`/`InChannel` (no
            // `Failed` wire state) and exposes the reason as `last_error`;
            // show it only while the bot isn't playing so a fresh track's
            // `NowPlaying` (which clears `last_error`) hides a stale banner.
            if d.last_error.is_some() && !playing {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Last track failed to play".to_string(),
                    "{d.last_error.as_deref().unwrap_or_default()}"
                }
            }
            PlayerControls { bot_id, detail, reload }
        }
    }
}

/// PURA-349 — fill ratio (`0.0..=1.0`) for the now-playing progress
/// bar. Returns `None` when either the play clock or the track duration
/// is unknown (ICY radio streams, id=0 tracks, or a zero-length
/// duration): the caller renders the 3-bar equalizer instead, rather
/// than a bar it can't fill truthfully.
fn track_progress(d: &wire::MusicBotDetail) -> Option<f64> {
    let duration = d.now_playing.as_ref()?.duration_secs?;
    if duration == 0 {
        return None;
    }
    let elapsed = d.now_playing_elapsed_secs?;
    Some((elapsed as f64 / duration as f64).clamp(0.0, 1.0))
}

/// One stateful Play/Pause control + Prev/Skip/Stop. Per PURA-343 §5:
/// optimistic state flip on click, whole-bar disable while a command is
/// in flight, spinner only past ~400ms, revert on error.
#[component]
fn PlayerControls(
    bot_id: u64,
    detail: Signal<Option<wire::MusicBotDetail>>,
    mut reload: Signal<u64>,
) -> Element {
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let mut detail = detail;
    // The command currently awaiting its REST round-trip, if any. Drives
    // the whole-bar disable; `slow` only flips the spinner on past 400ms.
    let mut pending: Signal<Option<AudioAction>> = use_signal(|| None);
    let mut slow: Signal<bool> = use_signal(|| false);

    let Some(d) = detail.read().clone() else {
        return rsx! {};
    };
    // Audio commands are meaningless when the bot isn't in a channel —
    // the backend would 404 the dispatch. Collapse `Connected` +
    // `channel_id` onto `InChannel` so the default-channel auto-join
    // cannot disable Pause/Stop while the header shows a channel.
    let shown = effective_playback_state(d.state, d.channel_id);
    let in_channel = bot_in_channel(d.state, d.channel_id);
    let has_track = d.now_playing.is_some();
    let busy = pending.read().is_some();
    let transport = transport_action(shown, has_track, d.queue.len());

    let dispatch = move |action: AudioAction| {
        if pending.read().is_some() {
            return;
        }
        let prev = detail.read().clone();
        detail.with_mut(|opt| {
            if let Some(s) = opt.as_mut() {
                apply_optimistic(s, action);
            }
        });
        pending.set(Some(action));
        slow.set(false);
        // Doherty threshold — only show a spinner if the round-trip is
        // still outstanding after ~400ms; a fast command never flickers.
        spawn(async move {
            gloo_timers::future::TimeoutFuture::new(400).await;
            if *pending.read() == Some(action) {
                slow.set(true);
            }
        });
        let gate = gate.clone();
        spawn(async move {
            let res = match action {
                AudioAction::SkipPrev => mb::skip_prev(gate, bot).await,
                AudioAction::Resume => mb::resume_bot(gate, bot).await,
                AudioAction::Pause => mb::pause_bot(gate, bot).await,
                AudioAction::SkipNext => mb::skip_next(gate, bot).await,
                AudioAction::Stop => mb::stop_bot(gate, bot).await,
            };
            pending.set(None);
            slow.set(false);
            match res {
                Ok(()) => {
                    toaster.push(ToastVariant::Success, action.success_label(), None);
                    // Same reload bump as connect/disconnect — hydrate
                    // now_playing / queue from REST if SSE is still catching up.
                    reload.with_mut(|n| *n += 1);
                }
                Err(e) => {
                    // Revert the optimistic flip to the last server-
                    // confirmed snapshot. The SSE stream remains the
                    // authoritative reconciler on the success path.
                    detail.set(prev);
                    toaster.push(
                        ToastVariant::Danger,
                        action.failure_label(),
                        Some(format_error(&e)),
                    );
                }
            }
        });
    };

    let spinner_for = move |action: AudioAction| *slow.read() && *pending.read() == Some(action);

    let on_prev = {
        let mut dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::SkipPrev)
    };
    let on_play_pause = {
        let mut dispatch = dispatch.clone();
        let action = transport.action;
        move |_| dispatch(action)
    };
    let on_skip = {
        let mut dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::SkipNext)
    };
    let on_stop = {
        let mut dispatch = dispatch;
        move |_| dispatch(AudioAction::Stop)
    };

    rsx! {
        div { class: "player-controls",
            Button {
                variant: ButtonVariant::Ghost,
                disabled: !in_channel || busy,
                loading: spinner_for(AudioAction::SkipPrev),
                aria_label: "Previous track".to_string(),
                onclick: on_prev,
                "⏮"
            }
            Button {
                variant: ButtonVariant::Primary,
                size: ButtonSize::Large,
                disabled: !in_channel || transport.disabled || busy,
                loading: spinner_for(AudioAction::Resume) || spinner_for(AudioAction::Pause),
                aria_label: transport.aria.to_string(),
                onclick: on_play_pause,
                "{transport.label}"
            }
            Button {
                variant: ButtonVariant::Ghost,
                disabled: !in_channel || busy,
                loading: spinner_for(AudioAction::SkipNext),
                aria_label: "Skip to next track".to_string(),
                onclick: on_skip,
                "⏭"
            }
            div { class: "player-controls__spacer" }
            VolumeSlider { bot_id, disabled: !in_channel }
            Button {
                variant: ButtonVariant::Danger,
                disabled: !in_channel || !has_track || busy,
                loading: spinner_for(AudioAction::Stop),
                onclick: on_stop,
                "⏹ Stop"
            }
        }
        if !in_channel {
            p { class: "muted small",
                "Bot must be in a channel to control playback. Connect and join a channel first."
            }
        }
    }
}

/// Resolved Play/Pause control for a given bot state — single stateful
/// button per PURA-343 §5.2 (no `aria-pressed` toggle; the accessible
/// *name* tracks state instead).
struct Transport {
    label: &'static str,
    aria: &'static str,
    action: AudioAction,
    disabled: bool,
}

fn transport_action(state: wire::BotState, has_track: bool, queue_len: usize) -> Transport {
    match state {
        wire::BotState::Playing => Transport {
            label: "⏸ Pause",
            aria: "Pause",
            action: AudioAction::Pause,
            disabled: false,
        },
        wire::BotState::InChannel if has_track => Transport {
            label: "▶ Resume",
            aria: "Resume",
            action: AudioAction::Resume,
            disabled: false,
        },
        wire::BotState::InChannel if queue_len > 0 => Transport {
            label: "▶ Play",
            aria: "Play",
            action: AudioAction::Resume,
            disabled: false,
        },
        wire::BotState::InChannel => Transport {
            label: "▶ Play",
            aria: "Play (queue is empty)",
            action: AudioAction::Resume,
            disabled: true,
        },
        _ => Transport {
            label: "▶ Play",
            aria: "Play",
            action: AudioAction::Resume,
            disabled: true,
        },
    }
}

/// Optimistic local flip applied the instant a transport button is
/// clicked, before the REST round-trip resolves. Reconciled by the SSE
/// `StateChanged` / `NowPlaying` event on success, reverted on error.
fn apply_optimistic(d: &mut wire::MusicBotDetail, action: AudioAction) {
    match action {
        AudioAction::Pause => {
            if d.state == wire::BotState::Playing {
                d.state = wire::BotState::InChannel;
            }
        }
        AudioAction::Resume => {
            if d.state == wire::BotState::InChannel {
                d.state = wire::BotState::Playing;
            }
        }
        AudioAction::Stop => {
            if d.state == wire::BotState::Playing {
                d.state = wire::BotState::InChannel;
            }
        }
        AudioAction::SkipPrev | AudioAction::SkipNext => {}
    }
}

#[component]
fn PlayNowComposer(bot_id: u64, state: wire::BotState, mut reload: Signal<u64>) -> Element {
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut url_input: Signal<String> = use_signal(String::new);
    let mut inline_error: Signal<Option<String>> = use_signal(|| None::<String>);
    let mut submitting: Signal<bool> = use_signal(|| false);

    // Audio dispatch only reaches the actor when the bot is on the
    // server. Mirrors the `PlaybackControls` predicate at the wider end
    // — the route layer enforces the in-channel requirement and the
    // inline error surfaces that to the operator.
    let on_server = matches!(
        state,
        wire::BotState::Connected | wire::BotState::InChannel | wire::BotState::Playing
    );

    let on_submit = {
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *submitting.read() {
                return;
            }
            let raw = url_input.read().clone();
            let Some(source) = parse_audio_source(&raw) else {
                inline_error.set(Some(
                    "Paste a URL, or a library:relative/path.mp3 source.".into(),
                ));
                return;
            };
            inline_error.set(None);
            submitting.set(true);
            let gate = gate.clone();
            spawn(async move {
                let res = mb::play_source(gate, bot, source).await;
                submitting.set(false);
                match res {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Playing", None);
                        url_input.set(String::new());
                        // Belt-and-suspenders: Play used to rely only on
                        // SSE to flip Now Playing. Bump the same reload
                        // signal connect/disconnect use so `get_bot`
                        // hydrates `now_playing` even if the event stream
                        // is still catching up.
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => {
                        let msg = format_error(&e);
                        toaster.push(ToastVariant::Danger, "Play failed", Some(msg.clone()));
                        inline_error.set(Some(msg));
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "card",
            h3 { "Play now" }
            form { class: "card-row", onsubmit: on_submit,
                label { class: "sr-only", r#for: "bot-play-source-input", "Audio source" }
                input {
                    id: "bot-play-source-input",
                    class: "input",
                    placeholder: "Paste a YouTube link, stream URL, or library:path/to.mp3",
                    value: "{url_input.read()}",
                    oninput: move |e| url_input.set(e.value()),
                }
                Button {
                    variant: ButtonVariant::Primary,
                    kind: ButtonType::Submit,
                    disabled: !on_server,
                    loading: *submitting.read(),
                    "Play"
                }
            }
            p { class: "muted small",
                "YouTube, SoundCloud, direct streams (Icecast), and any URL yt-dlp resolves are supported. Prefix a library entry with "
                code { "library:" }
                "."
            }
            if !on_server {
                p { class: "muted small",
                    "Bot must be connected to play. Connect (and join a channel) first."
                }
            }
            if let Some(msg) = inline_error.read().as_ref() {
                Banner { variant: BannerVariant::Danger, title: "Could not start playback".to_string(),
                    "{msg}"
                }
            }
        }
    }
}

/// PURA-351 — output-volume slider for the transport bar. Posts a linear
/// gain (`0.0..=1.0`) to `POST /api/music-bots/{id}/volume` when the
/// operator releases the slider. Optimistic: the thumb and the percent
/// label track the drag immediately; a failed POST reverts to the last
/// confirmed value and toasts.
///
/// The bot snapshot does not echo the current gain, so the slider opens
/// at 100 % on every page load. The music bot itself defaults to unity
/// gain, so that is truthful for a fresh bot; restoring a non-default
/// level after a reload is a follow-up (would need the gain in
/// `MusicBotDetail`).
#[component]
fn VolumeSlider(bot_id: u64, disabled: bool) -> Element {
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();
    // Operator-facing volume as a whole percent (0–100); 100 = unity gain.
    let mut volume: Signal<u8> = use_signal(|| 100);

    let on_change = move |evt: FormEvent| {
        let Ok(pct) = evt.value().parse::<u8>() else {
            return;
        };
        let pct = pct.min(100);
        let prev = *volume.peek();
        if pct == prev {
            return;
        }
        // Optimistic — the label moves with the thumb; the SSE stream
        // carries no volume event, so this local signal is authoritative
        // for the control's own display.
        volume.set(pct);
        let gate = gate.clone();
        let toaster = toaster;
        spawn(async move {
            let gain = f32::from(pct) / 100.0;
            if let Err(e) = mb::set_volume(gate, bot, gain).await {
                // Revert to the last value the server accepted.
                volume.set(prev);
                toaster.push(
                    ToastVariant::Danger,
                    "Volume change failed",
                    Some(format_error(&e)),
                );
            }
        });
    };

    rsx! {
        div { class: "volume-control",
            span { class: "volume-control__glyph", aria_hidden: "true", "🔊" }
            input {
                class: "volume-control__slider",
                r#type: "range",
                min: "0",
                max: "100",
                step: "1",
                value: "{volume}",
                disabled,
                aria_label: "Output volume",
                onchange: on_change,
            }
            span { class: "volume-control__value", "{volume}%" }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AudioAction {
    SkipPrev,
    Resume,
    Pause,
    SkipNext,
    Stop,
}

impl AudioAction {
    fn success_label(self) -> &'static str {
        match self {
            AudioAction::SkipPrev => "Skipping back",
            AudioAction::Resume => "Resuming",
            AudioAction::Pause => "Paused",
            AudioAction::SkipNext => "Skipping",
            AudioAction::Stop => "Stopped",
        }
    }

    fn failure_label(self) -> &'static str {
        match self {
            AudioAction::SkipPrev => "Skip back failed",
            AudioAction::Resume => "Resume failed",
            AudioAction::Pause => "Pause failed",
            AudioAction::SkipNext => "Skip failed",
            AudioAction::Stop => "Stop failed",
        }
    }
}

/// Return `queue` with the entries at `a` and `b` swapped. Out-of-range
/// indices are a no-op so a disabled first-row `↑` (or last-row `↓`) that
/// still fires can't panic.
fn reorder_swap(queue: &[wire::Track], a: usize, b: usize) -> Vec<wire::Track> {
    let mut v = queue.to_vec();
    if a < v.len() && b < v.len() {
        v.swap(a, b);
    }
    v
}

/// PURA-344 — the Queue card: upcoming tracks with keyboard-operable
/// up/down reorder (PURA-343 §7.2 chose buttons over drag-and-drop),
/// per-row remove, and a confirmed "Clear queue". All three mutations
/// apply optimistically to the live `detail` signal, then reconcile from
/// the `QueueChanged` SSE event (or revert on error).
#[component]
fn QueueCard(bot_id: u64, detail: Signal<Option<wire::MusicBotDetail>>) -> Element {
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let mut detail = detail;
    let mut confirm_clear: Signal<bool> = use_signal(|| false);
    // A queue mutation is in flight — lock the action buttons so a second
    // reorder/remove can't race the first against a stale permutation.
    let mut busy: Signal<bool> = use_signal(|| false);

    let Some(d) = detail.read().clone() else {
        return rsx! {};
    };
    let queue = d.queue.clone();
    let len = queue.len();
    let locked = *busy.read();

    let snapshot_queue = move || -> Vec<wire::Track> {
        detail
            .read()
            .as_ref()
            .map(|d| d.queue.clone())
            .unwrap_or_default()
    };

    let do_reorder = {
        let gate = gate.clone();
        move |a: usize, b: usize| {
            if *busy.read() {
                return;
            }
            let prev = snapshot_queue();
            let next = reorder_swap(&prev, a, b);
            if next == prev {
                return;
            }
            let track_ids: Vec<wire::TrackId> = next.iter().map(|t| t.id).collect();
            detail.with_mut(|opt| {
                if let Some(s) = opt.as_mut() {
                    s.queue = next;
                }
            });
            busy.set(true);
            let gate = gate.clone();
            spawn(async move {
                let res = mb::reorder_queue(gate, bot, track_ids).await;
                busy.set(false);
                match res {
                    Ok(tracks) => detail.with_mut(|opt| {
                        if let Some(s) = opt.as_mut() {
                            s.queue = tracks;
                        }
                    }),
                    Err(e) => {
                        detail.with_mut(|opt| {
                            if let Some(s) = opt.as_mut() {
                                s.queue = prev;
                            }
                        });
                        toaster.push(
                            ToastVariant::Danger,
                            "Reorder failed",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let do_remove = {
        let gate = gate.clone();
        move |track: wire::TrackId| {
            if *busy.read() {
                return;
            }
            let prev = snapshot_queue();
            detail.with_mut(|opt| {
                if let Some(s) = opt.as_mut() {
                    s.queue.retain(|t| t.id != track);
                }
            });
            busy.set(true);
            let gate = gate.clone();
            spawn(async move {
                let res = mb::remove_queue_track(gate, bot, track).await;
                busy.set(false);
                match res {
                    Ok(()) => toaster.push(ToastVariant::Success, "Removed from queue", None),
                    Err(e) => {
                        detail.with_mut(|opt| {
                            if let Some(s) = opt.as_mut() {
                                s.queue = prev;
                            }
                        });
                        toaster.push(
                            ToastVariant::Danger,
                            "Remove failed",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let on_clear = {
        let gate = gate.clone();
        move |_| {
            let prev = snapshot_queue();
            detail.with_mut(|opt| {
                if let Some(s) = opt.as_mut() {
                    s.queue.clear();
                }
            });
            confirm_clear.set(false);
            busy.set(true);
            let gate = gate.clone();
            spawn(async move {
                let res = mb::clear_queue(gate, bot).await;
                busy.set(false);
                match res {
                    Ok(()) => toaster.push(ToastVariant::Success, "Queue cleared", None),
                    Err(e) => {
                        detail.with_mut(|opt| {
                            if let Some(s) = opt.as_mut() {
                                s.queue = prev;
                            }
                        });
                        toaster.push(ToastVariant::Danger, "Clear failed", Some(format_error(&e)));
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "card queue-card",
            div { class: "card-header",
                h3 { "Up next" }
                div { class: "card-header__aside",
                    span { class: "muted", "{len} tracks" }
                    if len > 0 {
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            disabled: locked,
                            onclick: move |_| confirm_clear.set(true),
                            "Clear queue"
                        }
                    }
                }
            }
            if queue.is_empty() {
                div { class: "empty",
                    div { class: "icon", aria_hidden: "true", "♪" }
                    h3 { "Queue is empty" }
                    p {
                        "Enqueue tracks from the "
                        Link { to: Route::MusicPlaylistsPage { bot_id }, "Playlists" }
                        " page, or use Play now above."
                    }
                }
            } else {
                ol { class: "queue",
                    for (idx, track) in queue.iter().cloned().enumerate() {
                        QueueRow {
                            key: "{track.id.0}",
                            track: track.clone(),
                            position: idx + 1,
                            is_first: idx == 0,
                            is_last: idx + 1 == len,
                            locked,
                            on_up: {
                                let mut do_reorder = do_reorder.clone();
                                move |_| do_reorder(idx, idx - 1)
                            },
                            on_down: {
                                let mut do_reorder = do_reorder.clone();
                                move |_| do_reorder(idx, idx + 1)
                            },
                            on_remove: {
                                let mut do_remove = do_remove.clone();
                                let id = track.id;
                                move |_| do_remove(id)
                            },
                        }
                    }
                }
            }
        }
        if *confirm_clear.read() {
            div { class: "modal-backdrop", onclick: move |_| confirm_clear.set(false),
                div {
                    class: "modal modal-sm",
                    onclick: move |evt| evt.stop_propagation(),
                    role: "dialog",
                    "aria-modal": "true",
                    "aria-labelledby": "clear-queue-title",
                    div { class: "modal-header",
                        h2 { id: "clear-queue-title", "Clear the queue?" }
                        button {
                            r#type: "button",
                            class: "modal-close",
                            "aria-label": "Close",
                            onclick: move |_| confirm_clear.set(false),
                            "×"
                        }
                    }
                    div { class: "modal-body",
                        p {
                            "This removes all {len} tracks. The current track keeps playing."
                        }
                    }
                    div { class: "modal-actions",
                        Button {
                            variant: ButtonVariant::Ghost,
                            onclick: move |_| confirm_clear.set(false),
                            "Cancel"
                        }
                        Button {
                            variant: ButtonVariant::Danger,
                            onclick: on_clear,
                            "Clear queue"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn QueueRow(
    track: wire::Track,
    position: usize,
    is_first: bool,
    is_last: bool,
    locked: bool,
    on_up: EventHandler<MouseEvent>,
    on_down: EventHandler<MouseEvent>,
    on_remove: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        li { class: "queue-row",
            span { class: "queue-row__pos", "{position}." }
            div { class: "queue-row__body",
                span { class: "queue-row__title", title: "{track_display_title(&track)}", "{track_display_title(&track)}" }
                span { class: "queue-row__sub",
                    "{audio_source_host(&track.source)}"
                    if let Some(by) = track.requested_by.as_deref() {
                        " · requested by {by}"
                    }
                }
            }
            span { class: "queue-row__duration", "{format_duration(track.duration_secs)}" }
            div { class: "queue-row__actions",
                button {
                    r#type: "button",
                    class: "icon-btn",
                    disabled: locked || is_first,
                    "aria-label": "Move {track_display_title(&track)} up",
                    onclick: move |e| on_up.call(e),
                    "↑"
                }
                button {
                    r#type: "button",
                    class: "icon-btn",
                    disabled: locked || is_last,
                    "aria-label": "Move {track_display_title(&track)} down",
                    onclick: move |e| on_down.call(e),
                    "↓"
                }
                button {
                    r#type: "button",
                    class: "icon-btn icon-btn--danger",
                    disabled: locked,
                    "aria-label": "Remove {track_display_title(&track)} from queue",
                    onclick: move |e| on_remove.call(e),
                    "✕"
                }
            }
        }
    }
}

/// Keep a resolved title when a later event for the same source only
/// carries the Play-stamped URL placeholder (warm resolve title may
/// arrive first via ICY/`NowPlaying`, then a REST-shaped snapshot or
/// `QueueChanged` restates the URL).
fn prefer_resolved_title(existing: Option<&wire::Track>, incoming: &wire::Track) -> wire::Track {
    let Some(prev) = existing else {
        return incoming.clone();
    };
    if prev.source != incoming.source {
        return incoming.clone();
    }
    if track_title_is_placeholder(incoming) && !track_title_is_placeholder(prev) {
        let mut merged = incoming.clone();
        merged.title = prev.title.clone();
        return merged;
    }
    incoming.clone()
}

/// Copy for the Now Playing resolving pill. First warm attempt is quiet;
/// the automatic second attempt (Music Bot PR #21 `retrying: true`) is
/// the louder "resolving / retrying…" line.
fn resolving_status_label(retrying: bool) -> &'static str {
    if retrying {
        "resolving / retrying…"
    } else {
        "resolving…"
    }
}

fn clear_resolving(d: &mut wire::MusicBotDetail) {
    d.resolving_query = None;
    d.resolving_retrying = false;
}

/// Reduce a single SSE event into the locally-held [`wire::MusicBotDetail`]
/// snapshot. Every variant only mutates fields the route layer projects
/// onto the wire.
fn apply_event(d: &mut wire::MusicBotDetail, ev: &wire::BotEventWire) {
    match ev {
        wire::BotEventWire::StateChanged { to, .. } => {
            d.state = *to;
            if matches!(
                to,
                wire::BotState::Disconnected | wire::BotState::Disconnecting
            ) {
                d.channel_id = None;
                d.now_playing = None;
                d.now_playing_elapsed_secs = None;
                d.last_error = None;
                // THE-927 — a disconnect mid-resolve abandons the wait;
                // drop the pill so it doesn't stick around as a ghost.
                clear_resolving(d);
            } else if *to == wire::BotState::Playing {
                // Synthesised Playing means a track is on the wire;
                // the resolving chrome has done its job.
                clear_resolving(d);
            } else if *to == wire::BotState::Connected && d.channel_id.is_some() {
                // Handshake emits StateChanged(Connected) around the
                // default-channel JoinedChannel. Don't let the FSM step
                // hide an already-recorded channel from the chrome.
                d.state = wire::BotState::InChannel;
            }
        }
        wire::BotEventWire::Connected {
            default_channel, ..
        } => {
            d.channel_id = Some(*default_channel);
            d.last_error = None;
            if d.state == wire::BotState::Connected {
                d.state = wire::BotState::InChannel;
            }
        }
        wire::BotEventWire::Disconnected { .. } => {
            d.channel_id = None;
            d.now_playing = None;
            d.now_playing_elapsed_secs = None;
            d.last_error = None;
            clear_resolving(d);
        }
        wire::BotEventWire::JoinedChannel { channel_id } => {
            d.channel_id = Some(*channel_id);
            if d.state == wire::BotState::Connected {
                d.state = wire::BotState::InChannel;
            }
        }
        wire::BotEventWire::LeftChannel => {
            d.channel_id = None;
            if matches!(d.state, wire::BotState::InChannel | wire::BotState::Playing) {
                d.state = wire::BotState::Connected;
            }
        }
        wire::BotEventWire::NowPlaying { track } => {
            d.now_playing = Some(prefer_resolved_title(d.now_playing.as_ref(), track));
            // Keep the head of the queue in sync — callers also send a
            // `QueueChanged` right after `NowPlaying`, but applying it
            // optimistically here lets the row light up instantly.
            d.state = wire::BotState::Playing;
            // PURA-347 — a fresh track restarts the play clock at 0;
            // `Progress` ticks bump it once per second.
            d.now_playing_elapsed_secs = Some(0);
            // A fresh track supersedes any prior failure (PURA-261)
            // and any in-flight resolving chrome.
            d.last_error = None;
            clear_resolving(d);
        }
        // PURA-347 — playback-progress tick. Advance the elapsed clock
        // only while a track is playing so a stray tick can't resurrect
        // a stale value after a finish.
        wire::BotEventWire::Progress { elapsed_secs } => {
            if d.now_playing.is_some() {
                d.now_playing_elapsed_secs = Some(*elapsed_secs);
            }
        }
        wire::BotEventWire::QueueEmpty => {
            d.now_playing = None;
            d.now_playing_elapsed_secs = None;
            d.queue.clear();
            // Idle — nothing left to resolve or play.
            clear_resolving(d);
            // The lifecycle FSM doesn't carry "Playing" — collapse back
            // to a connected/in_channel state via the next StateChanged.
        }
        wire::BotEventWire::QueueChanged { current, .. } => match current {
            Some(track) => {
                d.now_playing = Some(prefer_resolved_title(d.now_playing.as_ref(), track));
            }
            None => {
                d.now_playing = None;
                d.now_playing_elapsed_secs = None;
                clear_resolving(d);
            }
        },
        // PURA-261 — audio pipeline drained. Clear `now_playing` so the
        // live view stops showing `Playing`; an auto-advance `NowPlaying`
        // for the next track arrives after this event and re-sets it.
        // A `failed: ` reason prefix means the pipeline produced no
        // audio — surface the cause as `last_error` and collapse the
        // synthesised `Playing` chip back to `InChannel` (a failed track
        // fires no `StateChanged`, so nothing else would).
        wire::BotEventWire::AudioFinished { reason } => {
            d.now_playing = None;
            d.now_playing_elapsed_secs = None;
            // THE-927 — a `failed:` AudioFinished means the resolve
            // collapsed without producing audio. Clear the pill so the
            // operator sees the error chip, not a stuck "resolving…".
            // A clean EOF after audible playback already cleared the pill
            // on `FirstFrameOnWire`, but redoing it is a no-op.
            clear_resolving(d);
            match reason.strip_prefix("failed: ") {
                Some(cause) => {
                    d.last_error = Some(cause.to_string());
                    if d.state == wire::BotState::Playing {
                        d.state = wire::BotState::InChannel;
                    }
                }
                None => d.last_error = None,
            }
        }
        // THE-927 — light the transient resolving pill while the audio
        // pipeline waits on yt-dlp + prebuffer. Replaces any previous
        // in-flight query (`!play` then `!skip` then `!play yt:` again
        // must show the newest wait). `retrying` is the automatic second
        // warm attempt from Music Bot PR #21; older events omit it.
        wire::BotEventWire::Resolving { query, retrying } => {
            d.resolving_query = Some(query.clone());
            d.resolving_retrying = *retrying;
        }
        // THE-927 — the first audible frame just shipped; clear the pill.
        wire::BotEventWire::FirstFrameOnWire => {
            clear_resolving(d);
        }
        wire::BotEventWire::Error { .. } => {
            // A bot-level error ends the wait; don't leave "resolving…"
            // up while the rest of the page is already in a failed state.
            clear_resolving(d);
        }
        wire::BotEventWire::PlaylistChanged { .. } | wire::BotEventWire::LibraryChanged => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(state: wire::BotState) -> wire::MusicBotDetail {
        wire::MusicBotDetail {
            id: wire::BotId(1),
            name: "DJ".into(),
            server_addr: "127.0.0.1:9987".into(),
            state,
            now_playing: None,
            now_playing_elapsed_secs: None,
            queue: Vec::new(),
            channel_id: None,
            last_error: None,
            resolving_query: None,
            resolving_retrying: false,
        }
    }

    fn track(id: u64, title: &str) -> wire::Track {
        wire::Track {
            id: wire::TrackId(id),
            source: wire::AudioSource::Url {
                url: "https://example.com/song.mp3".into(),
            },
            title: title.into(),
            duration_secs: Some(180),
            requested_by: None,
        }
    }

    #[test]
    fn state_changed_disconnect_clears_channel_and_now_playing() {
        let mut d = fixture(wire::BotState::InChannel);
        d.channel_id = Some(42);
        d.now_playing = Some(track(7, "Song"));
        apply_event(
            &mut d,
            &wire::BotEventWire::StateChanged {
                from: wire::BotState::InChannel,
                to: wire::BotState::Disconnecting,
            },
        );
        assert_eq!(d.state, wire::BotState::Disconnecting);
        assert!(
            d.channel_id.is_none(),
            "channel_id should clear on disconnecting"
        );
        assert!(
            d.now_playing.is_none(),
            "now_playing should clear on disconnecting"
        );
    }

    #[test]
    fn joined_channel_updates_channel_id() {
        let mut d = fixture(wire::BotState::Connected);
        apply_event(
            &mut d,
            &wire::BotEventWire::JoinedChannel { channel_id: 21 },
        );
        assert_eq!(d.channel_id, Some(21));
        assert_eq!(
            d.state,
            wire::BotState::InChannel,
            "JoinedChannel must promote Connected so transport matches the badge"
        );
    }

    #[test]
    fn connected_event_records_default_channel_as_in_channel() {
        let mut d = fixture(wire::BotState::Connected);
        apply_event(
            &mut d,
            &wire::BotEventWire::Connected {
                client_id: 1,
                default_channel: 2,
            },
        );
        assert_eq!(d.channel_id, Some(2));
        assert_eq!(d.state, wire::BotState::InChannel);
        assert!(bot_in_channel(d.state, d.channel_id));
        assert_eq!(
            state_label(effective_playback_state(d.state, d.channel_id)),
            "In channel"
        );
        assert!(
            !transport_action(effective_playback_state(d.state, d.channel_id), true, 0).disabled,
            "Pause/Resume must be enabled once a channel_id is present"
        );
    }

    #[test]
    fn state_changed_to_connected_does_not_hide_existing_channel() {
        let mut d = fixture(wire::BotState::Connecting);
        d.channel_id = Some(2);
        apply_event(
            &mut d,
            &wire::BotEventWire::StateChanged {
                from: wire::BotState::Connecting,
                to: wire::BotState::Connected,
            },
        );
        assert_eq!(d.channel_id, Some(2));
        assert_eq!(d.state, wire::BotState::InChannel);
    }

    #[test]
    fn left_channel_clears_channel_and_drops_to_connected() {
        let mut d = fixture(wire::BotState::InChannel);
        d.channel_id = Some(5);
        apply_event(&mut d, &wire::BotEventWire::LeftChannel);
        assert!(d.channel_id.is_none());
        assert_eq!(d.state, wire::BotState::Connected);
        assert!(!bot_in_channel(d.state, d.channel_id));
    }

    #[test]
    fn now_playing_keeps_resolved_title_over_url_placeholder() {
        let mut d = fixture(wire::BotState::InChannel);
        let mut named = track(0, "Never Gonna Give You Up");
        named.source = wire::AudioSource::Url {
            url: "https://www.youtube.com/watch?v=abc".into(),
        };
        apply_event(
            &mut d,
            &wire::BotEventWire::NowPlaying {
                track: named.clone(),
            },
        );
        let placeholder = wire::Track {
            id: wire::TrackId(0),
            source: named.source.clone(),
            title: "https://www.youtube.com/watch?v=abc".into(),
            duration_secs: Some(212),
            requested_by: None,
        };
        apply_event(
            &mut d,
            &wire::BotEventWire::NowPlaying { track: placeholder },
        );
        assert_eq!(
            d.now_playing.as_ref().map(|t| t.title.as_str()),
            Some("Never Gonna Give You Up")
        );
        assert_eq!(
            d.now_playing.as_ref().and_then(|t| t.duration_secs),
            Some(212),
            "later event still supplies duration / other fields"
        );
    }

    #[test]
    fn now_playing_event_promotes_state_to_playing() {
        let mut d = fixture(wire::BotState::InChannel);
        apply_event(
            &mut d,
            &wire::BotEventWire::NowPlaying {
                track: track(7, "Song"),
            },
        );
        assert_eq!(d.state, wire::BotState::Playing);
        assert_eq!(d.now_playing.as_ref().map(|t| t.id), Some(wire::TrackId(7)));
    }

    #[test]
    fn failed_audio_finish_drops_playing_and_surfaces_last_error() {
        // PURA-261 — a track that produced no audio must not keep the
        // live view stuck on `Playing`.
        let mut d = fixture(wire::BotState::InChannel);
        apply_event(
            &mut d,
            &wire::BotEventWire::NowPlaying {
                track: track(7, "bad video"),
            },
        );
        assert_eq!(d.state, wire::BotState::Playing);

        apply_event(
            &mut d,
            &wire::BotEventWire::AudioFinished {
                reason: "failed: audio pipeline produced 0 frames — check yt-dlp/ffmpeg logs"
                    .into(),
            },
        );
        assert!(
            d.now_playing.is_none(),
            "failed track must clear now_playing"
        );
        assert_ne!(d.state, wire::BotState::Playing, "must drop out of Playing");
        assert_eq!(
            d.last_error.as_deref(),
            Some("audio pipeline produced 0 frames — check yt-dlp/ffmpeg logs"),
        );
    }

    #[test]
    fn clean_audio_finish_clears_now_playing_without_error() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        apply_event(
            &mut d,
            &wire::BotEventWire::AudioFinished {
                reason: "end_of_stream".into(),
            },
        );
        assert!(d.now_playing.is_none());
        assert!(d.last_error.is_none());
    }

    #[test]
    fn queue_empty_clears_queue_and_now_playing() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        d.queue.push(track(7, "Song"));
        apply_event(&mut d, &wire::BotEventWire::QueueEmpty);
        assert!(d.now_playing.is_none());
        assert!(d.queue.is_empty());
    }

    /// THE-927 — `Resolving` lights the pill; the matching
    /// `FirstFrameOnWire` clears it. The reducer is event-driven so the
    /// FE doesn't need a client-side timer.
    #[test]
    fn resolving_lights_pill_first_frame_clears_it() {
        let mut d = fixture(wire::BotState::InChannel);
        apply_event(
            &mut d,
            &wire::BotEventWire::Resolving {
                query: "red leather last call".into(),
                retrying: false,
            },
        );
        assert_eq!(d.resolving_query.as_deref(), Some("red leather last call"));
        assert!(!d.resolving_retrying);
        apply_event(&mut d, &wire::BotEventWire::FirstFrameOnWire);
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
    }

    /// Warm-retry SSE: the same `resolving` event with `retrying: true`
    /// keeps the pill and stamps `resolvingRetrying` so Now Playing can
    /// show "resolving / retrying…".
    #[test]
    fn resolving_retrying_stamps_detail_flag() {
        let mut d = fixture(wire::BotState::InChannel);
        apply_event(
            &mut d,
            &wire::BotEventWire::Resolving {
                query: "never gonna".into(),
                retrying: true,
            },
        );
        assert_eq!(d.resolving_query.as_deref(), Some("never gonna"));
        assert!(d.resolving_retrying);
        apply_event(&mut d, &wire::BotEventWire::FirstFrameOnWire);
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
    }

    #[test]
    fn resolving_status_label_is_quiet_then_retrying() {
        assert_eq!(resolving_status_label(false), "resolving…");
        assert_eq!(resolving_status_label(true), "resolving / retrying…");
    }

    #[test]
    fn now_playing_clears_resolving_chrome() {
        let mut d = fixture(wire::BotState::InChannel);
        d.resolving_query = Some("never gonna".into());
        d.resolving_retrying = true;
        apply_event(
            &mut d,
            &wire::BotEventWire::NowPlaying {
                track: track(7, "Song"),
            },
        );
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
        assert_eq!(d.state, wire::BotState::Playing);
    }

    #[test]
    fn error_and_idle_events_clear_resolving_chrome() {
        let mut d = fixture(wire::BotState::InChannel);
        d.resolving_query = Some("query".into());
        d.resolving_retrying = true;
        apply_event(
            &mut d,
            &wire::BotEventWire::Error {
                message: "extractor failed".into(),
            },
        );
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);

        d.resolving_query = Some("query".into());
        d.resolving_retrying = true;
        apply_event(&mut d, &wire::BotEventWire::QueueEmpty);
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
    }

    /// THE-927 — a second `Resolving` replaces the in-flight query
    /// (operator skipped and queued a new yt: search; the pill must show
    /// the newest wait, not the abandoned one).
    #[test]
    fn second_resolving_supersedes_the_first() {
        let mut d = fixture(wire::BotState::InChannel);
        apply_event(
            &mut d,
            &wire::BotEventWire::Resolving {
                query: "first".into(),
                retrying: false,
            },
        );
        apply_event(
            &mut d,
            &wire::BotEventWire::Resolving {
                query: "second".into(),
                retrying: false,
            },
        );
        assert_eq!(d.resolving_query.as_deref(), Some("second"));
        assert!(!d.resolving_retrying);
    }

    /// THE-927 — a failed pipeline (`AudioFinished` with a `failed:`
    /// reason) clears the pill so the operator sees the error chip
    /// rather than a stuck "Resolving…" while a real failure is shown.
    #[test]
    fn failed_audio_finished_clears_resolving_pill() {
        let mut d = fixture(wire::BotState::Playing);
        d.resolving_query = Some("query".into());
        d.resolving_retrying = true;
        apply_event(
            &mut d,
            &wire::BotEventWire::AudioFinished {
                reason: "failed: yt-dlp produced 0 frames".into(),
            },
        );
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
        assert_eq!(d.last_error.as_deref(), Some("yt-dlp produced 0 frames"));
    }

    /// THE-927 — disconnect mid-resolve abandons the wait; the pill
    /// can't pretend resolution is still in flight when the bot left.
    #[test]
    fn disconnect_during_resolve_drops_the_pill() {
        let mut d = fixture(wire::BotState::InChannel);
        d.resolving_query = Some("query".into());
        d.resolving_retrying = true;
        apply_event(
            &mut d,
            &wire::BotEventWire::StateChanged {
                from: wire::BotState::InChannel,
                to: wire::BotState::Disconnected,
            },
        );
        assert!(d.resolving_query.is_none());
        assert!(!d.resolving_retrying);
    }

    #[test]
    fn library_and_playlist_changes_dont_clobber_snapshot() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        let before = d.clone();
        apply_event(&mut d, &wire::BotEventWire::LibraryChanged);
        apply_event(
            &mut d,
            &wire::BotEventWire::PlaylistChanged {
                name: "lo-fi".into(),
            },
        );
        assert_eq!(d, before);
    }

    #[test]
    fn transport_playing_offers_pause() {
        let t = transport_action(wire::BotState::Playing, true, 3);
        assert_eq!(t.action, AudioAction::Pause);
        assert!(!t.disabled);
        assert_eq!(t.aria, "Pause");
    }

    #[test]
    fn transport_in_channel_with_track_offers_resume() {
        let t = transport_action(wire::BotState::InChannel, true, 0);
        assert_eq!(t.action, AudioAction::Resume);
        assert!(!t.disabled);
        assert_eq!(t.aria, "Resume");
    }

    #[test]
    fn transport_in_channel_no_track_but_queue_offers_play() {
        let t = transport_action(wire::BotState::InChannel, false, 2);
        assert_eq!(t.action, AudioAction::Resume);
        assert!(!t.disabled);
        assert_eq!(t.label, "▶ Play");
    }

    #[test]
    fn transport_in_channel_empty_queue_disables_play() {
        let t = transport_action(wire::BotState::InChannel, false, 0);
        assert!(t.disabled);
        assert_eq!(t.aria, "Play (queue is empty)");
    }

    #[test]
    fn transport_disconnected_disables_play() {
        for state in [
            wire::BotState::Disconnected,
            wire::BotState::Connecting,
            wire::BotState::Connected,
            wire::BotState::Disconnecting,
        ] {
            assert!(transport_action(state, false, 5).disabled, "{state:?}");
        }
    }

    #[test]
    fn optimistic_pause_drops_playing_to_in_channel() {
        let mut d = fixture(wire::BotState::Playing);
        apply_optimistic(&mut d, AudioAction::Pause);
        assert_eq!(d.state, wire::BotState::InChannel);
    }

    #[test]
    fn optimistic_resume_promotes_in_channel_to_playing() {
        let mut d = fixture(wire::BotState::InChannel);
        apply_optimistic(&mut d, AudioAction::Resume);
        assert_eq!(d.state, wire::BotState::Playing);
    }

    #[test]
    fn optimistic_skip_leaves_state_untouched() {
        let mut d = fixture(wire::BotState::Playing);
        apply_optimistic(&mut d, AudioAction::SkipNext);
        assert_eq!(d.state, wire::BotState::Playing);
    }

    #[test]
    fn reorder_swap_moves_a_row_up() {
        let q = vec![track(1, "a"), track(2, "b"), track(3, "c")];
        let out = reorder_swap(&q, 1, 0);
        let ids: Vec<u64> = out.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 1, 3]);
    }

    #[test]
    fn reorder_swap_out_of_range_is_noop() {
        let q = vec![track(1, "a"), track(2, "b")];
        // first-row `↑` would compute target `usize::MAX` via `idx - 1`
        // wrapping — the disabled button shouldn't fire, but guard anyway.
        let out = reorder_swap(&q, 0, usize::MAX);
        assert_eq!(out, q);
    }

    #[test]
    fn track_progress_is_elapsed_over_duration() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song")); // duration_secs: Some(180)
        d.now_playing_elapsed_secs = Some(90);
        assert_eq!(track_progress(&d), Some(0.5));
    }

    #[test]
    fn track_progress_clamps_overrun_to_one() {
        // A `Progress` tick can land past the reported duration (the
        // duration is an estimate); the fill must not overflow its track.
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        d.now_playing_elapsed_secs = Some(900);
        assert_eq!(track_progress(&d), Some(1.0));
    }

    #[test]
    fn track_progress_none_without_duration() {
        // ICY radio / id=0 tracks carry no duration — the caller falls
        // back to the equalizer rather than a bar it can't fill.
        let mut d = fixture(wire::BotState::Playing);
        let mut t = track(0, "Live radio");
        t.duration_secs = None;
        d.now_playing = Some(t);
        d.now_playing_elapsed_secs = Some(42);
        assert_eq!(track_progress(&d), None);
    }

    #[test]
    fn track_progress_none_without_play_clock() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        d.now_playing_elapsed_secs = None;
        assert_eq!(track_progress(&d), None);
    }

    #[test]
    fn track_progress_none_for_zero_duration() {
        let mut d = fixture(wire::BotState::Playing);
        let mut t = track(7, "Song");
        t.duration_secs = Some(0);
        d.now_playing = Some(t);
        d.now_playing_elapsed_secs = Some(0);
        assert_eq!(track_progress(&d), None);
    }

    #[test]
    fn track_progress_none_without_now_playing() {
        let d = fixture(wire::BotState::InChannel);
        assert_eq!(track_progress(&d), None);
    }
}
