//! `/flows` — flow list page.
//!
//! Default landing for the Flows nav item. Renders an empty-state card if
//! no flows exist for the active server, otherwise a table per
//! `ui-brief.md` §3.1.

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::flows as fl;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::pages::flows::dialog::{ConfirmDialog, DeletePrompt};
use crate::ui::pages::flows::shared::{
    enabled_badge_class, enabled_label, format_error, is_run_in_flight_conflict, last_run_cell,
    trigger_summary,
};
use crate::ui::routes::Route;

#[component]
pub fn FlowsListPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let storage = session.storage.clone();
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let servers_ctx = use_servers_context();
    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);

    let mut rows: Signal<Vec<wire::Flow>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let virtual_server_id = active_server::DEFAULT_VIRTUAL_SERVER_ID;
    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            // `peek()` so the resource subscribes once and we drive
            // refetches by bumping the dependency token below — avoids
            // the read/set deadlock pattern documented in PURA-132.
            let _ = *reload.peek();
            let _ = reload.read();
            async move { fl::list_flows(gate, Some(virtual_server_id)).await }
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

    let on_fire = {
        let gate = gate.clone();
        move |flow: wire::FlowId| {
            let gate = gate.clone();
            spawn(async move {
                match fl::fire_flow(gate, flow, None).await {
                    Ok(resp) => toaster.push(
                        ToastVariant::Success,
                        format!("Fired run #{}", resp.run_id.0),
                        None,
                    ),
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Fire failed", Some(format_error(&e)))
                    }
                }
            });
        }
    };

    let on_toggle_enabled = {
        let gate = gate.clone();
        let mut bump = bump;
        move |flow: wire::FlowId, currently_enabled: bool| {
            let gate = gate.clone();
            let body = wire::UpdateFlowRequest {
                enabled: Some(!currently_enabled),
                ..Default::default()
            };
            spawn(async move {
                match fl::update_flow(gate, flow, &body).await {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            if currently_enabled {
                                "Disabled"
                            } else {
                                "Enabled"
                            },
                            None,
                        );
                        bump();
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Update failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    // Delete is a two-stage, explicit-confirm flow (PURA-246 B1): a row
    // click only opens the confirm dialog; the actual `delete_flow` call
    // happens on dialog-confirm, and a `run_in_flight` 409 re-prompts with
    // an explicit force choice rather than silently escalating.
    let delete_prompt: Signal<DeletePrompt> = use_signal(|| DeletePrompt::Closed);
    let deleting: Signal<bool> = use_signal(|| false);

    let on_confirm_delete = {
        let gate = gate.clone();
        let mut bump = bump;
        move |_| {
            let mut delete_prompt = delete_prompt;
            let mut deleting = deleting;
            let (flow, force) = match *delete_prompt.read() {
                DeletePrompt::Confirm(id) => (id, false),
                DeletePrompt::Force(id) => (id, true),
                DeletePrompt::Closed => return,
            };
            let gate = gate.clone();
            spawn(async move {
                deleting.set(true);
                let result = fl::delete_flow(gate, flow, force).await;
                deleting.set(false);
                match result {
                    Ok(()) => {
                        if force {
                            toaster.push(
                                ToastVariant::Warning,
                                "Force-deleted flow",
                                Some("Interrupted the in-flight run.".into()),
                            );
                        } else {
                            toaster.push(ToastVariant::Success, "Deleted flow", None);
                        }
                        delete_prompt.set(DeletePrompt::Closed);
                        bump();
                    }
                    // 409 on a non-force delete → escalate to an explicit
                    // force-delete prompt; the operator decides.
                    Err(e) if !force && is_run_in_flight_conflict(&e) => {
                        delete_prompt.set(DeletePrompt::Force(flow));
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            if force {
                                "Force delete failed"
                            } else {
                                "Delete failed"
                            },
                            Some(format_error(&e)),
                        );
                        delete_prompt.set(DeletePrompt::Closed);
                    }
                }
            });
        }
    };

    let footer_copy = match server.as_ref() {
        Some(s) => format!(
            "Flows run on this manager only. Scoped to virtual server {} on {}.",
            virtual_server_id, s.name
        ),
        None => {
            "Flows run on this manager only. (Multi-manager support is a future feature.)".into()
        }
    };

    rsx! {
        div { class: "crumb", "Flows" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Flows" }
                p { class: "page-lede",
                    "Run an action when something happens. For example: send a welcome message when a client joins, or hit a webhook every five minutes."
                }
            }
            div { class: "page-actions",
                Link {
                    to: Route::FlowFormPage {},
                    class: "btn btn-primary",
                    "+ New flow"
                }
            }
        }

        if let Some(err) = error.read().as_ref() {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load flows".to_string(),
                "{format_error(err)}"
            }
        }

        section { class: "stack-md",
            if *loading.read() && rows.read().is_empty() {
                div { class: "card", aria_busy: "true",
                    p { class: "muted", "Loading flows…" }
                }
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", aria_hidden: "true", "⚡" }
                    h3 { "No flows yet" }
                    p {
                        "Flows let you trigger an action when something happens — for example, send a welcome message when a client joins."
                    }
                    div { class: "actions",
                        Link {
                            to: Route::FlowFormPage {},
                            class: "btn btn-primary",
                            "Create a flow"
                        }
                    }
                }
            } else {
                FlowsTable {
                    rows: rows.read().clone(),
                    on_fire: EventHandler::new({
                        let on_fire = on_fire.clone();
                        move |id: wire::FlowId| on_fire(id)
                    }),
                    on_toggle_enabled: EventHandler::new({
                        let on_toggle = on_toggle_enabled.clone();
                        move |(id, en): (wire::FlowId, bool)| on_toggle(id, en)
                    }),
                    on_delete: EventHandler::new(move |id: wire::FlowId| {
                        let mut delete_prompt = delete_prompt;
                        delete_prompt.set(DeletePrompt::Confirm(id));
                    }),
                }
            }
        }

        footer { class: "muted",
            p { "{footer_copy}" }
        }

        if delete_prompt.read().is_open() {
            {
                let is_force = delete_prompt.read().is_force();
                let on_confirm = on_confirm_delete.clone();
                rsx! {
                    ConfirmDialog {
                        title: if is_force {
                            "A run is in flight".to_string()
                        } else {
                            "Delete this flow?".to_string()
                        },
                        message: if is_force {
                            "This flow has a run in progress. Force-deleting will interrupt the running flow and remove its run history. This cannot be undone.".to_string()
                        } else {
                            "Run history will be removed. This cannot be undone.".to_string()
                        },
                        confirm_label: if is_force {
                            "Force delete".to_string()
                        } else {
                            "Delete".to_string()
                        },
                        busy: *deleting.read(),
                        on_confirm: move |_| on_confirm(()),
                        on_cancel: move |_| {
                            let mut delete_prompt = delete_prompt;
                            delete_prompt.set(DeletePrompt::Closed);
                        },
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct FlowsTableProps {
    rows: Vec<wire::Flow>,
    on_fire: EventHandler<wire::FlowId>,
    on_toggle_enabled: EventHandler<(wire::FlowId, bool)>,
    on_delete: EventHandler<wire::FlowId>,
}

#[component]
fn FlowsTable(props: FlowsTableProps) -> Element {
    rsx! {
        table { class: "data-table",
            "aria-label": "Flows",
            thead {
                tr {
                    th { scope: "col", "Name" }
                    th { scope: "col", "Trigger" }
                    th { scope: "col", "Status" }
                    th { scope: "col", "Last run" }
                    th { scope: "col", class: "actions-col", "Actions" }
                }
            }
            tbody {
                for f in props.rows.iter() {
                    {
                        let f = f.clone();
                        let id = f.id;
                        let enabled = f.enabled;
                        let on_fire = props.on_fire;
                        let on_toggle = props.on_toggle_enabled;
                        let on_delete = props.on_delete;
                        let trig = trigger_summary(&f.definition.trigger);
                        let last = last_run_cell(f.last_run.as_ref());
                        rsx! {
                            tr { key: "{id.0}",
                                td { class: "client-cell",
                                    Link {
                                        to: Route::FlowDetailPage { flow_id: id.0 },
                                        class: "client-name",
                                        "{f.name}"
                                    }
                                    if let Some(d) = f.description.as_deref().filter(|s| !s.is_empty()) {
                                        span { class: "client-uid", "{d}" }
                                    }
                                }
                                td { "{trig}" }
                                td {
                                    span { class: enabled_badge_class(enabled),
                                        "{enabled_label(enabled)}"
                                    }
                                }
                                td { "{last}" }
                                td { class: "actions-col",
                                    Button {
                                        variant: ButtonVariant::Primary,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_fire.call(id),
                                        "Fire"
                                    }
                                    Link {
                                        to: Route::FlowEditPage { flow_id: id.0 },
                                        class: "btn btn-ghost btn-sm",
                                        "Edit"
                                    }
                                    Button {
                                        variant: ButtonVariant::Secondary,
                                        size: ButtonSize::Small,
                                        onclick: move |_| on_toggle.call((id, enabled)),
                                        if enabled { "Disable" } else { "Enable" }
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
