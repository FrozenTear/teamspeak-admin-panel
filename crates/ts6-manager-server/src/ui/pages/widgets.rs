//! `/widgets` — operator-facing Widget Manager (PURA-92, spec Chapter 34).
//!
//! Slice G of the Phase 2 widgets epic ([PURA-72](/PURA/issues/PURA-72)).
//! Backed by the `/api/widgets` CRUD endpoints from Slice D
//! ([PURA-89](/PURA/issues/PURA-89)).
//!
//! ## Surface
//!
//! - **Table on the left** — one row per widget. Identifier cell shows
//!   `name` + the joined server name, plus a token preview. Trailing actions
//!   are Edit / Regenerate / Delete (the last two confirm-gated per
//!   `components.md` §3.5).
//! - **Embed URL row per widget** — the four canonical URLs from
//!   [`WidgetEmbedUrls`] (`/api/widget/{token}/data`,
//!   `/api/widget/{token}/image.svg`, `/api/widget/{token}/image.png`,
//!   `/widget/{token}`) each rendered with a copy-to-clipboard button.
//!   Origin is prepended client-side from `window.location.origin` so an
//!   operator pasting into a third-party site gets a fully-qualified URL.
//! - **Create dialog** — a `<Modal>` wraps the create form; opens from the
//!   page header's "+ New widget" button.
//! - **Edit drawer** — a right-side slide-in panel for the in-place patch
//!   form (per the Slice G layout brief).
//!
//! ## Theme picker
//!
//! Six live thumbnail tiles per `study-documents/design-system/widget-themes.md`
//! §3 — each tile is a 240×80 inline SVG mini-render of a realistic header +
//! channel + client sample, fed by [`WidgetThemePalette`]. The `transparent`
//! tile lays a CSS checkerboard behind the SVG so the operator sees that the
//! host page background will show through; selecting it surfaces the
//! "best on dark websites" warning sentence verbatim from the design doc.
//!
//! ## What this surface does NOT do
//!
//! - Virtual-server discovery is not in v1; we ship a numeric input that
//!   defaults to [`active_server::DEFAULT_VIRTUAL_SERVER_ID`]. Most TS3
//!   deployments only have one virtual server (id 1); the picker lands when
//!   the upstream `vserverlist` query is wired into a typed REST surface.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::servers::ServerSummary;
use ts6_manager_shared::widgets::{
    CreateWidgetRequest, UpdateWidgetRequest, WidgetEmbedUrls, WidgetSummary, WidgetThemeName,
    WidgetThemePalette,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::layout::{ServersData, use_servers_context};
use crate::ui::pages::active_server;

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// Modal/drawer state machine. At most one panel is visible at a time so the
/// keyboard focus trap and ESC-handler stay simple.
#[derive(Clone, Debug, PartialEq)]
enum PanelState {
    Closed,
    Create,
    Edit(i64),
    Regenerate(i64),
    Delete(i64),
    /// PURA-322 — copy-ready Twitch/Kick share recipe for one widget.
    Share(i64),
}

#[component]
pub fn WidgetsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    // The page-level snapshot resource captures the gate; the panel
    // sub-components grab their own gate via `use_auth_gate()` so we don't
    // have to pass an `Arc<RefreshGate>` through `Props` (the type can't
    // derive `PartialEq`).
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let servers_ctx = use_servers_context();

    // Local working copy fed by the snapshot fetch + post-mutation reloads.
    let mut rows: Signal<Vec<WidgetSummary>> = use_signal(Vec::<WidgetSummary>::new);
    let mut last_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut panel: Signal<PanelState> = use_signal(|| PanelState::Closed);
    // Bumping `reload_marker` re-runs the snapshot resource. Mutations that
    // succeed (create / patch / delete / regenerate) bump it; this keeps the
    // page in sync without a stale-entry race against the WS hub.
    let mut reload_marker: Signal<u64> = use_signal(|| 0u64);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload_marker.read();
            async move { fetch_widgets(gate).await }
        }
    });

    {
        use_effect(move || match &*snapshot.read_unchecked() {
            Some(Ok(list)) => {
                rows.set(list.clone());
                last_error.set(None);
                loading.set(false);
            }
            Some(Err(e)) => {
                last_error.set(Some(e.clone()));
                loading.set(false);
            }
            None => {
                loading.set(true);
            }
        });
    }

    let on_close_panel = {
        let mut panel = panel;
        move |_| panel.set(PanelState::Closed)
    };

    let on_open_create = {
        let mut panel = panel;
        move |_| panel.set(PanelState::Create)
    };

    let make_after_mutation = move || {
        reload_marker.with_mut(|n| *n += 1);
        panel.set(PanelState::Closed);
    };

    let active = panel.read().clone();
    let row_for_active = |id: i64| rows.read().iter().find(|w| w.id == id).cloned();

    rsx! {
        div { class: "crumb", "Widgets" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Widgets" }
                p { class: "page-lede",
                    "Public, read-only embed widgets for community sites. Each token is its own credential — rotate or revoke at any time."
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: on_open_create,
                    "+ New widget"
                }
            }
        }

        if let Some(err) = last_error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load widgets".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading widgets…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", "▣" }
                    h3 { "No widgets yet" }
                    p { "Create a widget to share a public, read-only view of one of your TeamSpeak servers." }
                    div { class: "actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: {
                                let mut panel = panel;
                                move |_| panel.set(PanelState::Create)
                            },
                            "+ New widget"
                        }
                    }
                }
            } else {
                WidgetsTable {
                    rows: rows.read().clone(),
                    on_edit: {
                        let mut panel = panel;
                        EventHandler::new(move |id: i64| panel.set(PanelState::Edit(id)))
                    },
                    on_regenerate: {
                        let mut panel = panel;
                        EventHandler::new(move |id: i64| panel.set(PanelState::Regenerate(id)))
                    },
                    on_delete: {
                        let mut panel = panel;
                        EventHandler::new(move |id: i64| panel.set(PanelState::Delete(id)))
                    },
                    on_share: {
                        let mut panel = panel;
                        EventHandler::new(move |id: i64| panel.set(PanelState::Share(id)))
                    },
                    on_copy: {
                        EventHandler::new(move |label: String| {
                            toaster.push(ToastVariant::Success, format!("Copied {label}"), None);
                        })
                    },
                }
            }
        }

        // ── Panels (modal / drawer) ──────────────────────────────────────
        match active {
            PanelState::Create => rsx! {
                CreateWidgetModal {
                    servers: servers_ctx.data.read().clone(),
                    on_close: on_close_panel,
                    on_created: {
                        let mut after = make_after_mutation;
                        EventHandler::new(move |w: WidgetSummary| {
                            toaster.push(ToastVariant::Success, format!("Created widget “{}”", w.name), None);
                            after();
                        })
                    },
                }
            },
            PanelState::Edit(id) => match row_for_active(id) {
                Some(row) => rsx! {
                    EditWidgetDrawer {
                        widget: row,
                        on_close: on_close_panel,
                        on_saved: {
                            let mut after = make_after_mutation;
                            EventHandler::new(move |w: WidgetSummary| {
                                toaster.push(ToastVariant::Success, format!("Saved “{}”", w.name), None);
                                after();
                            })
                        },
                    }
                },
                None => rsx! { "" },
            },
            PanelState::Regenerate(id) => match row_for_active(id) {
                Some(row) => rsx! {
                    RegenerateConfirmModal {
                        widget: row,
                        on_close: on_close_panel,
                        on_done: {
                            let mut after = make_after_mutation;
                            EventHandler::new(move |w: WidgetSummary| {
                                toaster.push(
                                    ToastVariant::Success,
                                    "Token regenerated",
                                    Some(format!("New URL: /widget/{}", w.token)),
                                );
                                after();
                            })
                        },
                    }
                },
                None => rsx! { "" },
            },
            PanelState::Delete(id) => match row_for_active(id) {
                Some(row) => rsx! {
                    DeleteConfirmModal {
                        widget: row,
                        on_close: on_close_panel,
                        on_done: {
                            let mut after = make_after_mutation;
                            EventHandler::new(move |name: String| {
                                toaster.push(ToastVariant::Success, format!("Deleted “{name}”"), None);
                                after();
                            })
                        },
                    }
                },
                None => rsx! { "" },
            },
            PanelState::Share(id) => match row_for_active(id) {
                Some(row) => rsx! {
                    ShareWidgetModal { widget: row, on_close: on_close_panel }
                },
                None => rsx! { "" },
            },
            PanelState::Closed => rsx! { "" },
        }
    }
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct WidgetsTableProps {
    rows: Vec<WidgetSummary>,
    on_edit: EventHandler<i64>,
    on_regenerate: EventHandler<i64>,
    on_delete: EventHandler<i64>,
    on_share: EventHandler<i64>,
    on_copy: EventHandler<String>,
}

