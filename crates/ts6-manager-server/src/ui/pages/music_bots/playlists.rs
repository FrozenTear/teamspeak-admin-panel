//! `/music-bots/:bot_id/playlists` — playlist CRUD + enqueue.
//!
//! Two-pane layout: left side lists playlists with create/delete; right
//! side shows the selected playlist's tracks with add-track and
//! enqueue-to-bot actions. Drag-to-reorder is deferred to a follow-up
//! once the WS-5 surface exposes a track-reorder endpoint (the WS-3
//! store has the primitive but the route layer doesn't surface it).

use dioxus::prelude::*;
use ts6_manager_shared::music_bots as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::music_bots as mb;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant,
};
use crate::ui::pages::music_bots::shared::{
    audio_source_summary, format_duration, format_error, parse_audio_source,
};
use crate::ui::routes::Route;

#[component]
pub fn MusicPlaylistsPage(bot_id: u64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut playlists: Signal<Vec<wire::PlaylistSummary>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut selected: Signal<Option<String>> = use_signal(|| None::<String>);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { mb::list_playlists(gate, bot).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(list)) => {
            playlists.set(list.clone());
            error.set(None);
            loading.set(false);
            // Default-select the first playlist when nothing was picked.
            if selected.read().is_none() {
                if let Some(first) = list.first() {
                    selected.set(Some(first.name.clone()));
                }
            }
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    let bump = move || reload.with_mut(|n| *n += 1);

    let mut new_name: Signal<String> = use_signal(String::new);
    let mut creating: Signal<bool> = use_signal(|| false);

    let on_create = {
        let gate = gate.clone();
        let toaster = toaster;
        let mut bump = bump;
        move |evt: FormEvent| {
            evt.prevent_default();
            if *creating.read() {
                return;
            }
            let name = new_name.read().trim().to_string();
            if name.is_empty() {
                return;
            }
            creating.set(true);
            let gate = gate.clone();
            let name_for_select = name.clone();
            spawn(async move {
                match mb::create_playlist(gate, bot, &name).await {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Created \u{201C}{name}\u{201D}"),
                            None,
                        );
                        new_name.set(String::new());
                        selected.set(Some(name_for_select));
                        bump();
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Create failed",
                        Some(format_error(&e)),
                    ),
                }
                creating.set(false);
            });
        }
    };

    let on_delete = {
        let gate = gate.clone();
        let toaster = toaster;
        let mut bump = bump;
        move |name: String| {
            let gate = gate.clone();
            spawn(async move {
                match mb::delete_playlist(gate, bot, &name).await {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Deleted \u{201C}{name}\u{201D}"),
                            None,
                        );
                        // Drop selection if the playlist we removed was open.
                        selected.with_mut(|s| {
                            if s.as_deref() == Some(&name) {
                                *s = None;
                            }
                        });
                        bump();
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Delete failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        div { class: "crumb",
            Link { to: Route::BotsIndexPage {}, "Music bots" }
            " · "
            Link { to: Route::BotDetailPage { bot_id }, "Bot {bot.0}" }
            " · Playlists"
        }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Playlists" }
                p { class: "page-lede", "Group tracks for this bot. Enqueue a whole playlist to the bot's queue with one click." }
            }
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load playlists".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md two-col",
            div { class: "card",
                h3 { "Playlists" }
                form { class: "card-row", onsubmit: on_create,
                    input {
                        class: "input",
                        placeholder: "New playlist name",
                        value: "{new_name.read()}",
                        oninput: move |e| new_name.set(e.value()),
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *creating.read(),
                        "Create"
                    }
                }
                if *loading.read() && playlists.read().is_empty() {
                    p { class: "muted", "Loading…" }
                } else if playlists.read().is_empty() {
                    p { class: "muted", "No playlists yet — create one above." }
                } else {
                    ul { class: "side-list",
                        for p in playlists.read().iter() {
                            {
                                let p = p.clone();
                                let name = p.name.clone();
                                let name_for_click = name.clone();
                                let name_for_delete = name.clone();
                                let on_delete = on_delete.clone();
                                let is_selected = selected
                                    .read()
                                    .as_deref()
                                    .map(|s| s == name)
                                    .unwrap_or(false);
                                let cls = if is_selected { "side-list-item is-active" } else { "side-list-item" };
                                rsx! {
                                    li { key: "{name}", class: "{cls}",
                                        button {
                                            r#type: "button",
                                            class: "side-list-link",
                                            onclick: move |_| selected.set(Some(name_for_click.clone())),
                                            span { class: "side-list-name", "{p.name}" }
                                            span { class: "side-list-meta", "{p.track_count}" }
                                        }
                                        Button {
                                            variant: ButtonVariant::Danger,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_delete(name_for_delete.clone()),
                                            "Delete"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if let Some(name) = selected.read().clone() {
                PlaylistDetailPane {
                    bot: bot,
                    name: name.clone(),
                    on_changed: EventHandler::new({
                        let mut bump = bump;
                        move |_: ()| bump()
                    }),
                }
            } else {
                div { class: "card",
                    h3 { "No playlist selected" }
                    p { class: "muted", "Pick a playlist to view its tracks." }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PlaylistDetailPaneProps {
    bot: wire::BotId,
    name: String,
    on_changed: EventHandler<()>,
}

#[component]
fn PlaylistDetailPane(props: PlaylistDetailPaneProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let bot = props.bot;
    let name = props.name.clone();
    let name_for_resource = name.clone();
    let on_changed = props.on_changed;

    let mut detail: Signal<Option<wire::PlaylistDetail>> = use_signal(|| None);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let snapshot = use_resource({
        let gate = gate.clone();
        let name = name_for_resource.clone();
        move || {
            let gate = gate.clone();
            let name = name.clone();
            let _ = *reload.read();
            async move { mb::get_playlist(gate, bot, &name).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(d)) => {
            detail.set(Some(d.clone()));
            error.set(None);
        }
        Some(Err(e)) => error.set(Some(e.clone())),
        None => {}
    });

    let bump_local = move || reload.with_mut(|n| *n += 1);

    let mut new_url: Signal<String> = use_signal(String::new);
    let mut new_title: Signal<String> = use_signal(String::new);
    let mut adding: Signal<bool> = use_signal(|| false);

    let on_add = {
        let gate = gate.clone();
        let toaster = toaster;
        let name_for_add = name.clone();
        let on_changed = on_changed;
        let mut bump_local = bump_local;
        move |evt: FormEvent| {
            evt.prevent_default();
            if *adding.read() {
                return;
            }
            let title = new_title.read().trim().to_string();
            let url = new_url.read().clone();
            let Some(source) = parse_audio_source(&url) else {
                toaster.push(
                    ToastVariant::Warning,
                    "Source required",
                    Some("Enter a URL or library:relative-path source.".into()),
                );
                return;
            };
            if title.is_empty() {
                toaster.push(ToastVariant::Warning, "Title required", None);
                return;
            }
            adding.set(true);
            let gate = gate.clone();
            let playlist = name_for_add.clone();
            spawn(async move {
                match mb::add_playlist_track(gate, bot, &playlist, &source, &title).await {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Track added", None);
                        new_url.set(String::new());
                        new_title.set(String::new());
                        bump_local();
                        on_changed.call(());
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Add failed",
                        Some(format_error(&e)),
                    ),
                }
                adding.set(false);
            });
        }
    };

    let on_remove_track = {
        let gate = gate.clone();
        let toaster = toaster;
        let name_for_remove = name.clone();
        let on_changed = on_changed;
        let mut bump_local = bump_local;
        move |track: wire::TrackId| {
            let gate = gate.clone();
            let playlist = name_for_remove.clone();
            spawn(async move {
                match mb::remove_playlist_track(gate, bot, &playlist, track).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Track removed", None);
                        bump_local();
                        on_changed.call(());
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Remove failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let on_enqueue = {
        let gate = gate.clone();
        let toaster = toaster;
        let name_for_enqueue = name.clone();
        move |_| {
            let gate = gate.clone();
            let playlist = name_for_enqueue.clone();
            spawn(async move {
                match mb::enqueue_playlist(gate, bot, &playlist).await {
                    Ok(d) => toaster.push(
                        ToastVariant::Success,
                        format!("Enqueued {} tracks", d.tracks.len()),
                        None,
                    ),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Enqueue failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        div { class: "card",
            div { class: "card-header",
                h3 { "{name}" }
                Button {
                    variant: ButtonVariant::Primary,
                    size: ButtonSize::Small,
                    onclick: on_enqueue,
                    "Enqueue to bot"
                }
            }

            if let Some(err) = error.read().as_ref() {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load playlist".to_string(),
                    "{format_error(err)}"
                }
            }

            form { class: "card-row", onsubmit: on_add,
                input {
                    class: "input",
                    placeholder: "Track title",
                    value: "{new_title.read()}",
                    oninput: move |e| new_title.set(e.value()),
                }
                input {
                    class: "input",
                    placeholder: "URL or library:relative/path.mp3",
                    value: "{new_url.read()}",
                    oninput: move |e| new_url.set(e.value()),
                }
                Button {
                    variant: ButtonVariant::Primary,
                    kind: ButtonType::Submit,
                    loading: *adding.read(),
                    "Add track"
                }
            }

            if let Some(d) = detail.read().clone() {
                if d.tracks.is_empty() {
                    p { class: "muted", "No tracks yet." }
                } else {
                    ol { class: "queue-list",
                        for (idx, track) in d.tracks.iter().enumerate() {
                            {
                                let track = track.clone();
                                let track_id = track.id;
                                let pos = idx + 1;
                                let on_remove = on_remove_track.clone();
                                rsx! {
                                    li { key: "{track_id.0}", class: "queue-item",
                                        span { class: "queue-pos", "{pos}." }
                                        div { class: "queue-meta",
                                            span { class: "queue-title", "{track.title}" }
                                            span { class: "queue-source",
                                                "{audio_source_summary(&track.source)}"
                                            }
                                        }
                                        span { class: "queue-duration",
                                            "{format_duration(track.duration_secs)}"
                                        }
                                        Button {
                                            variant: ButtonVariant::Ghost,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_remove(track_id),
                                            "Remove"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                p { class: "muted", "Loading…" }
            }
        }
    }
}
