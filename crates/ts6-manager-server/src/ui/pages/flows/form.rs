//! `/flows/new` and `/flows/{id}/edit` — v2 graph canvas editor.
//!
//! Both routes mount a three-pane `FlowCanvasEditor` (PURA-267) inside a
//! thin shell that owns the flow-level fields (name, description, enabled)
//! and drives the v2 REST API (`client::flows`). The canvas fires `on_save`
//! with the finalized `FlowGraph`; the shell stitches it into the full
//! create/update request body.
//!
//! v1.1 flows loaded in the edit view are automatically projected into a
//! read-only degenerate graph via `v2::project_legacy` so the canvas can
//! display them. Saving replaces the stored definition with a v2 graph.

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;
use ts6_manager_shared::flows::v2::{self, project_legacy};

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::flows as fl;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::pages::flows::canvas::{FlowCanvasEditor, starter_graph};
use crate::ui::pages::flows::shared::{MAX_NAME_LEN, format_error};
use crate::ui::routes::Route;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormMode {
    Create,
    Edit { flow_id: wire::FlowId },
}

/// `/flows/new` — canvas form for creating a new v2 graph flow.
#[component]
pub fn FlowFormPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let servers_ctx = use_servers_context();
    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let server_config_id = server.as_ref().map(|s| s.id).unwrap_or(0);
    let server_name = server
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "no server selected".into());

    rsx! {
        div { class: "crumb",
            Link { to: Route::FlowsListPage {}, "Flows" }
            " · New flow"
        }
        CanvasFormShell {
            mode: FormMode::Create,
            server_config_id,
            server_name,
            initial: None,
        }
    }
}

/// `/flows/{id}/edit` — canvas form for editing an existing flow.
///
/// Loads the flow as a [`v2::FlowView`] first; v1.1 flows are projected into
/// a degenerate graph via `project_legacy` so the canvas can display them.
#[component]
pub fn FlowEditPage(flow_id: i64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let flow = wire::FlowId(flow_id);
    let gate = use_auth_gate();
    let mut view: Signal<Option<v2::FlowView>> = use_signal(|| None);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fl::get_flow_view(gate, flow).await }
        }
    });
    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(f)) => {
            view.set(Some(f.clone()));
            error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    let snap = view.read().clone();
    let crumb_name = snap
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_else(|| format!("Flow {flow_id}"));

    rsx! {
        div { class: "crumb",
            Link { to: Route::FlowsListPage {}, "Flows" }
            " · "
            Link { to: Route::FlowDetailPage { flow_id }, "{crumb_name}" }
            " · Edit"
        }
        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load flow".to_string(),
                "{format_error(err)}"
            }
        }
        if *loading.read() && snap.is_none() {
            div { class: "card", aria_busy: "true",
                p { class: "muted", "Loading flow…" }
            }
        } else if let Some(f) = snap {
            CanvasFormShell {
                mode: FormMode::Edit { flow_id: f.id },
                server_config_id: f.server_config_id,
                server_name: format!("server #{}", f.server_config_id),
                initial: Some(f),
            }
        }
    }
}

// ── Canvas form shell ─────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct CanvasFormShellProps {
    mode: FormMode,
    server_config_id: i64,
    server_name: String,
    initial: Option<v2::FlowView>,
}

