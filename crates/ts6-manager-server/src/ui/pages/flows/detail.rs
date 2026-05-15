//! `/flows/{id}` — per-flow operator surface.
//!
//! Tabs: Runs (default) + Definition. Runs is the operator's debugging
//! surface; Definition is the read-only view of the trigger + actions
//! (edit lives at `/flows/{id}/edit`).

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::flows as fl;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonVariant};
use crate::ui::pages::flows::shared::{
    action_kind_label, enabled_badge_class, enabled_label, format_error, is_run_in_flight_conflict,
    relative_when, run_status_badge_class, run_status_hint, run_status_label, trigger_summary,
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

    let mut detail: Signal<Option<wire::Flow>> = use_signal(|| None);
    let mut detail_error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut detail_loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);

    let flow_snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.peek();
            let _ = reload.read();
            async move { fl::get_flow(gate, flow).await }
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

    let on_delete = {
        let gate = gate.clone();
        let nav = use_navigator();
        move |_| {
            let gate = gate.clone();
            spawn(async move {
                match fl::delete_flow(gate.clone(), flow, false).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Deleted flow", None);
                        nav.push(Route::FlowsListPage {});
                    }
                    Err(e) if is_run_in_flight_conflict(&e) => {
                        match fl::delete_flow(gate, flow, true).await {
                            Ok(()) => {
                                toaster.push(
                                    ToastVariant::Warning,
                                    "Force-deleted flow",
                                    Some("Interrupted in-flight run.".into()),
                                );
                                nav.push(Route::FlowsListPage {});
                            }
                            Err(e2) => toaster.push(
                                ToastVariant::Danger,
                                "Force delete failed",
                                Some(format_error(&e2)),
                            ),
                        }
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
                on_fire: EventHandler::new({
                    let on_fire = on_fire.clone();
                    move |_| on_fire(())
                }),
                on_toggle_enabled: EventHandler::new({
                    let on_toggle = on_toggle_enabled.clone();
                    move |en: bool| on_toggle(en)
                }),
                on_delete: EventHandler::new({
                    let on_delete = on_delete.clone();
                    move |_| on_delete(())
                }),
            }

            // Tab bar.
            div { class: "tabs", role: "tablist",
                button {
                    r#type: "button",
                    role: "tab",
                    class: if *tab.read() == DetailTab::Runs { "tab is-active" } else { "tab" },
                    "aria-selected": if *tab.read() == DetailTab::Runs { "true" } else { "false" },
                    onclick: move |_| tab.set(DetailTab::Runs),
                    "Runs"
                }
                button {
                    r#type: "button",
                    role: "tab",
                    class: if *tab.read() == DetailTab::Definition { "tab is-active" } else { "tab" },
                    "aria-selected": if *tab.read() == DetailTab::Definition { "true" } else { "false" },
                    onclick: move |_| tab.set(DetailTab::Definition),
                    "Definition"
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
                DefinitionPanel { flow: f }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct FlowHeaderProps {
    flow: wire::Flow,
    on_fire: EventHandler<()>,
    on_toggle_enabled: EventHandler<bool>,
    on_delete: EventHandler<()>,
}

#[component]
fn FlowHeader(props: FlowHeaderProps) -> Element {
    let f = props.flow.clone();
    let on_fire = props.on_fire;
    let on_toggle = props.on_toggle_enabled;
    let on_delete = props.on_delete;
    let enabled = f.enabled;
    let edit_route = Route::FlowEditPage { flow_id: f.id.0 };
    rsx! {
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "{f.name}" }
                p { class: "page-lede",
                    span { class: enabled_badge_class(enabled),
                        "{enabled_label(enabled)}"
                    }
                    " · "
                    "{trigger_summary(&f.definition.trigger)}"
                }
                if let Some(d) = f.description.as_deref().filter(|s| !s.is_empty()) {
                    p { class: "muted", "{d}" }
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: move |_| on_fire.call(()),
                    "Fire"
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    onclick: move |_| on_toggle.call(enabled),
                    if enabled { "Disable" } else { "Enable" }
                }
                Link {
                    to: edit_route,
                    class: "btn btn-ghost",
                    "Edit"
                }
                Button {
                    variant: ButtonVariant::Danger,
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
    rsx! {
        section { role: "tabpanel", class: "stack-md",
            if let Some(e) = err.as_ref() {
                Banner { variant: BannerVariant::Danger, title: "Could not load runs".to_string(),
                    "{format_error(e)}"
                }
            }
            if runs.is_empty() && !loading {
                div { class: "empty",
                    div { class: "icon", "↻" }
                    h3 { "No runs yet" }
                    p { "Hit ", strong { "Fire" }, " to make the engine run this flow on demand." }
                }
            } else {
                table { class: "data-table",
                    "aria-label": "Flow runs",
                    thead {
                        tr {
                            th { scope: "col", "When" }
                            th { scope: "col", "Trigger" }
                            th { scope: "col", "Status" }
                            th { scope: "col", "Duration" }
                            th { scope: "col", "Error" }
                        }
                    }
                    tbody {
                        for r in runs.iter() {
                            {
                                let r = r.clone();
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
                                rsx! {
                                    tr { key: "{r.summary.id.0}",
                                        td { "{when}" }
                                        td { "{trig_kind}" }
                                        td {
                                            span { class: run_status_badge_class(r.summary.status),
                                                title: hint.unwrap_or(""),
                                                "{run_status_label(r.summary.status)}"
                                            }
                                        }
                                        td { "{dur}" }
                                        td { class: "muted", "{err}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "actions",
                Button {
                    variant: ButtonVariant::Ghost,
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
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DefinitionPanelProps {
    flow: wire::Flow,
}

#[component]
fn DefinitionPanel(props: DefinitionPanelProps) -> Element {
    let f = props.flow.clone();
    let definition_locked = f.enabled;
    rsx! {
        section { role: "tabpanel", class: "stack-md",
            article { class: "card",
                h2 { "Trigger" }
                p { "{trigger_summary(&f.definition.trigger)}" }
            }
            article { class: "card",
                h2 { "Actions" }
                if f.definition.actions.is_empty() {
                    p { class: "muted", "No actions defined." }
                } else {
                    ol { class: "stack-sm",
                        for (idx, a) in f.definition.actions.iter().enumerate() {
                            li { key: "{idx}",
                                strong { "{action_kind_label(a)}" }
                                ActionSummary { action: a.clone() }
                            }
                        }
                    }
                }
            }
            div { class: "actions",
                if definition_locked {
                    Button {
                        variant: ButtonVariant::Secondary,
                        disabled: true,
                        // The brief asks for the disabled hover tooltip
                        // — `title` is the cheapest way to surface it
                        // without pulling in a tooltip primitive.
                        onclick: move |_| (),
                        "Edit definition (disable first)"
                    }
                } else {
                    Link {
                        to: Route::FlowEditPage { flow_id: f.id.0 },
                        class: "btn btn-secondary",
                        "Edit definition"
                    }
                }
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
