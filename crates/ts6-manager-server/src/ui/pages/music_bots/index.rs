//! `/music-bots` — bot index page.
//!
//! Lists every bot the supervisor knows about, with state badges and the
//! lifecycle actions an operator needs in one click: spawn a new bot,
//! connect / disconnect, and delete (clean shutdown). The detail page
//! (one click into a row) covers per-bot queue management.

use dioxus::prelude::*;
use ts6_manager_shared::music_bots as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::music_bots as mb;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::pages::music_bots::shared::{format_error, state_badge_class, state_label};
use crate::ui::routes::Route;

#[component]
pub fn BotsIndexPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut rows: Signal<Vec<wire::MusicBotSummary>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut show_create: Signal<bool> = use_signal(|| false);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { mb::list_bots(gate).await }
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

    let on_connect = {
        let gate = gate.clone();
        move |bot: wire::BotId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::connect_bot(gate, bot).await {
                    Ok(()) => toaster.push(
                        ToastVariant::Success,
                        format!("Connecting bot {}", bot.0),
                        None,
                    ),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Connect failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let on_disconnect = {
        let gate = gate.clone();
        move |bot: wire::BotId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::disconnect_bot(gate, bot).await {
                    Ok(()) => toaster.push(
                        ToastVariant::Success,
                        format!("Disconnecting bot {}", bot.0),
                        None,
                    ),
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Disconnect failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let on_delete = {
        let gate = gate.clone();
        let mut bump = bump;
        move |bot: wire::BotId| {
            let gate = gate.clone();
            spawn(async move {
                match mb::delete_bot(gate, bot).await {
                    Ok(()) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Deleted bot {}", bot.0),
                            None,
                        );
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
        div { class: "crumb", "Music bots" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Music bots" }
                p { class: "page-lede",
                    "Spawn music bots, connect them to a TS6 server, and drive playback. Each bot owns its own queue, library, playlists, and radio presets."
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: move |_| show_create.set(true),
                    "+ New bot"
                }
            }
        }

        if let Some(err) = error.read().as_ref() {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load bots".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading bots…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", "♪" }
                    h3 { "No music bots yet" }
                    p { "Create a bot to start streaming audio into a TS6 channel." }
                    div { class: "actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: move |_| show_create.set(true),
                            "+ New bot"
                        }
                    }
                }
            } else {
                BotsTable {
                    rows: rows.read().clone(),
                    on_connect: EventHandler::new({
                        let on_connect = on_connect.clone();
                        move |id: wire::BotId| on_connect(id)
                    }),
                    on_disconnect: EventHandler::new({
                        let on_disconnect = on_disconnect.clone();
                        move |id: wire::BotId| on_disconnect(id)
                    }),
                    on_delete: EventHandler::new({
                        let on_delete = on_delete.clone();
                        move |id: wire::BotId| on_delete(id)
                    }),
                }
            }
        }

        if *show_create.read() {
            CreateBotModal {
                on_close: EventHandler::new(move |_: ()| show_create.set(false)),
                on_created: EventHandler::new({
                    let mut show_create = show_create;
                    let mut bump = bump;
                    move |s: wire::MusicBotSummary| {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Created bot \u{201C}{}\u{201D}", s.name),
                            None,
                        );
                        show_create.set(false);
                        bump();
                    }
                }),
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct BotsTableProps {
    rows: Vec<wire::MusicBotSummary>,
    on_connect: EventHandler<wire::BotId>,
    on_disconnect: EventHandler<wire::BotId>,
    on_delete: EventHandler<wire::BotId>,
}

#[component]
fn BotsTable(props: BotsTableProps) -> Element {
    rsx! {
        table { class: "data-table",
            "aria-label": "Music bots",
            thead {
                tr {
                    th { scope: "col", "Bot" }
                    th { scope: "col", "Server" }
                    th { scope: "col", "State" }
                    th { scope: "col", "Now playing" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for b in props.rows.iter() {
                    {
                        let b = b.clone();
                        let id = b.id;
                        let state = b.state;
                        let on_connect = props.on_connect;
                        let on_disconnect = props.on_disconnect;
                        let on_delete = props.on_delete;
                        let online = matches!(state, wire::BotState::Connected | wire::BotState::InChannel | wire::BotState::Playing);
                        let now_playing = b
                            .now_playing
                            .as_ref()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| "—".into());
                        rsx! {
                            tr { key: "{id.0}",
                                td { class: "client-cell",
                                    Link {
                                        to: Route::BotDetailPage { bot_id: id.0 },
                                        class: "client-name",
                                        "{b.name}"
                                    }
                                    span { class: "client-uid", "id {id.0}" }
                                }
                                td { "{b.server_addr}" }
                                td {
                                    span { class: state_badge_class(state),
                                        "{state_label(state)}"
                                    }
                                }
                                td { "{now_playing}" }
                                td { class: "actions-col",
                                    if online {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_disconnect.call(id),
                                            "Disconnect"
                                        }
                                    } else {
                                        Button {
                                            variant: ButtonVariant::Primary,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_connect.call(id),
                                            "Connect"
                                        }
                                    }
                                    Link {
                                        to: Route::BotDetailPage { bot_id: id.0 },
                                        class: "btn btn-ghost btn-sm",
                                        "Open"
                                    }
                                    Button {
                                        variant: ButtonVariant::Danger,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_delete.call(id),
                                        "Delete"
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

#[derive(Props, Clone, PartialEq)]
struct CreateBotModalProps {
    on_close: EventHandler<()>,
    on_created: EventHandler<wire::MusicBotSummary>,
}

#[component]
fn CreateBotModal(props: CreateBotModalProps) -> Element {
    let gate = use_auth_gate();
    let mut name: Signal<String> = use_signal(String::new);
    let mut server_addr: Signal<String> = use_signal(|| String::from("127.0.0.1:9987"));
    let mut auto_connect: Signal<bool> = use_signal(|| true);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_close = props.on_close;
    let on_created = props.on_created;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed_name = name.read().trim().to_string();
        let trimmed_addr = server_addr.read().trim().to_string();
        if trimmed_name.is_empty() {
            error.set(Some("Name is required.".into()));
            return;
        }
        if trimmed_addr.is_empty() {
            error.set(Some("Server address is required (e.g. host:9987).".into()));
            return;
        }
        submitting.set(true);
        error.set(None);
        let body = wire::CreateBotRequest {
            name: trimmed_name,
            server_addr: trimmed_addr,
            identity_path: None,
            auto_connect: Some(*auto_connect.read()),
        };
        let gate = gate.clone();
        spawn(async move {
            match mb::create_bot(gate, &body).await {
                Ok(s) => {
                    submitting.set(false);
                    on_created.call(s);
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
                "aria-labelledby": "create-bot-title",
                div { class: "modal-header",
                    h2 { id: "create-bot-title", "Create music bot" }
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
                        Banner { variant: BannerVariant::Danger, title: "Could not create bot".to_string(),
                            "{msg}"
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Name" }
                        input {
                            class: "input",
                            value: "{name.read()}",
                            placeholder: "DJ-Bot",
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Server address" }
                        input {
                            class: "input",
                            value: "{server_addr.read()}",
                            placeholder: "host:9987",
                            oninput: move |e| server_addr.set(e.value()),
                        }
                    }
                    label { class: "field-inline",
                        input {
                            r#type: "checkbox",
                            checked: *auto_connect.read(),
                            oninput: move |e| auto_connect.set(e.value() == "true"),
                        }
                        " Auto-connect on spawn"
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
                        "Create bot"
                    }
                }
            }
        }
    }
}