/// Identification + server header above the canvas. Owns name/description/
/// enabled state and issues the API call when the canvas fires `on_save`.
#[component]
fn CanvasFormShell(props: CanvasFormShellProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let nav = use_navigator();

    let init_name = props
        .initial
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_default();
    let init_desc = props
        .initial
        .as_ref()
        .and_then(|f| f.description.clone())
        .unwrap_or_default();
    let init_enabled = props.initial.as_ref().map(|f| f.enabled).unwrap_or(false);

    // Resolve the initial graph: v2 → use directly; v1.1 → project to a
    // degenerate path graph so the canvas can display the flow's structure.
    let init_graph = props
        .initial
        .as_ref()
        .and_then(|f| f.spec().ok())
        .map(|spec| match spec {
            v2::FlowSpec::Graph { graph } => graph,
            v2::FlowSpec::Legacy { definition } => project_legacy(&definition),
        })
        .unwrap_or_else(starter_graph);

    let mut name = use_signal(|| init_name);
    let mut description = use_signal(|| init_desc);
    let mut enabled = use_signal(|| init_enabled);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut form_error: Signal<Option<String>> = use_signal(|| None::<String>);

    let mode = props.mode;
    let server_config_id = props.server_config_id;
    let virtual_server_id = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let on_save = EventHandler::new(move |graph: v2::FlowGraph| {
        let trimmed_name = name.read().trim().to_string();
        if trimmed_name.is_empty() {
            form_error.set(Some("Name is required.".into()));
            return;
        }
        if trimmed_name.len() > MAX_NAME_LEN {
            form_error.set(Some(format!("Name is over {MAX_NAME_LEN} characters.")));
            return;
        }
        if server_config_id == 0 {
            form_error.set(Some(
                "Select a server first — flows are scoped to a server config.".into(),
            ));
            return;
        }
        let desc_value = description.read().trim().to_string();
        let description_opt = if desc_value.is_empty() {
            None
        } else {
            Some(desc_value)
        };
        let enabled_value = *enabled.read();
        // An enabled flow's definition is locked — don't swap the graph while
        // enabled (the API would reject it with 409 definition_swap_locked).
        let definition_locked = matches!(mode, FormMode::Edit { .. }) && enabled_value;

        submitting.set(true);
        form_error.set(None);
        let gate = gate.clone();
        spawn(async move {
            let result = match mode {
                FormMode::Create => {
                    let body = v2::CreateFlowBody {
                        name: trimmed_name.clone(),
                        description: description_opt,
                        server_config_id,
                        virtual_server_id,
                        enabled: enabled_value,
                        graph: Some(graph),
                        definition: None,
                    };
                    fl::create_graph_flow(gate, &body).await
                }
                FormMode::Edit { flow_id } => {
                    let body = v2::UpdateFlowBody {
                        name: Some(trimmed_name.clone()),
                        description: Some(description_opt),
                        virtual_server_id: Some(virtual_server_id),
                        enabled: Some(enabled_value),
                        graph: if definition_locked { None } else { Some(graph) },
                        definition: None,
                    };
                    fl::update_graph_flow(gate, flow_id, &body).await
                }
            };
            submitting.set(false);
            match result {
                Ok(f) => {
                    toaster.push(
                        ToastVariant::Success,
                        match mode {
                            FormMode::Create => "Flow created",
                            FormMode::Edit { .. } => "Flow updated",
                        },
                        Some(f.name.clone()),
                    );
                    nav.push(Route::FlowDetailPage { flow_id: f.id.0 });
                }
                Err(e) => {
                    form_error.set(Some(format_error(&e)));
                }
            }
        });
    });

    let definition_locked = matches!(mode, FormMode::Edit { .. }) && *enabled.read();

    rsx! {
        div { class: "stack-lg",
            // ── Identification ─────────────────────────────────────
            section { class: "card stack-md",
                h2 { "Identification" }
                label { class: "field",
                    span { class: "field-label", "Name" }
                    input {
                        class: "input",
                        value: "{name.read()}",
                        maxlength: "{MAX_NAME_LEN}",
                        placeholder: "welcome-on-join",
                        oninput: move |e| name.set(e.value()),
                    }
                    span { class: "field-hint",
                        "Unique per virtual server. {MAX_NAME_LEN} chars max."
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Description (optional)" }
                    textarea {
                        class: "input",
                        rows: "2",
                        maxlength: "280",
                        placeholder: "What this flow does, in 280 chars or fewer.",
                        value: "{description.read()}",
                        oninput: move |e| description.set(e.value()),
                    }
                }
            }

            // ── Target server ────────────────────────────────────
            section { class: "card stack-sm",
                h2 { "Target server" }
                p { class: "muted",
                    "Scoped to ", strong { "{props.server_name}" }, "."
                }
                details { class: "disclosure",
                    summary { "Show advanced identifiers" }
                    p { class: "muted",
                        "Server config ", code { "{server_config_id}" },
                        " · virtual server ", code { "{virtual_server_id}" }, "."
                    }
                }
            }

            // ── Enable toggle + lock warning ─────────────────────
            section { class: "card stack-sm",
                if definition_locked {
                    Banner { variant: BannerVariant::Warning,
                        "This flow is enabled so its graph is locked. Uncheck "
                        strong { "Enable on save" }
                        " and save to unlock it."
                    }
                }
                label { class: "field-inline",
                    input {
                        r#type: "checkbox",
                        checked: *enabled.read(),
                        oninput: move |e| enabled.set(e.value() == "true"),
                    }
                    " Enable on save"
                    span { class: "field-hint",
                        "An enabled flow starts responding to its trigger immediately. Its graph is then locked until you disable it again."
                    }
                }
            }

            if let Some(msg) = form_error.read().as_ref() {
                Banner { variant: BannerVariant::Danger, title: "Save failed".to_string(),
                    "{msg}"
                }
            }

            // ── Canvas ───────────────────────────────────────────
            // The canvas owns the graph state and fires `on_save` when the
            // operator clicks Save (only available when !has_errors &&
            // on_save is Some). The shell's name/desc/enabled are stitched
            // in by the handler above.
            FlowCanvasEditor {
                initial: init_graph,
                read_only: definition_locked,
                run_overlay: None,
                on_save: Some(on_save),
            }
        }
    }
}

// ── Shared helpers kept for other callers in this crate ───────────────────

/// Render a `serde_json::Value` as a string for kv editors. Strings are
/// unwrapped; other types serialise verbatim.
pub fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Inverse of [`value_to_string`]. Parses an input as JSON first (so
/// numbers, bools, arrays round-trip); falls back to a plain string.
pub fn string_to_value(s: &str) -> serde_json::Value {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return serde_json::Value::String(String::new());
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return v;
    }
    serde_json::Value::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_value_round_trip_handles_strings_and_numbers() {
        assert_eq!(value_to_string(&serde_json::json!("hello")), "hello");
        assert_eq!(value_to_string(&serde_json::json!(5)), "5");
        assert_eq!(value_to_string(&serde_json::json!(true)), "true");
    }

    #[test]
    fn string_to_value_prefers_typed_when_parseable() {
        assert_eq!(string_to_value("5"), serde_json::json!(5));
        assert_eq!(string_to_value("true"), serde_json::json!(true));
        assert_eq!(string_to_value("\"hi\""), serde_json::json!("hi"));
        assert_eq!(string_to_value("hello"), serde_json::json!("hello"));
        assert_eq!(string_to_value(""), serde_json::json!(""));
    }
}
