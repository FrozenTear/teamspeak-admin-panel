//! `/music-bots/:bot_id/radio` — radio-station presets.
//!
//! Radio stations are library entries carrying the `radio` marker tag —
//! the WS-5 server keeps the same id space so /radio-stations and
//! /music-library agree. The preset list shows title + source plus a
//! one-shot Play button that dispatches `BotCommand::Audio(Play)`
//! directly (bypassing the queue per the WS-5 contract).

use dioxus::prelude::*;
use ts6_manager_shared::music_bots as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::music_bots as mb;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::pages::music_bots::shared::{
    audio_source_summary, format_error, parse_audio_source,
};
use crate::ui::routes::Route;

#[component]
pub fn RadioStationsPage(bot_id: u64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut rows: Signal<Vec<wire::RadioStation>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { mb::list_radio_stations(gate, bot).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(list)) => {
            rows.set(list.clone());
            error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    let bump = move || reload.with_mut(|n| *n += 1);

    let mut new_title: Signal<String> = use_signal(String::new);
    let mut new_url: Signal<String> = use_signal(String::new);
    let mut new_tags: Signal<String> = use_signal(String::new);
    let mut creating: Signal<bool> = use_signal(|| false);

    let on_create = {
        let gate = gate.clone();
        let mut bump = bump;
        move |evt: FormEvent| {
            evt.prevent_default();
            if *creating.read() {
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
            // Comma-separated extra tags. The server always adds the
            // `radio` marker, so we filter empty fragments and skip
            // duplicates of `radio` here.
            let tags: Vec<String> = new_tags
                .read()
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty() && t != wire::RADIO_TAG)
                .collect();
            creating.set(true);
            let gate = gate.clone();
            spawn(async move {
                match mb::create_radio_station(gate, bot, source, title.clone(), tags).await {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Added \u{201C}{title}\u{201D}"),
                            None,
                        );
                        new_title.set(String::new());
                        new_url.set(String::new());
                        new_tags.set(String::new());
                        bump();
                    }
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Add failed", Some(format_error(&e)))
                    }
                }
                creating.set(false);
            });
        }
    };

    let on_play = {
        let gate = gate.clone();
        move |id: wire::LibraryEntryId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::play_radio_station(gate, bot, id).await {
                    Ok(()) => toaster.push(ToastVariant::Success, "Playing", None),
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Play failed", Some(format_error(&e)))
                    }
                }
            });
        }
    };

    let on_delete = {
        let gate = gate.clone();
        let mut bump = bump;
        move |id: wire::LibraryEntryId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::delete_radio_station(gate, bot, id).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Removed", None);
                        bump();
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

    rsx! {
        div { class: "crumb",
            Link { to: Route::BotsIndexPage {}, "Music bots" }
            " · "
            Link { to: Route::BotDetailPage { bot_id }, "Bot {bot.0}" }
            " · Radio"
        }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Radio stations" }
                p { class: "page-lede", "Saved presets that play directly without going through the queue." }
            }
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load stations".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            form { class: "card", onsubmit: on_create,
                h3 { "Add station" }
                div { class: "card-row",
                    input {
                        class: "input",
                        placeholder: "Title (e.g. Lo-fi Beats)",
                        value: "{new_title.read()}",
                        oninput: move |e| new_title.set(e.value()),
                    }
                    input {
                        class: "input",
                        placeholder: "URL or library:path",
                        value: "{new_url.read()}",
                        oninput: move |e| new_url.set(e.value()),
                    }
                    input {
                        class: "input",
                        placeholder: "Extra tags (comma-separated)",
                        value: "{new_tags.read()}",
                        oninput: move |e| new_tags.set(e.value()),
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *creating.read(),
                        "Add"
                    }
                }
            }

            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading stations…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", "📻" }
                    h3 { "No stations yet" }
                    p { "Add a preset above to give the bot a one-click radio source." }
                }
            } else {
                table { class: "data-table",
                    "aria-label": "Radio stations",
                    thead {
                        tr {
                            th { scope: "col", "Title" }
                            th { scope: "col", "Source" }
                            th { scope: "col", "Tags" }
                            th { scope: "col", class: "actions-col", "Actions" }
                        }
                    }
                    tbody {
                        for station in rows.read().iter() {
                            {
                                let station = station.clone();
                                let id = station.id;
                                let extra_tags: Vec<String> = station
                                    .tags
                                    .iter()
                                    .filter(|t| t.as_str() != wire::RADIO_TAG)
                                    .cloned()
                                    .collect();
                                let on_play = on_play.clone();
                                let on_delete = on_delete.clone();
                                rsx! {
                                    tr { key: "{id.0}",
                                        td { "{station.title}" }
                                        td { "{audio_source_summary(&station.source)}" }
                                        td {
                                            if extra_tags.is_empty() {
                                                span { class: "muted", "—" }
                                            } else {
                                                for t in extra_tags.iter() {
                                                    span { class: "tag", "{t}" }
                                                }
                                            }
                                        }
                                        td { class: "actions-col",
                                            Button {
                                                variant: ButtonVariant::Primary,
                                                size: ButtonSize::Small,
                                                onclick: move |_| on_play(id),
                                                "Play"
                                            }
                                            Button {
                                                variant: ButtonVariant::Danger,
                                                size: ButtonSize::Small,
                                                onclick: move |_| on_delete(id),
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
    }
}
