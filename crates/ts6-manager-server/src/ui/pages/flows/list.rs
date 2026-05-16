//! `/flows` — flow list page.
//!
//! Default landing for the Flows nav item. Renders an empty-state card if
//! no flows exist for the active server, otherwise a table per
//! `ui-brief.md` §3.1. Rows carry a `v1`/`graph` version badge and a
//! "Convert" action for legacy v1.1 flows (PURA-267).

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;
use ts6_manager_shared::flows::v2;

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
    ADMIN_ONLY_HINT, admin_only_title, enabled_badge_class, enabled_label, format_error,
    is_run_in_flight_conflict, run_status_badge_class, run_status_icon, run_status_label,
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

    let is_admin = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin"))
        .unwrap_or(false);

    let mut rows: Signal<Vec<v2::FlowView>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let virtual_server_id = active_server::DEFAULT_VIRTUAL_SERVER_ID;
    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.peek();
            let _ = reload.read();
            async move { fl::list_flow_views(gate, Some(virtual_server_id)).await }
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
            let body = v2::UpdateFlowBody {
                enabled: Some(!currently_enabled),
                ..Default::default()
            };
            spawn(async move {
                match fl::update_graph_flow(gate, flow, &body).await {
                    Ok(_) => {
                        toaster.push(
                            ToastVariant::Success,
                            if currently_enabled { "Disabled" } else { "Enabled" },
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

    let on_convert = {
        let gate = gate.clone();
        let mut bump = bump;
        move |flow: wire::FlowId| {
            let gate = gate.clone();
            spawn(async move {
                match fl::convert_flow(gate, flow).await {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Converted to graph flow", None);
                        bump();
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Convert failed",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

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
                    Err(e) if !force && is_run_in_flight_conflict(&e) => {
                        delete_prompt.set(DeletePrompt::Force(flow));
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            if force { "Force delete failed" } else { "Delete failed" },
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
                if is_admin {
                    Link {
                        to: Route::FlowFormPage {},
                        class: "btn btn-primary",
                        "+ New flow"
                    }
                } else {
                    Button {
                        variant: ButtonVariant::Primary,
                        disabled: true,
                        title: Some(ADMIN_ONLY_HINT.to_string()),
                        "+ New flow"
                    }
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
                FlowsTableSkeleton {}
            } else if rows.read().is_empty() {
                div { class: "empty",
                    div { class: "icon", aria_hidden: "true", "⚡" }
                    h3 { "No flows yet" }
                    p {
                        "Flows let you trigger an action when something happens — for example, send a welcome message when a client joins."
                    }
                    div { class: "actions",
                        if is_admin {
                            Link {
                                to: Route::FlowFormPage {},
                                class: "btn btn-primary",
                                "Create a flow"
                            }
                        } else {
                            Button {
                                variant: ButtonVariant::Primary,
                                disabled: true,
                                title: Some(ADMIN_ONLY_HINT.to_string()),
                                "Create a flow"
                            }
                        }
                    }
                }
            } else {
                FlowsTable {
                    is_admin,
                    rows: rows.read().clone(),
                    on_fire: EventHandler::new({
                        let on_fire = on_fire.clone();
                        move |id: wire::FlowId| on_fire(id)
                    }),
                    on_toggle_enabled: EventHandler::new({
                        let on_toggle = on_toggle_enabled.clone();
                        move |(id, en): (wire::FlowId, bool)| on_toggle(id, en)
                    }),
                    on_convert: EventHandler::new({
                        let on_convert = on_convert.clone();
                        move |id: wire::FlowId| on_convert(id)
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

// ── Table components ───────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct FlowsTableProps {
    rows: Vec<v2::FlowView>,
    is_admin: bool,
    on_fire: EventHandler<wire::FlowId>,
    on_toggle_enabled: EventHandler<(wire::FlowId, bool)>,
    on_convert: EventHandler<wire::FlowId>,
    on_delete: EventHandler<wire::FlowId>,
}

#[component]
fn FlowsTableSkeleton() -> Element {
    rsx! {
        div { aria_busy: "true",
            span { class: "sr-only", role: "status", "aria-live": "polite",
                "Loading flows…"
            }
            table { class: "data-table data-table--cards", aria_hidden: "true",
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
                    for i in 0..4 {
                        tr { key: "{i}",
                            td { "data-label": "Name",
                                div { class: "skeleton skeleton-line wide" }
                            }
                            td { "data-label": "Trigger",
                                div { class: "skeleton skeleton-line" }
                            }
                            td { "data-label": "Status",
                                div { class: "skeleton skeleton-line narrow" }
                            }
                            td { "data-label": "Last run",
                                div { class: "skeleton skeleton-line" }
                            }
                            td { class: "actions-col", "data-label": "Actions",
                                div { class: "skeleton skeleton-line narrow" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One-line trigger summary for a `FlowView`. v2 graph flows have no stored
/// definition on the FE side; they show "graph" instead of a trigger kind.
fn flow_trigger_label(f: &v2::FlowView) -> String {
    match f.definition.as_ref() {
        Some(def) => trigger_summary(&def.trigger),
        None => "graph".into(),
    }
}

/// Compact last-run summary from a `FlowRunSummaryView` — same badge +
/// relative-time approach as the v1.1 list, adapted to the v2 type.
fn last_run_cell(last: Option<&v2::FlowRunSummaryView>) -> Element {
    match last {
        None => rsx! { span { class: "muted", "Never run" } },
        Some(r) => {
            use crate::ui::pages::flows::shared::relative_when;
            let when = relative_when(r.started_at);
            let status = r.status;
            rsx! {
                span { class: "last-run-cell",
                    span { class: run_status_badge_class(status),
                        span { class: "flow-icon", aria_hidden: "true",
                            "{run_status_icon(status)}"
                        }
                        "{run_status_label(status)}"
                    }
                    span { class: "muted last-run-when", "{when}" }
                }
            }
        }
    }
}

#[component]
fn FlowsTable(props: FlowsTableProps) -> Element {
    rsx! {
        table { class: "data-table data-table--cards",
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
                        let is_admin = props.is_admin;
                        let on_fire = props.on_fire;
                        let on_toggle = props.on_toggle_enabled;
                        let on_convert = props.on_convert;
                        let on_delete = props.on_delete;
                        let trig = flow_trigger_label(&f);
                        let is_v1 = f.flow_version == 1;
                        let version_label = if is_v1 { "v1" } else { "graph" };
                        let version_class = if is_v1 {
                            "bot-badge bot-badge--off"
                        } else {
                            "bot-badge bot-badge--idle"
                        };
                        rsx! {
                            tr { key: "{id.0}",
                                td { class: "client-cell",
                                    Link {
                                        to: Route::FlowDetailPage { flow_id: id.0 },
                                        class: "client-name",
                                        "{f.name}"
                                    }
                                    span { class: "{version_class}", "{version_label}" }
                                    if let Some(d) = f.description.as_deref().filter(|s| !s.is_empty()) {
                                        span { class: "client-uid", "{d}" }
                                    }
                                }
                                td { "data-label": "Trigger", "{trig}" }
                                td { "data-label": "Status",
                                    span { class: enabled_badge_class(enabled),
                                        "{enabled_label(enabled)}"
                                    }
                                }
                                td { "data-label": "Last run",
                                    {last_run_cell(f.last_run.as_ref())}
                                }
                                td { class: "actions-col", "data-label": "Actions",
                                    Button {
                                        variant: ButtonVariant::Primary,
                                        size: ButtonSize::Small,
                                        disabled: !is_admin,
                                        title: admin_only_title(is_admin),
                                        onclick: move |_| on_fire.call(id),
                                        "Fire"
                                    }
                                    if is_admin {
                                        Link {
                                            to: Route::FlowEditPage { flow_id: id.0 },
                                            class: "btn btn-ghost btn-sm",
                                            "Edit"
                                        }
                                    } else {
                                        Button {
                                            variant: ButtonVariant::Ghost,
                                            size: ButtonSize::Small,
                                            disabled: true,
                                            title: Some(ADMIN_ONLY_HINT.to_string()),
                                            "Edit"
                                        }
                                    }
                                    if is_v1 && is_admin {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            title: Some("Convert this legacy flow to a v2 graph".to_string()),
                                            onclick: move |_| on_convert.call(id),
                                            "Convert"
                                        }
                                    } else {
                                        Button {
                                            variant: ButtonVariant::Secondary,
                                            size: ButtonSize::Small,
                                            disabled: !is_admin,
                                            title: admin_only_title(is_admin),
                                            onclick: move |_| on_toggle.call((id, enabled)),
                                            if enabled { "Disable" } else { "Enable" }
                                        }
                                    }
                                    Button {
                                        variant: ButtonVariant::Danger,
                                        size: ButtonSize::Small,
                                        disabled: !is_admin,
                                        title: admin_only_title(is_admin),
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
