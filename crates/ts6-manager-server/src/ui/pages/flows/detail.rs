//! `/flows/{id}` — per-flow operator surface.
//!
//! Tabs: Runs (default) + Definition. Runs is the operator's debugging
//! surface; Definition is the read-only canvas view of the flow graph
//! (edit lives at `/flows/{id}/edit`).

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;
use ts6_manager_shared::flows::v2::{self, project_legacy};

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::flows as fl;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::pages::flows::canvas::FlowCanvasEditor;
use crate::ui::pages::flows::dialog::{ConfirmDialog, DeletePrompt};
use crate::ui::pages::flows::shared::{
    ADMIN_ONLY_HINT, action_status_badge_class, action_status_icon, action_status_label,
    action_wire_kind_icon, action_wire_kind_label, admin_only_title, enabled_badge_class,
    enabled_label, format_error, is_run_in_flight_conflict, relative_when, run_status_badge_class,
    run_status_hint, run_status_icon, run_status_label, trigger_summary,
};
use crate::ui::routes::Route;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DetailTab {
    Runs,
    Definition,
}

#[component]
pub fn FlowDetailPage(flow_id: i64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let flow = wire::FlowId(flow_id);
    let gate = use_auth_gate();
    let toaster = use_toaster();

    // PURA-248 M5 — the role rides in the session blob client-side, so the
    // write affordances can be suppressed up front rather than waiting for
    // a 403. The route layer remains the real enforcement point.
    let is_admin = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin"))
        .unwrap_or(false);

    let mut detail: Signal<Option<v2::FlowView>> = use_signal(|| None);
    let mut detail_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut detail_loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let flow_snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.peek();
            let _ = reload.read();
            async move { fl::get_flow_view(gate, flow).await }
        }
    });

    use_effect(move || match &*flow_snapshot.read_unchecked() {
        Some(Ok(f)) => {
            detail.set(Some(f.clone()));
            detail_error.set(None);
            detail_loading.set(false);
        }
        Some(Err(e)) => {
            detail_error.set(Some(e.clone()));
            detail_loading.set(false);
        }
        None => detail_loading.set(true),
    });

    let mut tab: Signal<DetailTab> = use_signal(|| DetailTab::Runs);
    let runs: Signal<Vec<wire::FlowRun>> = use_signal(Vec::new);
    let next_cursor: Signal<Option<wire::FlowRunId>> = use_signal(|| None);
    let runs_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let runs_loading: Signal<bool> = use_signal(|| false);
    let mut runs_reload: Signal<u64> = use_signal(|| 0u64);

    let load_runs = {
        let gate = gate.clone();
        let mut runs_loading = runs_loading;
        let mut runs_error = runs_error;
        let mut runs = runs;
        let mut next_cursor = next_cursor;
        move |cursor: Option<wire::FlowRunId>, append: bool| {
            let gate = gate.clone();
            spawn(async move {
                runs_loading.set(true);
                match fl::list_runs(gate, flow, Some(25), cursor).await {
                    Ok(resp) => {
                        next_cursor.set(resp.next_cursor);
                        if append {
                            runs.with_mut(|v| v.extend(resp.runs));
                        } else {
                            runs.set(resp.runs);
                        }
                        runs_error.set(None);
                    }
                    Err(e) => runs_error.set(Some(e)),
                }
                runs_loading.set(false);
            });
        }
    };

    // Initial runs fetch + reload bump.
    use_effect({
        let load_runs = load_runs.clone();
        move || {
            let _ = *runs_reload.read();
            load_runs(None, false);
        }
    });

    let bump_flow = move || reload.with_mut(|n| *n += 1);
    let bump_runs = move || runs_reload.with_mut(|n| *n += 1);

    let on_fire = {
        let gate = gate.clone();
        let mut bump_runs = bump_runs;
        move |_| {
            let gate = gate.clone();
            spawn(async move {
                match fl::fire_flow(gate, flow, None).await {
                    Ok(resp) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Fired run #{}", resp.run_id.0),
                            None,
                        );
                        bump_runs();
                    }
                    Err(e) => {
                        toaster.push(ToastVariant::Danger, "Fire failed", Some(format_error(&e)))
                    }
                }
            });
        }
    };

    let on_toggle_enabled = {
        let gate = gate.clone();
        let mut bump_flow = bump_flow;
        move |currently_enabled: bool| {
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
                            if currently_enabled {
                                "Disabled"
                            } else {
                                "Enabled"
                            },
                            None,
                        );
                        bump_flow();
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

    // Delete is a two-stage, explicit-confirm flow (PURA-246 B1): the
    // header button only opens the confirm dialog; a `run_in_flight` 409
    // re-prompts with an explicit force choice rather than auto-escalating.
    let delete_prompt: Signal<DeletePrompt> = use_signal(|| DeletePrompt::Closed);
    let deleting: Signal<bool> = use_signal(|| false);

    let on_confirm_delete = {
        let gate = gate.clone();
        let nav = use_navigator();
        move |_| {
            let mut delete_prompt = delete_prompt;
            let mut deleting = deleting;
            let force = match *delete_prompt.read() {
                DeletePrompt::Confirm(_) => false,
                DeletePrompt::Force(_) => true,
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
                        nav.push(Route::FlowsListPage {});
                    }
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

    let snap = detail.read().clone();
    let crumb_name = snap
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_else(|| format!("Flow {flow_id}"));

    rsx! {
        div { class: "crumb",
            Link { to: Route::FlowsListPage {}, "Flows" }
            " · "
            "{crumb_name}"
        }

        if let Some(err) = detail_error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load flow".to_string(),
                "{format_error(err)}"
            }
        }

        if *detail_loading.read() && snap.is_none() {
            div { class: "card", aria_busy: "true",
                p { class: "muted", "Loading flow…" }
            }
        } else if let Some(f) = snap {
            FlowHeader {
                flow: f.clone(),
                is_admin,
                on_fire: EventHandler::new({
                    let on_fire = on_fire.clone();
                    move |_| on_fire(())
                }),
                on_toggle_enabled: EventHandler::new({
                    let on_toggle = on_toggle_enabled.clone();
                    move |en: bool| on_toggle(en)
                }),
                on_delete: EventHandler::new(move |_| {
                    let mut delete_prompt = delete_prompt;
                    delete_prompt.set(DeletePrompt::Confirm(flow));
                }),
            }

            // Tab bar — APG tabs pattern (PURA-248 M6): roving tabindex,
            // arrow/Home/End navigation, and each tab `aria-controls` its
            // panel. The panels carry the matching `id` + `aria-labelledby`.
            {
                let runs_active = *tab.read() == DetailTab::Runs;
                let on_tab_keydown = move |evt: KeyboardEvent| {
                    let next = match evt.key() {
                        Key::ArrowRight | Key::ArrowDown => Some(DetailTab::Definition),
                        Key::ArrowLeft | Key::ArrowUp => Some(DetailTab::Runs),
                        Key::Home => Some(DetailTab::Runs),
                        Key::End => Some(DetailTab::Definition),
                        _ => None,
                    };
                    if let Some(t) = next {
                        evt.prevent_default();
                        tab.set(t);
                        #[cfg(target_arch = "wasm32")]
                        focus_element(match t {
                            DetailTab::Runs => "flow-tab-runs",
                            DetailTab::Definition => "flow-tab-definition",
                        });
                    }
                };
                rsx! {
                    div { class: "tabs", role: "tablist", "aria-label": "Flow detail sections",
                        button {
                            r#type: "button",
                            role: "tab",
                            id: "flow-tab-runs",
                            class: if runs_active { "tab is-active" } else { "tab" },
                            "aria-selected": if runs_active { "true" } else { "false" },
                            "aria-controls": "flow-panel-runs",
                            tabindex: if runs_active { "0" } else { "-1" },
                            onclick: move |_| tab.set(DetailTab::Runs),
                            onkeydown: on_tab_keydown,
                            "Runs"
                        }
                        button {
                            r#type: "button",
                            role: "tab",
                            id: "flow-tab-definition",
                            class: if !runs_active { "tab is-active" } else { "tab" },
                            "aria-selected": if !runs_active { "true" } else { "false" },
                            "aria-controls": "flow-panel-definition",
                            tabindex: if !runs_active { "0" } else { "-1" },
                            onclick: move |_| tab.set(DetailTab::Definition),
                            onkeydown: on_tab_keydown,
                            "Definition"
                        }
                    }
                }
            }

            if *tab.read() == DetailTab::Runs {
                RunsPanel {
                    runs: runs.read().clone(),
                    error: runs_error.read().clone(),
                    loading: *runs_loading.read(),
                    has_more: next_cursor.read().is_some(),
                    on_refresh: EventHandler::new({
                        let mut bump_runs = bump_runs;
                        move |_| bump_runs()
                    }),
                    on_load_more: EventHandler::new({
                        let load_runs = load_runs.clone();
                        let cursor = *next_cursor.read();
                        move |_| load_runs(cursor, true)
                    }),
                }
            } else {
                DefinitionPanel { flow: f, is_admin }
            }
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
struct FlowHeaderProps {
    flow: v2::FlowView,
    is_admin: bool,
    on_fire: EventHandler<()>,
    on_toggle_enabled: EventHandler<bool>,
    on_delete: EventHandler<()>,
}

#[component]
fn FlowHeader(props: FlowHeaderProps) -> Element {
    let f = props.flow.clone();
    let is_admin = props.is_admin;
    let on_fire = props.on_fire;
    let on_toggle = props.on_toggle_enabled;
    let on_delete = props.on_delete;
    let enabled = f.enabled;
    let edit_route = Route::FlowEditPage { flow_id: f.id.0 };
    let trig_label = f
        .definition
        .as_ref()
        .map(|d| trigger_summary(&d.trigger))
        .unwrap_or_else(|| "graph".into());
    rsx! {
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "{f.name}" }
                p { class: "page-lede",
                    span { class: enabled_badge_class(enabled),
                        "{enabled_label(enabled)}"
                    }
                    " · "
                    "{trig_label}"
                }
                if let Some(d) = f.description.as_deref().filter(|s| !s.is_empty()) {
                    p { class: "muted", "{d}" }
                }
                if !is_admin {
                    p { class: "muted", "Read-only access — firing, editing and deleting flows is admin-only." }
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    disabled: !is_admin,
                    title: admin_only_title(is_admin),
                    onclick: move |_| on_fire.call(()),
                    "Fire"
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    disabled: !is_admin,
                    title: admin_only_title(is_admin),
                    onclick: move |_| on_toggle.call(enabled),
                    if enabled { "Disable" } else { "Enable" }
                }
                if is_admin {
                    Link {
                        to: edit_route,
                        class: "btn btn-ghost",
                        "Edit"
                    }
                } else {
                    Button {
                        variant: ButtonVariant::Ghost,
                        disabled: true,
                        title: Some(ADMIN_ONLY_HINT.to_string()),
                        "Edit"
                    }
                }
                Button {
                    variant: ButtonVariant::Danger,
                    disabled: !is_admin,
                    title: admin_only_title(is_admin),
                    onclick: move |_| on_delete.call(()),
                    "Delete"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct RunsPanelProps {
    runs: Vec<wire::FlowRun>,
    error: Option<ApiError>,
    loading: bool,
    has_more: bool,
    on_refresh: EventHandler<()>,
    on_load_more: EventHandler<()>,
}

#[component]
fn RunsPanel(props: RunsPanelProps) -> Element {
    let runs = props.runs.clone();
    let err = props.error.clone();
    let loading = props.loading;
    let has_more = props.has_more;
    let on_refresh = props.on_refresh;
    let on_load_more = props.on_load_more;

    // PURA-248 H1 — the run whose per-action `actionResults[]` breakdown is
    // shown in the slide-out drawer. `None` keeps the drawer closed.
    let mut selected_run: Signal<Option<wire::FlowRun>> = use_signal(|| None);

    rsx! {
        section {
            role: "tabpanel",
            id: "flow-panel-runs",
            "aria-labelledby": "flow-tab-runs",
            class: "stack-md",
            if let Some(e) = err.as_ref() {
                Banner { variant: BannerVariant::Danger, title: "Could not load runs".to_string(),
                    "{format_error(e)}"
                }
            }
            if runs.is_empty() && !loading {
                div { class: "empty",
                    div { class: "icon", aria_hidden: "true", "↻" }
                    h3 { "No runs yet" }
                    p { "Hit ", strong { "Fire" }, " to make the engine run this flow on demand." }
                }
            } else {
                // `data-table--cards` (PURA-246 R1) — the six-column runs
                // table reflows into stacked cards on viewports ≤768px so the
                // Details action is never pushed off-screen.
                table { class: "data-table data-table--cards",
                    "aria-label": "Flow runs",
                    thead {
                        tr {
                            th { scope: "col", "When" }
                            th { scope: "col", "Trigger" }
                            th { scope: "col", "Status" }
                            th { scope: "col", "Duration" }
                            th { scope: "col", "Error" }
                            th { scope: "col", class: "actions-col", "Details" }
                        }
                    }
                    tbody {
                        for r in runs.iter() {
                            {
                                let r = r.clone();
                                let row_run = r.clone();
                                let when = relative_when(r.summary.started_at);
                                let trig_kind = r
                                    .trigger
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let dur = r
                                    .summary
                                    .duration_ms
                                    .map(|d| format!("{d} ms"))
                                    .unwrap_or_else(|| "—".into());
                                let err = r.error.clone().unwrap_or_else(|| "—".into());
                                let hint = run_status_hint(r.summary.status);
                                let run_id = r.summary.id.0;
                                let action_count = r.action_results.len();
                                rsx! {
                                    tr { key: "{run_id}",
                                        td { "data-label": "When", "{when}" }
                                        td { "data-label": "Trigger", "{trig_kind}" }
                                        td { "data-label": "Status",
                                            span { class: run_status_badge_class(r.summary.status),
                                                title: hint.unwrap_or(""),
                                                span { class: "flow-icon", aria_hidden: "true",
                                                    "{run_status_icon(r.summary.status)}"
                                                }
                                                "{run_status_label(r.summary.status)}"
                                            }
                                        }
                                        td { "data-label": "Duration", "{dur}" }
                                        td { class: "muted", "data-label": "Error", "{err}" }
                                        td { class: "actions-col", "data-label": "Details",
                                            Button {
                                                variant: ButtonVariant::Ghost,
                                                size: ButtonSize::Small,
                                                aria_label: Some(format!("View action results for run #{run_id}")),
                                                onclick: move |_| selected_run.set(Some(row_run.clone())),
                                                {format!("View ({action_count})")}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "actions",
                // PURA-246 R3 — Secondary (bordered) so Refresh reads as a
                // button atom, not unstyled text, consistent with the other
                // controls on the flow surfaces.
                Button {
                    variant: ButtonVariant::Secondary,
                    size: ButtonSize::Small,
                    onclick: move |_| on_refresh.call(()),
                    loading,
                    "Refresh"
                }
                if has_more {
                    Button {
                        variant: ButtonVariant::Secondary,
                        size: ButtonSize::Small,
                        onclick: move |_| on_load_more.call(()),
                        "Load more"
                    }
                }
            }
            RunStatusLegend {}
        }
        if let Some(run) = selected_run.read().clone() {
            RunDrawer {
                run,
                on_close: EventHandler::new(move |_| selected_run.set(None)),
            }
        }
    }
}

/// PURA-248 L7 + M8 — a collapsible legend for the run-status pills. The
/// `skipped_disabled` explainer (M8) used to live only in a `title=`
/// attribute on the pill, which is unreachable by keyboard and touch;
/// folding it into the `<details>` legend makes it reachable for everyone.
#[component]
fn RunStatusLegend() -> Element {
    let entries: [(wire::FlowRunStatus, &str); 5] = [
        (
            wire::FlowRunStatus::Ok,
            "Every action in the flow completed without an error.",
        ),
        (
            wire::FlowRunStatus::Errored,
            "At least one action failed — open the run to see which one.",
        ),
        (wire::FlowRunStatus::InFlight, "The run is still executing."),
        (
            wire::FlowRunStatus::Interrupted,
            "The run was cut short — usually a force-delete or a manager restart.",
        ),
        (
            wire::FlowRunStatus::SkippedDisabled,
            run_status_hint(wire::FlowRunStatus::SkippedDisabled).unwrap_or(""),
        ),
    ];
    rsx! {
        details { class: "disclosure",
            summary { "What do the run statuses mean?" }
            ul { class: "stack-sm legend",
                for (status, copy) in entries {
                    li { key: "{run_status_label(status)}", class: "legend-row",
                        span { class: run_status_badge_class(status),
                            span { class: "flow-icon", aria_hidden: "true",
                                "{run_status_icon(status)}"
                            }
                            "{run_status_label(status)}"
                        }
                        span { class: "muted", "{copy}" }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct RunDrawerProps {
    run: wire::FlowRun,
    on_close: EventHandler<()>,
}

/// PURA-248 H1 — slide-out panel with one row per planned action of a
/// single run (`kind` / `status` / `durationMs` / `error`), per
/// `ui-brief.md` §3.3. Built on the shared `.drawer` primitive; Escape and
/// backdrop-click dismiss it, mirroring `flows::dialog::ConfirmDialog`.
#[component]
fn RunDrawer(props: RunDrawerProps) -> Element {
    let run = props.run.clone();
    let on_close = props.on_close;
    let summary = run.summary.clone();
    let when = relative_when(summary.started_at);
    let dur = summary
        .duration_ms
        .map(|d| format!("{d} ms"))
        .unwrap_or_else(|| "—".into());
    let trig_kind = run
        .trigger
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let run_error = run.error.clone();
    let results = run.action_results.clone();
    rsx! {
        div {
            class: "drawer-backdrop",
            onclick: move |_| on_close.call(()),
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    evt.prevent_default();
                    on_close.call(());
                }
            },
            div {
                class: "drawer",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "flow-run-drawer-title",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "drawer-header",
                    h2 { id: "flow-run-drawer-title", "Run #{summary.id.0}" }
                    button {
                        r#type: "button",
                        class: "btn btn-ghost btn-sm",
                        "aria-label": "Close run details",
                        autofocus: true,
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "drawer-body stack-md",
                    div { class: "stack-sm",
                        p {
                            span { class: run_status_badge_class(summary.status),
                                span { class: "flow-icon", aria_hidden: "true",
                                    "{run_status_icon(summary.status)}"
                                }
                                "{run_status_label(summary.status)}"
                            }
                        }
                        p { class: "muted", "Started ", "{when}", " · trigger ", code { "{trig_kind}" }, " · {dur}" }
                        if let Some(e) = run_error.as_ref() {
                            Banner { variant: BannerVariant::Danger, title: "Run error".to_string(),
                                "{e}"
                            }
                        }
                    }
                    section { class: "stack-sm",
                        h3 { "Action results" }
                        if results.is_empty() {
                            p { class: "muted",
                                "No action results were recorded for this run — it ended before any action ran."
                            }
                        } else {
                            table { class: "data-table", "aria-label": "Action results",
                                thead {
                                    tr {
                                        th { scope: "col", "#" }
                                        th { scope: "col", "Action" }
                                        th { scope: "col", "Status" }
                                        th { scope: "col", "Duration" }
                                        th { scope: "col", "Error" }
                                    }
                                }
                                tbody {
                                    for ar in results.iter() {
                                        {
                                            let ar = ar.clone();
                                            let ar_err = ar.error.clone().unwrap_or_else(|| "—".into());
                                            rsx! {
                                                tr { key: "{ar.index}",
                                                    td { "{ar.index + 1}" }
                                                    td {
                                                        span { class: "flow-icon", aria_hidden: "true",
                                                            "{action_wire_kind_icon(&ar.kind)}"
                                                        }
                                                        " {action_wire_kind_label(&ar.kind)}"
                                                    }
                                                    td {
                                                        span { class: action_status_badge_class(ar.status),
                                                            span { class: "flow-icon", aria_hidden: "true",
                                                                "{action_status_icon(ar.status)}"
                                                            }
                                                            "{action_status_label(ar.status)}"
                                                        }
                                                    }
                                                    td { "{ar.duration_ms} ms" }
                                                    td { class: "muted", "{ar_err}" }
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
}

#[derive(Props, Clone, PartialEq)]
struct DefinitionPanelProps {
    flow: v2::FlowView,
    is_admin: bool,
}

/// Read-only canvas view of the flow graph. v1.1 flows are projected into a
/// degenerate path graph via `project_legacy` so the canvas can display them.
#[component]
fn DefinitionPanel(props: DefinitionPanelProps) -> Element {
    let f = props.flow.clone();
    let is_admin = props.is_admin;
    let definition_locked = f.enabled;

    let graph = f
        .spec()
        .ok()
        .map(|spec| match spec {
            v2::FlowSpec::Graph { graph } => graph,
            v2::FlowSpec::Legacy { definition } => project_legacy(&definition),
        })
        .unwrap_or_else(|| v2::FlowGraph {
            nodes: vec![],
            edges: vec![],
        });

    rsx! {
        section {
            role: "tabpanel",
            id: "flow-panel-definition",
            "aria-labelledby": "flow-tab-definition",
            class: "stack-md",
            div { class: "actions",
                if !is_admin {
                    Button {
                        variant: ButtonVariant::Secondary,
                        disabled: true,
                        title: Some(ADMIN_ONLY_HINT.to_string()),
                        "Edit graph"
                    }
                } else if definition_locked {
                    Button {
                        variant: ButtonVariant::Secondary,
                        disabled: true,
                        title: Some(
                            "Disable the flow first — an enabled flow's graph is locked.".to_string(),
                        ),
                        "Edit graph (disable first)"
                    }
                } else {
                    Link {
                        to: Route::FlowEditPage { flow_id: f.id.0 },
                        class: "btn btn-secondary",
                        "Edit graph"
                    }
                }
            }
            FlowCanvasEditor {
                initial: graph,
                read_only: true,
                run_overlay: None,
                on_save: None,
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ActionSummaryProps {
    action: wire::Action,
}

#[component]
fn ActionSummary(props: ActionSummaryProps) -> Element {
    match props.action {
        wire::Action::Ts6Command { command, args } => rsx! {
            p { class: "muted",
                "command: ", code { "{command}" },
                if !args.is_empty() {
                    " · args: "
                    code { "{serde_json::to_string(&args).unwrap_or_default()}" }
                }
            }
        },
        wire::Action::MusicBotCommand {
            bot_id,
            command,
            args,
        } => rsx! {
            p { class: "muted",
                "bot: ", code { "{bot_id}" },
                " · command: ", code { "{command}" },
                if !args.is_empty() {
                    " · args: "
                    code { "{serde_json::to_string(&args).unwrap_or_default()}" }
                }
            }
        },
        wire::Action::WebhookOut { url, headers } => rsx! {
            p { class: "muted",
                "url: ", code { "{url}" },
                if !headers.is_empty() {
                    " · " {format!("{} header(s)", headers.len())}
                }
            }
        },
        wire::Action::LogLine { message } => rsx! {
            p { class: "muted", "message: ", code { "{message}" } }
        },
    }
}

/// Move DOM focus to an element by id — used by the tab keyboard handler
/// (PURA-248 M6) to keep focus on the active tab after arrow-key
/// navigation. Mirrors the helper in `components/dropdown.rs`.
#[cfg(target_arch = "wasm32")]
fn focus_element(id: &str) {
    use wasm_bindgen::JsCast;
    if let Some(elem) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Some(html) = elem.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}
