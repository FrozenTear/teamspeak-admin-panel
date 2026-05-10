//! `/music-bots/:bot_id/library` — per-bot saved sources.
//!
//! Library entries are persistent audio sources the operator wants to
//! reach for repeatedly. The page exposes search (free-text title
//! filter, client-side), add-by-URL, and delete; tagging the entry as
//! `radio` is left to the radio-stations page so the marker tag stays
//! the entry point for the radio surface.

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
    audio_source_summary, format_error, parse_audio_source,
};
use crate::ui::routes::Route;

#[component]
pub fn MusicLibraryPage(bot_id: u64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let bot = wire::BotId(bot_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut rows: Signal<Vec<wire::LibraryEntry>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut filter: Signal<String> = use_signal(String::new);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { mb::list_library(gate, bot, None).await }
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

    let mut new_url: Signal<String> = use_signal(String::new);
    let mut new_title: Signal<String> = use_signal(String::new);
    let mut adding: Signal<bool> = use_signal(|| false);

    let on_add = {
        let gate = gate.clone();
        let toaster = toaster;
        let mut bump = bump;
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
            spawn(async move {
                match mb::add_library_entry(gate, bot, &source, &title, &[]).await {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, format!("Added \u{201C}{title}\u{201D}"), None);
                        new_url.set(String::new());
                        new_title.set(String::new());
                        bump();
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

    let on_delete = {
        let gate = gate.clone();
        let toaster = toaster;
        let mut bump = bump;
        move |id: wire::LibraryEntryId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::delete_library_entry(gate, bot, id).await {
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

    let filtered: Vec<wire::LibraryEntry> = {
        let needle = filter.read().to_lowercase();
        rows.read()
            .iter()
            .filter(|e| {
                if needle.is_empty() {
                    return true;
                }
                e.title.to_lowercase().contains(&needle)
                    || audio_source_summary(&e.source).to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
    };

    rsx! {
        div { class: "crumb",
            Link { to: Route::BotsIndexPage {}, "Music bots" }
            " · "
            Link { to: Route::BotDetailPage { bot_id }, "Bot {bot.0}" }
            " · Library"
        }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Library" }
                p { class: "page-lede", "Saved audio sources for this bot. Tag entries as ‘radio’ to expose them on the Radio page." }
            }
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load library".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            form { class: "card", onsubmit: on_add,
                h3 { "Add entry" }
                div { class: "card-row",
                    input {
                        class: "input",
                        placeholder: "Title (e.g. Lo-fi study mix)",
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
                        "Add"
                    }
                }
            }

            div { class: "card-row",
                input {
                    class: "input",
                    placeholder: "Filter by title or URL",
                    value: "{filter.read()}",
                    oninput: move |e| filter.set(e.value()),
                }
            }

            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading library…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", "♪" }
                    h3 { "Library is empty" }
                    p { "Save a source above to reuse it across playlists and radio stations." }
                }
            } else if filtered.is_empty() {
                div { class: "empty",
                    div { class: "icon", "○" }
                    h3 { "No matches" }
                    p { "Try a different search term, or clear the filter." }
                }
            } else {
                table { class: "data-table",
                    "aria-label": "Library entries",
                    thead {
                        tr {
                            th { scope: "col", "Title" }
                            th { scope: "col", "Source" }
                            th { scope: "col", "Tags" }
                            th { scope: "col", class: "actions-col", "Actions" }
                        }
                    }
                    tbody {
                        for entry in filtered.iter() {
                            {
                                let entry = entry.clone();
                                let id = entry.id;
                                let on_delete = on_delete.clone();
                                rsx! {
                                    tr { key: "{id.0}",
                                        td { "{entry.title}" }
                                        td { "{audio_source_summary(&entry.source)}" }
                                        td {
                                            if entry.tags.is_empty() {
                                                span { class: "muted", "—" }
                                            } else {
                                                for t in entry.tags.iter() {
                                                    span { class: "tag", "{t}" }
                                                }
                                            }
                                        }
                                        td { class: "actions-col",
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
