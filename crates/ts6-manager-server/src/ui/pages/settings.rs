//! `/settings` — admin-scoped manager settings (PURA-224).
//!
//! Phase 1 ships one section: YouTube cookies for the music-bot pipeline.
//! Lives under its own `/settings` route rather than inside `/music-bots`
//! because the cookies file is process-global (every bot's `yt-dlp` shells
//! out against the same file) and admin-only — putting it on the bots
//! index would suggest per-bot scoping, which the PURA-223 spec
//! explicitly defers.
//!
//! All three calls flow through [`crate::client::settings`], which mirrors
//! the [`crate::routes::settings`] handlers byte-for-byte. The page never
//! sees raw URLs or `serde_json::Value`.
//!
//! Rendering states cover the four on the operator's mental model:
//!
//! - Loading the initial status (skeleton).
//! - "No cookies uploaded" (idle empty state, file picker visible).
//! - "Cookies uploaded YYYY-MM-DD HH:MM" + Replace + Delete affordances.
//! - Upload in flight (button busy, file input disabled).
//!
//! Backend errors surface through [`crate::ui::pages::music_bots::shared::format_error`]
//! so the vocabulary matches the rest of the SPA.

use dioxus::prelude::*;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::settings as cookie_api;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonVariant};

#[component]
pub fn SettingsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }

    rsx! {
        div { class: "crumb", "Settings" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Settings" }
                p { class: "page-lede",
                    "Process-wide configuration. These settings apply to every server connection and bot in this manager."
                }
            }
        }

        section { class: "stack-md",
            YoutubeCookieSection {}
            YoutubeApiKeySection {}
        }
    }
}

/// Status of the persisted cookie file: not yet fetched, an error, or a
/// concrete `{ uploaded, uploadedAt? }` payload.
#[derive(Clone, Debug, PartialEq)]
enum CookieView {
    Loading,
    Error(ApiError),
    Loaded(cookie_api::CookieStatus),
}

#[component]
fn YoutubeCookieSection() -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut view: Signal<CookieView> = use_signal(|| CookieView::Loading);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut uploading: Signal<bool> = use_signal(|| false);
    let mut deleting: Signal<bool> = use_signal(|| false);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { cookie_api::get_youtube_cookie_status(gate).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(status)) => view.set(CookieView::Loaded(status.clone())),
        Some(Err(e)) => view.set(CookieView::Error(e.clone())),
        None => view.set(CookieView::Loading),
    });

    let on_delete = {
        let gate = gate.clone();
        move |_| {
            if *uploading.read() || *deleting.read() {
                return;
            }
            deleting.set(true);
            let gate = gate.clone();
            spawn(async move {
                match cookie_api::delete_youtube_cookie_file(gate).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "YouTube cookies removed", None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not delete cookies",
                            Some(format_error(&e)),
                        );
                    }
                }
                deleting.set(false);
            });
        }
    };

    let on_file_picked = {
        let gate = gate.clone();
        move |file: PickedFile| {
            if *uploading.read() {
                return;
            }
            uploading.set(true);
            let gate = gate.clone();
            spawn(async move {
                #[cfg(target_arch = "wasm32")]
                let result = cookie_api::upload_youtube_cookie_file(gate, file.0).await;
                #[cfg(not(target_arch = "wasm32"))]
                let result: Result<(), ApiError> = {
                    let _ = (gate, file);
                    Err(ApiError::UnsupportedTarget)
                };
                match result {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "YouTube cookies uploaded", None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Upload failed",
                            Some(format_error(&e)),
                        );
                    }
                }
                uploading.set(false);
            });
        }
    };

    rsx! {
        div { class: "card stack-md",
            div { class: "page-title-block",
                h2 { "YouTube cookies" }
                p { class: "muted",
                    "Upload a Netscape-format cookies.txt to authenticate yt-dlp against age-restricted, members-only, or sign-in-walled videos. Applies process-wide to every music bot."
                }
            }

            { match view.read().clone() {
                CookieView::Loading => rsx! {
                    p { class: "muted", aria_busy: "true", "Loading cookie status…" }
                },
                CookieView::Error(err) => rsx! {
                    Banner { variant: BannerVariant::Danger,
                        title: "Could not load cookie status".to_string(),
                        "{format_error(&err)}"
                    }
                },
                CookieView::Loaded(status) => {
                    let busy = *uploading.read() || *deleting.read();
                    let body = if status.uploaded {
                        rsx! {
                            CookieStatusLine { uploaded_at: status.uploaded_at.clone() }
                            div { class: "actions",
                                FilePickerButton {
                                    label: "Replace cookies".to_string(),
                                    busy: *uploading.read(),
                                    disabled: busy,
                                    on_file: on_file_picked.clone(),
                                }
                                Button {
                                    variant: ButtonVariant::Danger,
                                    disabled: busy,
                                    loading: *deleting.read(),
                                    onclick: on_delete.clone(),
                                    "Delete cookies"
                                }
                            }
                        }
                    } else {
                        rsx! {
                            p { class: "muted", "No cookies uploaded." }
                            div { class: "actions",
                                FilePickerButton {
                                    label: "Upload cookies.txt".to_string(),
                                    busy: *uploading.read(),
                                    disabled: busy,
                                    on_file: on_file_picked.clone(),
                                }
                            }
                        }
                    };
                    rsx! { {body} }
                }
            } }
        }
    }
}

