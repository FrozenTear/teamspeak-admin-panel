//! `/music-bots/:bot_id` — per-bot operator surface.
//!
//! Renders a single bot's connection state, channel, now-playing card,
//! and queue snapshot. Subscribes to the SSE event stream so reductions
//! land without polling — the underlying [`web_sys::EventSource`] also
//! handles reconnect + last-event-id for free.
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
    audio_source_summary, format_duration, format_error, state_badge_class, state_label,
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
    // reflects state changes without polling. Stored inside an `Rc<RefCell>`
    // owned by `use_hook` so the underlying `EventSource` closes on
    // unmount (the `BotEventStream`'s `Drop` impl does the work; the Rc
    // wrapper just gives `use_hook` a `Clone` handle to stash). Lag is
    // handled by the browser's native reconnect; library / playlist
    // events trigger a refetch because the snapshot doesn't carry that
    // data inline.
    use_hook(|| {
        let stream = mb::open_bot_event_source(bot, move |ev| {
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
        std::rc::Rc::new(std::cell::RefCell::new(Some(stream)))
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
    let on_join = {
        let gate = gate.clone();
        let mut bump = bump;
        move |evt: FormEvent| {
            evt.prevent_default();
            let raw = join_input.read().trim().to_string();
            let Ok(channel_id) = raw.parse::<u64>() else {
                toaster.push(
                    ToastVariant::Warning,
                    "Channel id required",
                    Some("Enter the numeric channel id to join.".into()),
                );
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
                        span { class: state_badge_class(d.state),
                            "{state_label(d.state)}"
                        }
                        " · {d.server_addr}"
                        if let Some(cid) = d.channel_id {
                            " · channel {cid}"
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
                            "Join channel"
                        }
                        Button {
                            variant: ButtonVariant::Ghost,
                            size: ButtonSize::Small,
                            onclick: on_leave,
                            "Leave"
                        }
                    }
                }

                div { class: "card",
                    h3 { "Now playing" }
                    if let Some(track) = d.now_playing.as_ref() {
                        div { class: "now-playing",
                            div { class: "now-playing-title", "{track.title}" }
                            div { class: "now-playing-source", "{audio_source_summary(&track.source)}" }
                            div { class: "now-playing-meta",
                                "{format_duration(track.duration_secs)}"
                                if let Some(by) = track.requested_by.as_deref() {
                                    " · requested by {by}"
                                }
                            }
                        }
                    } else {
                        p { class: "muted", "Nothing is playing." }
                    }
                    PlaybackControls {
                        bot_id,
                        state: d.state,
                        has_track: d.now_playing.is_some(),
                    }
                }

                div { class: "card",
                    div { class: "card-header",
                        h3 { "Queue" }
                        span { class: "muted", "{d.queue.len()} tracks" }
                    }
                    if d.queue.is_empty() {
                        p { class: "muted",
                            "Queue is empty. Enqueue tracks from the "
                            Link { to: Route::MusicPlaylistsPage { bot_id }, "Playlists" }
                            " page."
                        }
                    } else {
                        QueueList { tracks: d.queue.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn PlaybackControls(bot_id: u64, state: wire::BotState, has_track: bool) -> Element {
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    // Audio commands are meaningless when the bot isn't on a server — the
    // backend would 404 the dispatch with `bot not found` (translated by
    // `translate_send_error` for an `ActorGone` actor). Disable the row
    // wholesale so the chrome reflects what the operator can actually do.
    let in_channel = matches!(state, wire::BotState::InChannel | wire::BotState::Playing);

    let dispatch = {
        let gate = gate.clone();
        move |kind: AudioAction| {
            let gate = gate.clone();
            let toaster = toaster;
            spawn(async move {
                let res = match kind {
                    AudioAction::SkipPrev => mb::skip_prev(gate, bot).await,
                    AudioAction::Resume => mb::resume_bot(gate, bot).await,
                    AudioAction::Pause => mb::pause_bot(gate, bot).await,
                    AudioAction::SkipNext => mb::skip_next(gate, bot).await,
                    AudioAction::Stop => mb::stop_bot(gate, bot).await,
                };
                match res {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, kind.success_label(), None);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            kind.failure_label(),
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    let on_skip_prev = {
        let dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::SkipPrev)
    };
    let on_resume = {
        let dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::Resume)
    };
    let on_pause = {
        let dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::Pause)
    };
    let on_skip_next = {
        let dispatch = dispatch.clone();
        move |_| dispatch(AudioAction::SkipNext)
    };
    let on_stop = {
        let dispatch = dispatch;
        move |_| dispatch(AudioAction::Stop)
    };

    rsx! {
        div { class: "playback-controls",
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: !in_channel,
                onclick: on_skip_prev,
                "« Prev"
            }
            Button {
                variant: ButtonVariant::Primary,
                size: ButtonSize::Small,
                disabled: !in_channel || !has_track,
                onclick: on_resume,
                "Play"
            }
            Button {
                variant: ButtonVariant::Secondary,
                size: ButtonSize::Small,
                disabled: !in_channel || !has_track,
                onclick: on_pause,
                "Pause"
            }
            Button {
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Small,
                disabled: !in_channel,
                onclick: on_skip_next,
                "Skip »"
            }
            Button {
                variant: ButtonVariant::Danger,
                size: ButtonSize::Small,
                disabled: !in_channel || !has_track,
                onclick: on_stop,
                "Stop"
            }
        }
        if !in_channel {
            p { class: "muted small",
                "Bot must be in a channel to control playback. Connect and join a channel first."
            }
        }
    }
}

#[derive(Clone, Copy)]
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

#[derive(Props, Clone, PartialEq)]
struct QueueListProps {
    tracks: Vec<wire::Track>,
}

#[component]
fn QueueList(props: QueueListProps) -> Element {
    rsx! {
        ol { class: "queue-list",
            for (idx, track) in props.tracks.iter().enumerate() {
                {
                    let track = track.clone();
                    let pos = idx + 1;
                    rsx! {
                        li { key: "{track.id.0}", class: "queue-item",
                            span { class: "queue-pos", "{pos}." }
                            div { class: "queue-meta",
                                span { class: "queue-title", "{track.title}" }
                                span { class: "queue-source", "{audio_source_summary(&track.source)}" }
                            }
                            span { class: "queue-duration", "{format_duration(track.duration_secs)}" }
                        }
                    }
                }
            }
        }
    }
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
            }
        }
        wire::BotEventWire::Connected {
            default_channel, ..
        } => {
            d.channel_id = Some(*default_channel);
        }
        wire::BotEventWire::Disconnected { .. } => {
            d.channel_id = None;
            d.now_playing = None;
        }
        wire::BotEventWire::JoinedChannel { channel_id } => {
            d.channel_id = Some(*channel_id);
        }
        wire::BotEventWire::LeftChannel => {
            d.channel_id = None;
        }
        wire::BotEventWire::NowPlaying { track } => {
            d.now_playing = Some(track.clone());
            // Keep the head of the queue in sync — callers also send a
            // `QueueChanged` right after `NowPlaying`, but applying it
            // optimistically here lets the row light up instantly.
            d.state = wire::BotState::Playing;
        }
        wire::BotEventWire::QueueEmpty => {
            d.now_playing = None;
            d.queue.clear();
            // The lifecycle FSM doesn't carry "Playing" — collapse back
            // to a connected/in_channel state via the next StateChanged.
        }
        wire::BotEventWire::QueueChanged { current, .. } => {
            d.now_playing = current.clone();
        }
        // PURA-154 — audio pipeline drained. The auto-advance flow
        // pairs this with a NowPlaying (next track) or QueueEmpty, so
        // we don't need to mutate the snapshot here.
        wire::BotEventWire::AudioFinished { .. }
        | wire::BotEventWire::Error { .. }
        | wire::BotEventWire::PlaylistChanged { .. }
        | wire::BotEventWire::LibraryChanged => {}
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
            queue: Vec::new(),
            channel_id: None,
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
    fn queue_empty_clears_queue_and_now_playing() {
        let mut d = fixture(wire::BotState::Playing);
        d.now_playing = Some(track(7, "Song"));
        d.queue.push(track(7, "Song"));
        apply_event(&mut d, &wire::BotEventWire::QueueEmpty);
        assert!(d.now_playing.is_none());
        assert!(d.queue.is_empty());
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
}