#[component]
fn WidgetsTable(props: WidgetsTableProps) -> Element {
    rsx! {
        table { class: "data-table widgets-table",
            "aria-label": "Operator widgets",
            thead {
                tr {
                    th { scope: "col", "Widget" }
                    th { scope: "col", "Server" }
                    th { scope: "col", "Theme" }
                    th { scope: "col", "Embed" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for w in props.rows.iter() {
                    {
                        let w = w.clone();
                        let id = w.id;
                        let on_edit = props.on_edit;
                        let on_regenerate = props.on_regenerate;
                        let on_delete = props.on_delete;
                        let on_share = props.on_share;
                        let on_copy = props.on_copy;
                        rsx! {
                            tr { key: "{id}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{w.name}" }
                                    span { class: "client-uid",
                                        "token "
                                        span { class: "token-prefix", "{token_preview(&w.token)}" }
                                    }
                                }
                                td {
                                    div { class: "server-cell",
                                        span { class: "server-name", "{w.server_name.clone().unwrap_or_else(|| String::from(\"(deleted)\"))}" }
                                        if let Some(host) = w.server_host.as_deref() {
                                            span { class: "server-host", "{host}" }
                                        }
                                    }
                                }
                                td {
                                    span { class: "theme-tag", "{w.theme}" }
                                }
                                td { class: "embed-cell",
                                    EmbedUrlsRow { urls: w.embed_urls.clone(), on_copy: on_copy }
                                }
                                td { class: "actions-col",
                                    Button {
                                        variant: ButtonVariant::Secondary,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_edit.call(id),
                                        "Edit"
                                    }
                                    Button {
                                        variant: ButtonVariant::Secondary,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_share.call(id),
                                        "Share"
                                    }
                                    Button {
                                        variant: ButtonVariant::Ghost,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_regenerate.call(id),
                                        "Regenerate"
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
struct EmbedUrlsRowProps {
    urls: WidgetEmbedUrls,
    on_copy: EventHandler<String>,
}

#[component]
fn EmbedUrlsRow(props: EmbedUrlsRowProps) -> Element {
    let copy_button = move |label: String, path: String| {
        let on_copy = props.on_copy;
        let label_for_toast = label.clone();
        rsx! {
            button {
                class: "embed-url",
                r#type: "button",
                title: "Copy {label} URL",
                onclick: move |_| {
                    let abs = absolute_url(&path);
                    copy_to_clipboard(&abs);
                    on_copy.call(label_for_toast.clone());
                },
                span { class: "embed-label", "{label}" }
                span { class: "embed-path", "{path}" }
                span { class: "embed-copy", "Copy" }
            }
        }
    };
    rsx! {
        div { class: "embed-urls",
            {copy_button("page".to_string(), props.urls.page_url.clone())}
            {copy_button("data".to_string(), props.urls.data_url.clone())}
            {copy_button("svg".to_string(), props.urls.svg_url.clone())}
            {copy_button("png".to_string(), props.urls.png_url.clone())}
        }
    }
}

// ---------------------------------------------------------------------------
// Share to Twitch / Kick (PURA-322)
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct ShareWidgetModalProps {
    widget: WidgetSummary,
    on_close: EventHandler<MouseEvent>,
}

/// Per-widget "Share to Twitch / Kick" recipe. Twitch and Kick strip
/// `<iframe>`/`<script>` from every profile surface, so this modal hands the
/// operator the realistic fallback: a snapshot image + the live page link,
/// with absolute (origin-prefixed) URLs that paste straight into the
/// platform. See the `widget-live-options` doc on [PURA-322] for the full
/// per-surface rationale.
#[component]
fn ShareWidgetModal(props: ShareWidgetModalProps) -> Element {
    let toaster = use_toaster();
    let on_close = props.on_close;
    let widget = props.widget.clone();

    let abs_png = absolute_url(&widget.embed_urls.png_url);
    let abs_page = absolute_url(&widget.embed_urls.page_url);

    // Reusable copy-to-clipboard chip. `value` is already a final, absolute
    // string — unlike `EmbedUrlsRow`, nothing is absolutised on click.
    let copy_value = move |label: String, value: String| {
        let toast_label = label.clone();
        rsx! {
            button {
                class: "embed-url",
                r#type: "button",
                title: "Copy {label}",
                onclick: move |_| {
                    copy_to_clipboard(&value);
                    toaster.push(ToastVariant::Success, format!("Copied {toast_label}"), None);
                },
                span { class: "embed-label", "{label}" }
                span { class: "embed-path", "{value}" }
                span { class: "embed-copy", "Copy" }
            }
        }
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            div {
                class: "modal modal-lg",
                onclick: move |evt| evt.stop_propagation(),
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "share-widget-title",
                div { class: "modal-header",
                    h2 { id: "share-widget-title", "Share “{widget.name}” to Twitch / Kick" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "modal-body stack-md",
                    Banner { variant: BannerVariant::Info, title: "Profiles can't embed a live widget".to_string(),
                        "Twitch and Kick strip live embeds from every profile surface. These recipes use a snapshot image plus the live page link — the closest the platforms allow."
                    }

                    section { class: "stack-sm",
                        h3 { "Twitch — stream panel" }
                        ol { class: "share-steps",
                            li { "Open your channel → " strong { "Edit Panels" } " → add a panel." }
                            li { "Download the snapshot image and upload it as the panel image:" }
                            li { class: "share-copy-row", {copy_value("snapshot image URL".to_string(), abs_png.clone())} }
                            li { "Set the panel " strong { "link" } " to the live widget page:" }
                            li { class: "share-copy-row", {copy_value("live page link".to_string(), abs_page.clone())} }
                            li { "Save. The panel image is a snapshot; the link always opens the live, auto-updating page." }
                        }
                    }

                    section { class: "stack-sm",
                        h3 { "Kick — About section" }
                        ol { class: "share-steps",
                            li { "Open your channel → " strong { "About" } " → " strong { "Edit Panels" } " → add a panel." }
                            li { "Download the snapshot image and upload it as the panel image:" }
                            li { class: "share-copy-row", {copy_value("snapshot image URL".to_string(), abs_png.clone())} }
                            li { "Set the panel " strong { "link" } " to the live widget page:" }
                            li { class: "share-copy-row", {copy_value("live page link".to_string(), abs_page.clone())} }
                            li { "Save. The panel image is a static snapshot — to refresh it, re-download and re-upload. The link always opens the live, auto-updating page." }
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Done"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Create modal
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct CreateWidgetModalProps {
    servers: ServersData,
    on_close: EventHandler<MouseEvent>,
    on_created: EventHandler<WidgetSummary>,
}

#[component]
fn CreateWidgetModal(props: CreateWidgetModalProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    // Pick the first available server as the default — the form falls back
    // to "no servers" copy when the list is empty.
    let server_rows: Vec<ServerSummary> = props.servers.rows().to_vec();
    let initial_server = server_rows.first().map(|s| s.id).unwrap_or(0);

    let mut name: Signal<String> = use_signal(String::new);
    let mut server_id: Signal<i64> = use_signal(|| initial_server);
    let mut virtual_server_id: Signal<i64> =
        use_signal(|| active_server::DEFAULT_VIRTUAL_SERVER_ID);
    let mut theme: Signal<WidgetThemeName> = use_signal(|| WidgetThemeName::Dark);
    let mut show_channel_tree: Signal<bool> = use_signal(|| true);
    let mut show_clients: Signal<bool> = use_signal(|| true);
    let mut hide_empty_channels: Signal<bool> = use_signal(|| false);
    let mut max_depth: Signal<i64> = use_signal(|| 5i64);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_close = props.on_close;
    let on_created = props.on_created;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed = name.read().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("Name is required.".into()));
            return;
        }
        if *server_id.read() <= 0 {
            error.set(Some("Choose a server.".into()));
            return;
        }
        submitting.set(true);
        error.set(None);

        let body = CreateWidgetRequest {
            name: trimmed,
            server_config_id: *server_id.read(),
            virtual_server_id: *virtual_server_id.read(),
            theme: Some(theme.read().as_str().into()),
            show_channel_tree: Some(*show_channel_tree.read()),
            show_clients: Some(*show_clients.read()),
            hide_empty_channels: Some(*hide_empty_channels.read()),
            max_channel_depth: Some(*max_depth.read()),
        };
        let gate = gate.clone();
        spawn(async move {
            let res = api::authorized_post_json::<_, WidgetSummary>(
                &gate,
                &api::api_base(),
                "/api/widgets",
                Some(&body),
            )
            .await;
            submitting.set(false);
            match res {
                Ok(w) => on_created.call(w),
                Err(e) => error.set(Some(format_error(&e))),
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "modal modal-lg",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "create-widget-title",
                div { class: "modal-header",
                    h2 { id: "create-widget-title", "New widget" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not create widget".to_string(),
                            "{msg}"
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Widget name "
                            span { class: "field-required", "*" }
                        }
                        input {
                            class: "input",
                            r#type: "text",
                            value: "{name.read()}",
                            placeholder: "e.g. Community widget",
                            required: true,
                            autofocus: true,
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    if server_rows.is_empty() {
                        Banner { variant: BannerVariant::Warning, title: "No servers configured".to_string(),
                            "Add a server first — widgets must point at an existing server connection."
                        }
                    } else {
                        ServerPicker {
                            servers: server_rows.clone(),
                            value: *server_id.read(),
                            on_change: EventHandler::new(move |v: i64| server_id.set(v)),
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Virtual server id" }
                        input {
                            class: "input",
                            r#type: "number",
                            inputmode: "numeric",
                            min: "1",
                            value: "{virtual_server_id.read()}",
                            oninput: move |e| {
                                if let Ok(v) = e.value().parse::<i64>() {
                                    virtual_server_id.set(v.max(1));
                                }
                            },
                        }
                        span { class: "field-help",
                            "Most TeamSpeak servers only host one virtual server (id 1)."
                        }
                    }
                    ThemePicker {
                        value: *theme.read(),
                        on_change: EventHandler::new(move |t: WidgetThemeName| theme.set(t)),
                    }
                    VisibilityToggles {
                        show_channel_tree: *show_channel_tree.read(),
                        show_clients: *show_clients.read(),
                        hide_empty_channels: *hide_empty_channels.read(),
                        on_show_channel_tree: EventHandler::new(move |v: bool| show_channel_tree.set(v)),
                        on_show_clients: EventHandler::new(move |v: bool| show_clients.set(v)),
                        on_hide_empty: EventHandler::new(move |v: bool| hide_empty_channels.set(v)),
                    }
                    DepthSlider {
                        value: *max_depth.read(),
                        on_change: EventHandler::new(move |v: i64| max_depth.set(v)),
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        disabled: server_rows.is_empty(),
                        "Create widget"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Edit drawer
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct EditWidgetDrawerProps {
    widget: WidgetSummary,
    on_close: EventHandler<MouseEvent>,
    on_saved: EventHandler<WidgetSummary>,
}

#[component]
fn EditWidgetDrawer(props: EditWidgetDrawerProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    let initial = props.widget.clone();

    let mut name: Signal<String> = use_signal(|| initial.name.clone());
    let mut theme: Signal<WidgetThemeName> =
        use_signal(|| WidgetThemeName::parse_or_default(&initial.theme));
    let mut show_channel_tree: Signal<bool> = use_signal(|| initial.show_channel_tree);
    let mut show_clients: Signal<bool> = use_signal(|| initial.show_clients);
    let mut hide_empty_channels: Signal<bool> = use_signal(|| initial.hide_empty_channels);
    let mut max_depth: Signal<i64> = use_signal(|| initial.max_channel_depth);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_close = props.on_close;
    let on_saved = props.on_saved;
    let widget_id = initial.id;
    let server_label = initial
        .server_name
        .clone()
        .unwrap_or_else(|| String::from("(deleted)"));

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let trimmed = name.read().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("Name is required.".into()));
            return;
        }
        submitting.set(true);
        error.set(None);

        let body = UpdateWidgetRequest {
            name: Some(trimmed),
            theme: Some(theme.read().as_str().into()),
            show_channel_tree: Some(*show_channel_tree.read()),
            show_clients: Some(*show_clients.read()),
            hide_empty_channels: Some(*hide_empty_channels.read()),
            max_channel_depth: Some(*max_depth.read()),
        };
        let gate = gate.clone();
        spawn(async move {
            let path = format!("/api/widgets/{widget_id}");
            let res = api::authorized_patch_json::<_, WidgetSummary>(
                &gate,
                &api::api_base(),
                &path,
                &body,
            )
            .await;
            submitting.set(false);
            match res {
                Ok(w) => on_saved.call(w),
                Err(e) => error.set(Some(format_error(&e))),
            }
        });
    };

    rsx! {
        div { class: "drawer-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "drawer",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "edit-widget-title",
                div { class: "drawer-header",
                    h2 { id: "edit-widget-title", "Edit widget" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "drawer-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not save widget".to_string(),
                            "{msg}"
                        }
                    }
                    div { class: "info-row",
                        p { class: "info-label", "Server" }
                        p { class: "info-value", "{server_label}" }
                        p { class: "info-hint",
                            "The widget's server binding is fixed at creation time — recreate the widget to point at a different server."
                        }
                    }
                    label { class: "field",
                        span { class: "field-label", "Widget name "
                            span { class: "field-required", "*" }
                        }
                        input {
                            class: "input",
                            r#type: "text",
                            value: "{name.read()}",
                            required: true,
                            oninput: move |e| name.set(e.value()),
                        }
                    }
                    ThemePicker {
                        value: *theme.read(),
                        on_change: EventHandler::new(move |t: WidgetThemeName| theme.set(t)),
                    }
                    VisibilityToggles {
                        show_channel_tree: *show_channel_tree.read(),
                        show_clients: *show_clients.read(),
                        hide_empty_channels: *hide_empty_channels.read(),
                        on_show_channel_tree: EventHandler::new(move |v: bool| show_channel_tree.set(v)),
                        on_show_clients: EventHandler::new(move |v: bool| show_clients.set(v)),
                        on_hide_empty: EventHandler::new(move |v: bool| hide_empty_channels.set(v)),
                    }
                    DepthSlider {
                        value: *max_depth.read(),
                        on_change: EventHandler::new(move |v: i64| max_depth.set(v)),
                    }
                }
                div { class: "drawer-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        "Save changes"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Confirm modals
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct RegenerateConfirmModalProps {
    widget: WidgetSummary,
    on_close: EventHandler<MouseEvent>,
    on_done: EventHandler<WidgetSummary>,
}

#[component]
fn RegenerateConfirmModal(props: RegenerateConfirmModalProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    let widget = props.widget.clone();
    let mut typed: Signal<String> = use_signal(String::new);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_close = props.on_close;
    let on_done = props.on_done;
    let widget_id = widget.id;
    let widget_name = widget.name.clone();
    let confirm_match = widget_name.clone();

    let typed_matches = *typed.read() == confirm_match;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() || !typed_matches {
            return;
        }
        submitting.set(true);
        error.set(None);
        let gate = gate.clone();
        spawn(async move {
            let path = format!("/api/widgets/{widget_id}/regenerate-token");
            let res = api::authorized_post_json::<(), WidgetSummary>(
                &gate,
                &api::api_base(),
                &path,
                None,
            )
            .await;
            submitting.set(false);
            match res {
                Ok(w) => on_done.call(w),
                Err(e) => error.set(Some(format_error(&e))),
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "regenerate-token-title",
                div { class: "modal-header",
                    h2 { id: "regenerate-token-title", "Regenerate token" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not regenerate token".to_string(),
                            "{msg}"
                        }
                    }
                    p {
                        "Rotating the token will immediately break every existing embed that uses the current URL — sites embedding "
                        strong { "{widget_name}" }
                        " will return 404 until you copy the new URL out."
                    }
                    label { class: "field",
                        span { class: "field-label",
                            "Type the widget name to confirm: "
                            code { "{widget_name}" }
                        }
                        input {
                            class: "input",
                            r#type: "text",
                            value: "{typed.read()}",
                            autofocus: true,
                            oninput: move |e| typed.set(e.value()),
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        kind: ButtonType::Submit,
                        disabled: !typed_matches,
                        loading: *submitting.read(),
                        "Regenerate token"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DeleteConfirmModalProps {
    widget: WidgetSummary,
    on_close: EventHandler<MouseEvent>,
    on_done: EventHandler<String>,
}

#[component]
fn DeleteConfirmModal(props: DeleteConfirmModalProps) -> Element {
    let gate: Arc<RefreshGate> = use_auth_gate();
    let widget = props.widget.clone();
    let mut typed: Signal<String> = use_signal(String::new);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None::<String>);

    let on_close = props.on_close;
    let on_done = props.on_done;
    let widget_id = widget.id;
    let widget_name = widget.name.clone();
    let confirm_match = widget_name.clone();
    let confirm_for_done = widget_name.clone();
    let typed_matches = *typed.read() == confirm_match;

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() || !typed_matches {
            return;
        }
        submitting.set(true);
        error.set(None);
        let gate = gate.clone();
        let confirm_for_done = confirm_for_done.clone();
        spawn(async move {
            let path = format!("/api/widgets/{widget_id}");
            let res = api::authorized_delete(&gate, &api::api_base(), &path).await;
            submitting.set(false);
            match res {
                Ok(()) => on_done.call(confirm_for_done),
                Err(e) => error.set(Some(format_error(&e))),
            }
        });
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                "role": "dialog",
                "aria-modal": "true",
                "aria-labelledby": "delete-widget-title",
                div { class: "modal-header",
                    h2 { id: "delete-widget-title", "Delete widget" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        onclick: move |evt| on_close.call(evt),
                        "aria-label": "Close",
                        "✕"
                    }
                }
                div { class: "modal-body stack-md",
                    if let Some(msg) = error.read().as_ref() {
                        Banner { variant: BannerVariant::Danger, title: "Could not delete widget".to_string(),
                            "{msg}"
                        }
                    }
                    p {
                        "Deleting "
                        strong { "{widget_name}" }
                        " removes the widget row and its public token. All four embed URLs will return 404 immediately and cannot be recovered."
                    }
                    label { class: "field",
                        span { class: "field-label",
                            "Type the widget name to confirm: "
                            code { "{widget_name}" }
                        }
                        input {
                            class: "input",
                            r#type: "text",
                            value: "{typed.read()}",
                            autofocus: true,
                            oninput: move |e| typed.set(e.value()),
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |evt| on_close.call(evt),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        kind: ButtonType::Submit,
                        disabled: !typed_matches,
                        loading: *submitting.read(),
                        "Delete widget"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Form sub-components
// ---------------------------------------------------------------------------

#[derive(Props, Clone, PartialEq)]
struct ServerPickerProps {
    servers: Vec<ServerSummary>,
    value: i64,
    on_change: EventHandler<i64>,
}

#[component]
fn ServerPicker(props: ServerPickerProps) -> Element {
    let on_change = props.on_change;
    rsx! {
        label { class: "field",
            span { class: "field-label", "Server "
                span { class: "field-required", "*" }
            }
            select {
                class: "input",
                value: "{props.value}",
                onchange: move |e| {
                    if let Ok(v) = e.value().parse::<i64>() {
                        on_change.call(v);
                    }
                },
                for s in props.servers.iter() {
                    option { key: "{s.id}", value: "{s.id}", "{s.name} — {s.host}" }
                }
            }
            span { class: "field-help",
                "Each widget binds to one server connection. Recreate the widget to change servers."
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ThemePickerProps {
    value: WidgetThemeName,
    on_change: EventHandler<WidgetThemeName>,
}

#[component]
fn ThemePicker(props: ThemePickerProps) -> Element {
    let themes: [WidgetThemeName; 6] = [
        WidgetThemeName::Dark,
        WidgetThemeName::Light,
        WidgetThemeName::Transparent,
        WidgetThemeName::Neon,
        WidgetThemeName::Military,
        WidgetThemeName::Minimal,
    ];
    let selected = props.value;
    let on_change = props.on_change;
    rsx! {
        div { class: "field",
            span { class: "field-label", "Theme" }
            div { class: "widget-theme-grid",
                "role": "radiogroup",
                "aria-label": "Widget theme",
                for theme in themes.iter().copied() {
                    {
                        let is_selected = theme == selected;
                        let palette = theme.palette();
                        let is_transparent = matches!(theme, WidgetThemeName::Transparent);
                        let tile_class = if is_transparent {
                            if is_selected { "widget-theme-tile is-transparent is-selected" } else { "widget-theme-tile is-transparent" }
                        } else if is_selected {
                            "widget-theme-tile is-selected"
                        } else {
                            "widget-theme-tile"
                        };
                        rsx! {
                            button {
                                key: "{theme.as_str()}",
                                class: "{tile_class}",
                                r#type: "button",
                                "role": "radio",
                                "aria-checked": if is_selected { "true" } else { "false" },
                                "data-theme-name": "{theme.as_str()}",
                                onclick: move |_| on_change.call(theme),
                                ThemeThumbnail { palette: palette }
                                span { class: "widget-theme-name",
                                    "{theme.as_str()}"
                                    if is_selected {
                                        span { class: "widget-theme-check", "✓" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if matches!(selected, WidgetThemeName::Transparent) {
                p { class: "field-help widget-theme-warning",
                    "Best on dark websites — your site's background will show through. Use ‘light’ instead if your site is light-themed."
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ThemeThumbnailProps {
    palette: WidgetThemePalette,
}

#[component]
fn ThemeThumbnail(props: ThemeThumbnailProps) -> Element {
    let p = props.palette;
    // 240×80 mini-render: header band with server name + ONLINE badge,
    // one channel row, one client. Mirrors the live widget's vertical
    // rhythm so the operator can pre-flight palette legibility before
    // copying the embed URL out.
    rsx! {
        svg {
            class: "widget-theme-svg",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 240 80",
            width: "240",
            height: "80",
            "aria-hidden": "true",
            "focusable": "false",
            // Outer
            rect { x: "0", y: "0", width: "240", height: "80", rx: "6", ry: "6", fill: "{p.background}", stroke: "{p.border}" }
            // Header
            rect { x: "0", y: "0", width: "240", height: "22", fill: "{p.header_bg}" }
            text {
                x: "10", y: "15", fill: "{p.accent}",
                font_family: "Inter, system-ui, sans-serif", font_size: "11", font_weight: "700",
                "Community"
            }
            rect { x: "180", y: "5", width: "52", height: "12", rx: "6", ry: "6", fill: "{p.client_color}" }
            text {
                x: "187", y: "14", fill: "#FFFFFF",
                font_family: "Inter, system-ui, sans-serif", font_size: "9", font_weight: "600",
                "ONLINE 4"
            }
            // Header separator
            line { x1: "0", y1: "22", x2: "240", y2: "22", stroke: "{p.border}", stroke_width: "1" }
            // Channel row
            text {
                x: "10", y: "40", fill: "{p.accent}",
                font_family: "Inter, system-ui, sans-serif", font_size: "11", font_weight: "700",
                "#"
            }
            text {
                x: "22", y: "40", fill: "{p.text_primary}",
                font_family: "Inter, system-ui, sans-serif", font_size: "11", font_weight: "500",
                "general"
            }
            text {
                x: "210", y: "40", fill: "{p.text_secondary}",
                font_family: "Inter, system-ui, sans-serif", font_size: "10",
                "2/30"
            }
            // Client
            circle { cx: "16", cy: "57", r: "3", fill: "{p.client_color}" }
            text {
                x: "26", y: "60", fill: "{p.client_color}",
                font_family: "Inter, system-ui, sans-serif", font_size: "10",
                "operator"
            }
            // Footer
            line { x1: "0", y1: "70", x2: "240", y2: "70", stroke: "{p.border}", stroke_width: "1" }
            text {
                x: "10", y: "78", fill: "{p.text_secondary}", opacity: "0.6",
                font_family: "Inter, system-ui, sans-serif", font_size: "9",
                "TS6 widget"
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct VisibilityTogglesProps {
    show_channel_tree: bool,
    show_clients: bool,
    hide_empty_channels: bool,
    on_show_channel_tree: EventHandler<bool>,
    on_show_clients: EventHandler<bool>,
    on_hide_empty: EventHandler<bool>,
}

#[component]
fn VisibilityToggles(props: VisibilityTogglesProps) -> Element {
    let on_show_channel_tree = props.on_show_channel_tree;
    let on_show_clients = props.on_show_clients;
    let on_hide_empty = props.on_hide_empty;
    let show_channel_tree = props.show_channel_tree;
    let show_clients = props.show_clients;
    let hide_empty = props.hide_empty_channels;

    rsx! {
        fieldset { class: "field widget-toggles",
            legend { class: "field-label", "Visibility" }
            label { class: "toggle-row",
                input {
                    r#type: "checkbox",
                    checked: show_channel_tree,
                    onchange: move |e| on_show_channel_tree.call(e.value() == "true" || e.checked()),
                }
                span { class: "toggle-label", "Show channel tree" }
                span { class: "toggle-help", "Channels are listed below the server header." }
            }
            label { class: "toggle-row",
                input {
                    r#type: "checkbox",
                    checked: show_clients,
                    onchange: move |e| on_show_clients.call(e.value() == "true" || e.checked()),
                }
                span { class: "toggle-label", "Show client nicknames" }
                span { class: "toggle-help", "Hide for privacy on public-internet embeds." }
            }
            label { class: "toggle-row",
                input {
                    r#type: "checkbox",
                    checked: hide_empty,
                    onchange: move |e| on_hide_empty.call(e.value() == "true" || e.checked()),
                }
                span { class: "toggle-label", "Hide empty channels" }
                span { class: "toggle-help", "Drop channels with zero clients from the rendered tree." }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DepthSliderProps {
    value: i64,
    on_change: EventHandler<i64>,
}

#[component]
fn DepthSlider(props: DepthSliderProps) -> Element {
    let on_change = props.on_change;
    let value = props.value;
    rsx! {
        label { class: "field",
            span { class: "field-label", "Maximum channel depth" }
            div { class: "depth-slider",
                input {
                    r#type: "range",
                    min: "1",
                    max: "10",
                    step: "1",
                    value: "{value}",
                    oninput: move |e| {
                        if let Ok(v) = e.value().parse::<i64>() {
                            on_change.call(v.clamp(1, 10));
                        }
                    },
                }
                span { class: "depth-value", "{value}" }
            }
            span { class: "field-help",
                "Channels deeper than this are dropped from the rendered tree (per spec §27.1)."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn token_preview(token: &str) -> String {
    let mut chars: String = token.chars().take(4).collect();
    if !chars.is_empty() {
        chars.push('…');
    }
    chars
}

async fn fetch_widgets(gate: Arc<RefreshGate>) -> Result<Vec<WidgetSummary>, ApiError> {
    api::authorized_get_json(&gate, &api::api_base(), "/api/widgets").await
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

/// Build a fully-qualified URL by prepending `window.location.origin` to a
/// path-relative embed URL. Operators paste these into third-party sites, so
/// the wire shape `/api/widget/{token}/data` is not enough on its own.
fn absolute_url(path: &str) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(origin) = window.location().origin() {
                return format!("{origin}{path}");
            }
        }
    }
    path.to_string()
}

/// Asynchronously copy `text` to the system clipboard. Best-effort on the
/// browser side — the call returns a `Promise` that resolves once the write
/// completes; we deliberately do not await it because the toast that
/// confirms the copy is fire-and-forget. On non-WASM targets (SSR / native
/// tests) this is a no-op.
fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let clipboard = window.navigator().clipboard();
            let _ = clipboard.write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_preview_truncates_to_four_chars_plus_ellipsis() {
        assert_eq!(token_preview("abcdef-1234567890_"), "abcd…");
    }

    #[test]
    fn token_preview_short_input_keeps_ellipsis_after_chars() {
        assert_eq!(token_preview("xy"), "xy…");
    }

    #[test]
    fn token_preview_empty_is_empty() {
        assert_eq!(token_preview(""), "");
    }

    #[test]
    fn absolute_url_native_returns_path_unchanged() {
        // Native target — no window — falls back to the raw path so the
        // helper stays usable from SSR / unit tests.
        assert_eq!(absolute_url("/api/widget/abc/data"), "/api/widget/abc/data");
    }

    #[test]
    fn copy_to_clipboard_is_a_noop_on_native() {
        // Smoke: the native path doesn't panic / network. The wasm32 path
        // is exercised by the browser smoke test, not unit tests.
        copy_to_clipboard("anything");
    }
}