/// Status of the persisted YouTube Data API key (THE-948): not yet
/// fetched, an error, or a `{ configured }` payload. The key value never
/// reaches the UI — only whether one is set.
#[derive(Clone, Debug, PartialEq)]
enum ApiKeyView {
    Loading,
    Error(ApiError),
    Loaded(cookie_api::ApiKeyStatus),
}

#[component]
fn YoutubeApiKeySection() -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut view: Signal<ApiKeyView> = use_signal(|| ApiKeyView::Loading);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut input: Signal<String> = use_signal(String::new);
    let mut saving: Signal<bool> = use_signal(|| false);
    let mut clearing: Signal<bool> = use_signal(|| false);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { cookie_api::get_youtube_api_key_status(gate).await }
        }
    });

    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(status)) => view.set(ApiKeyView::Loaded(status.clone())),
        Some(Err(e)) => view.set(ApiKeyView::Error(e.clone())),
        None => view.set(ApiKeyView::Loading),
    });

    let on_save = {
        let gate = gate.clone();
        move |_| {
            if *saving.read() || *clearing.read() {
                return;
            }
            let key = input.read().trim().to_string();
            if key.is_empty() {
                toaster.push(
                    ToastVariant::Danger,
                    "Enter an API key first",
                    None,
                );
                return;
            }
            saving.set(true);
            let gate = gate.clone();
            spawn(async move {
                match cookie_api::set_youtube_api_key(gate, key).await {
                    Ok(()) => {
                        input.set(String::new());
                        toaster.push(ToastVariant::Success, "YouTube API key saved", None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not save API key",
                            Some(format_error(&e)),
                        );
                    }
                }
                saving.set(false);
            });
        }
    };

    let on_clear = {
        let gate = gate.clone();
        move |_| {
            if *saving.read() || *clearing.read() {
                return;
            }
            clearing.set(true);
            let gate = gate.clone();
            spawn(async move {
                match cookie_api::delete_youtube_api_key(gate).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "YouTube API key cleared", None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not clear API key",
                            Some(format_error(&e)),
                        );
                    }
                }
                clearing.set(false);
            });
        }
    };

    rsx! {
        div { class: "card stack-md",
            div { class: "page-title-block",
                h2 { "YouTube API key" }
                p { class: "muted",
                    "Paste a YouTube Data API v3 key to make !play yt: searches resolve in ~300 ms via the API instead of the slower organic yt-dlp search. Applies process-wide; takes effect immediately, no restart. The stored key is never shown back."
                }
            }

            { match view.read().clone() {
                ApiKeyView::Loading => rsx! {
                    p { class: "muted", aria_busy: "true", "Loading API key status…" }
                },
                ApiKeyView::Error(err) => rsx! {
                    Banner { variant: BannerVariant::Danger,
                        title: "Could not load API key status".to_string(),
                        "{format_error(&err)}"
                    }
                },
                ApiKeyView::Loaded(status) => {
                    let busy = *saving.read() || *clearing.read();
                    let status_line = if status.configured {
                        rsx! { p { class: "settings-status", "Configured ✓" } }
                    } else {
                        rsx! { p { class: "muted", "Not set" } }
                    };
                    rsx! {
                        {status_line}
                        div { class: "form-row",
                            input {
                                id: "yt-api-key",
                                r#type: "password",
                                class: "input",
                                placeholder: if status.configured { "Enter a new key to replace" } else { "Paste API key" },
                                autocomplete: "off",
                                disabled: busy,
                                value: "{input}",
                                oninput: move |evt| input.set(evt.value()),
                            }
                        }
                        div { class: "actions",
                            Button {
                                variant: ButtonVariant::Primary,
                                disabled: busy,
                                loading: *saving.read(),
                                onclick: on_save.clone(),
                                "Save"
                            }
                            if status.configured {
                                Button {
                                    variant: ButtonVariant::Danger,
                                    disabled: busy,
                                    loading: *clearing.read(),
                                    onclick: on_clear.clone(),
                                    "Clear"
                                }
                            }
                        }
                    }
                }
            } }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CookieStatusLineProps {
    uploaded_at: Option<String>,
}

#[component]
fn CookieStatusLine(props: CookieStatusLineProps) -> Element {
    let label = match props.uploaded_at.as_deref() {
        Some(iso) => format!("Cookies uploaded {}", format_uploaded_at(iso)),
        None => "Cookies uploaded".to_string(),
    };
    rsx! {
        p { class: "settings-status", "{label}" }
    }
}

/// Wraps a `web_sys::File` so it can travel through Dioxus event handlers.
/// Only the WASM build carries a real handle; SSR / native builds use a
/// unit-typed sentinel so the page type-checks on every target.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct PickedFile(web_sys::File);

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct PickedFile;

#[cfg(target_arch = "wasm32")]
impl PickedFile {
    fn new(file: web_sys::File) -> Self {
        PickedFile(file)
    }
}

#[derive(Props, Clone, PartialEq)]
struct FilePickerButtonProps {
    label: String,
    #[props(default)]
    busy: bool,
    #[props(default)]
    disabled: bool,
    on_file: EventHandler<PickedFile>,
}

#[component]
fn FilePickerButton(props: FilePickerButtonProps) -> Element {
    // Hidden `<input type=file>` driven by a label-wrapped button so the
    // styling matches the rest of the action row. Using a label avoids
    // having to imperatively `.click()` the input through `web-sys`.
    let on_change = move |_evt: FormEvent| {
        #[cfg(target_arch = "wasm32")]
        {
            use wasm_bindgen::JsCast;
            let Some(window) = web_sys::window() else {
                return;
            };
            let Some(document) = window.document() else {
                return;
            };
            let Some(element) = document.get_element_by_id("yt-cookie-file") else {
                return;
            };
            let Ok(input) = element.dyn_into::<web_sys::HtmlInputElement>() else {
                return;
            };
            let Some(files) = input.files() else { return };
            let Some(file) = files.get(0) else { return };
            // Reset the input value so picking the same file again still
            // fires `onchange` on the next interaction (browsers suppress
            // the event when value is unchanged).
            input.set_value("");
            props.on_file.call(PickedFile::new(file));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = _evt;
        }
    };

    rsx! {
        label {
            class: if props.disabled { "btn btn-secondary is-disabled" } else { "btn btn-secondary" },
            r#for: "yt-cookie-file",
            "aria-busy": if props.busy { "true" } else { "false" },
            "aria-disabled": if props.disabled { "true" } else { "false" },
            if props.busy {
                span { class: "spinner is-sm", "aria-hidden": "true" }
            }
            "{props.label}"
        }
        input {
            id: "yt-cookie-file",
            r#type: "file",
            accept: ".txt,text/plain",
            class: "sr-only",
            disabled: props.disabled,
            onchange: on_change,
        }
    }
}

/// Operator-facing one-liner for any [`ApiError`] hit by the cookie
/// surface. Inlined rather than borrowed from `music_bots::shared` —
/// that module is `pub(super)` and the cookie surface is the first
/// `/settings` consumer, so taking a copy here keeps the public crate
/// surface small. If a third caller appears, hoist this to a shared
/// utility instead of growing copies.
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

/// Render an ISO-8601 timestamp as `YYYY-MM-DD HH:MM` (UTC). Falls back
/// to the raw string if parsing fails — better than showing nothing when
/// the backend returns a value the panel didn't recognise.
fn format_uploaded_at(iso: &str) -> String {
    use chrono::{DateTime, Utc};
    match DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt
            .with_timezone(&Utc)
            .format("%Y-%m-%d %H:%M UTC")
            .to_string(),
        Err(_) => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uploaded_at_renders_minutes_in_utc() {
        let s = format_uploaded_at("2026-05-15T12:34:56Z");
        assert_eq!(s, "2026-05-15 12:34 UTC");
    }

    #[test]
    fn format_uploaded_at_normalises_offset_to_utc() {
        // +02:00 means 14:34 local → 12:34 UTC after rfc3339 parse.
        let s = format_uploaded_at("2026-05-15T14:34:56+02:00");
        assert_eq!(s, "2026-05-15 12:34 UTC");
    }

    #[test]
    fn format_uploaded_at_falls_back_to_raw_on_parse_failure() {
        let s = format_uploaded_at("not-a-timestamp");
        assert_eq!(s, "not-a-timestamp");
    }
}
