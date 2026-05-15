//! `/flows/new` and `/flows/{id}/edit` — create / edit form.
//!
//! `ui-brief.md` §3.2 lays out five sections: identification, target
//! server, trigger, actions, save. Edit shares the same shape — when the
//! flow is enabled the trigger + actions are read-only (matches API
//! constraint `definition_swap_locked`).

use std::collections::HashMap;

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;

use crate::client::api::ApiError;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::flows as fl;
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonType, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::pages::flows::shared::{
    CRON_PRESETS, MAX_ACTIONS, MAX_NAME_LEN, REMOVE_GLYPH, action_kind_icon, action_kind_label,
    cron_validation_message, format_error,
};
use crate::ui::routes::Route;

/// `/flows/new` — create form. The same component handles the create
/// branch; edit is a thin wrapper that loads the flow first.
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
        FlowForm {
            mode: FormMode::Create,
            initial: None,
            server_config_id,
            server_name,
        }
    }
}

/// `/flows/{id}/edit` — edit form. Loads the current flow first, then
/// reuses [`FlowForm`] with `mode = Edit`.
#[component]
pub fn FlowEditPage(flow_id: i64) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }
    let flow = wire::FlowId(flow_id);
    let gate = use_auth_gate();
    let mut detail: Signal<Option<wire::Flow>> = use_signal(|| None);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);

    let snapshot = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fl::get_flow(gate, flow).await }
        }
    });
    use_effect(move || match &*snapshot.read_unchecked() {
        Some(Ok(f)) => {
            detail.set(Some(f.clone()));
            error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    let snap = detail.read().clone();
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
            FlowForm {
                mode: FormMode::Edit { flow_id: f.id },
                server_config_id: f.server_config_id,
                server_name: format!("server #{}", f.server_config_id),
                initial: Some(f),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormMode {
    Create,
    Edit { flow_id: wire::FlowId },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TriggerKind {
    Cron,
    ManualFire,
    Ts6ClientJoined,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ActionKind {
    Ts6Command,
    MusicBotCommand,
    WebhookOut,
    LogLine,
}

impl ActionKind {
    fn default_action(&self) -> wire::Action {
        match self {
            ActionKind::Ts6Command => wire::Action::Ts6Command {
                command: String::new(),
                args: serde_json::Map::new(),
            },
            ActionKind::MusicBotCommand => wire::Action::MusicBotCommand {
                bot_id: 0,
                command: String::new(),
                args: serde_json::Map::new(),
            },
            ActionKind::WebhookOut => wire::Action::WebhookOut {
                url: String::new(),
                headers: Vec::new(),
            },
            ActionKind::LogLine => wire::Action::LogLine {
                message: String::new(),
            },
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct FlowFormProps {
    mode: FormMode,
    server_config_id: i64,
    server_name: String,
    initial: Option<wire::Flow>,
}

#[component]
fn FlowForm(props: FlowFormProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let nav = use_navigator();

    // Initial values — from the loaded flow on edit, defaults on create.
    let (init_name, init_desc, init_enabled, init_trigger, init_actions) =
        match props.initial.as_ref() {
            Some(f) => (
                f.name.clone(),
                f.description.clone().unwrap_or_default(),
                f.enabled,
                f.definition.trigger.clone(),
                f.definition.actions.clone(),
            ),
            None => (
                String::new(),
                String::new(),
                false,
                wire::Trigger::ManualFire,
                vec![wire::Action::LogLine {
                    message: "Hello from the flow engine.".into(),
                }],
            ),
        };

    let mut name = use_signal(|| init_name);
    let mut description = use_signal(|| init_desc);
    let mut enabled = use_signal(|| init_enabled);

    let (init_trigger_kind, init_cron, init_channel) = match &init_trigger {
        wire::Trigger::Cron { expression } => {
            (TriggerKind::Cron, expression.clone(), String::new())
        }
        wire::Trigger::ManualFire => (TriggerKind::ManualFire, String::new(), String::new()),
        wire::Trigger::Ts6ClientJoined { channel_id } => (
            TriggerKind::Ts6ClientJoined,
            String::new(),
            channel_id.map(|c| c.to_string()).unwrap_or_default(),
        ),
    };
    let mut trigger_kind = use_signal(|| init_trigger_kind);
    let mut cron_expression = use_signal(|| init_cron);
    let mut channel_input = use_signal(|| init_channel);

    let mut actions: Signal<Vec<wire::Action>> = use_signal(|| init_actions);
    let mut submitting: Signal<bool> = use_signal(|| false);
    let mut form_error: Signal<Option<String>> = use_signal(|| None::<String>);

    // Edit form lock state — definition is read-only while the flow is
    // enabled, matches API `definition_swap_locked`.
    let definition_locked = matches!(props.mode, FormMode::Edit { .. }) && *enabled.read();

    let server_config_id = props.server_config_id;
    let virtual_server_id = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let on_submit = {
        let mode = props.mode;
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *submitting.read() {
                return;
            }
            // Validate.
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
            // Build trigger.
            let trigger = match *trigger_kind.read() {
                TriggerKind::Cron => {
                    let expr = cron_expression.read().trim().to_string();
                    if expr.is_empty() {
                        form_error.set(Some("Cron expression is required.".into()));
                        return;
                    }
                    wire::Trigger::Cron { expression: expr }
                }
                TriggerKind::ManualFire => wire::Trigger::ManualFire,
                TriggerKind::Ts6ClientJoined => {
                    let chan = channel_input.read().trim().to_string();
                    let channel_id = if chan.is_empty() {
                        None
                    } else {
                        match chan.parse::<i64>() {
                            Ok(c) => Some(c),
                            Err(_) => {
                                form_error
                                    .set(Some("Channel id must be a positive integer.".into()));
                                return;
                            }
                        }
                    };
                    wire::Trigger::Ts6ClientJoined { channel_id }
                }
            };
            // Build action list.
            let acts: Vec<wire::Action> = actions.read().clone();
            if acts.is_empty() {
                form_error.set(Some("At least one action is required.".into()));
                return;
            }
            if acts.len() > MAX_ACTIONS {
                form_error.set(Some(format!("At most {MAX_ACTIONS} actions per flow.")));
                return;
            }

            let definition = wire::FlowDefinition {
                trigger,
                actions: acts,
            };
            let desc_value = description.read().trim().to_string();
            let description_opt = if desc_value.is_empty() {
                None
            } else {
                Some(desc_value)
            };
            let enabled_value = *enabled.read();
            submitting.set(true);
            form_error.set(None);

            let gate = gate.clone();
            spawn(async move {
                let result = match mode {
                    FormMode::Create => {
                        let body = wire::CreateFlowRequest {
                            name: trimmed_name.clone(),
                            description: description_opt.clone(),
                            server_config_id,
                            virtual_server_id,
                            enabled: enabled_value,
                            definition: definition.clone(),
                        };
                        fl::create_flow(gate, &body).await
                    }
                    FormMode::Edit { flow_id } => {
                        let body = wire::UpdateFlowRequest {
                            name: Some(trimmed_name.clone()),
                            description: Some(description_opt.clone()),
                            virtual_server_id: Some(virtual_server_id),
                            enabled: Some(enabled_value),
                            definition: if definition_locked {
                                None
                            } else {
                                Some(definition.clone())
                            },
                        };
                        fl::update_flow(gate, flow_id, &body).await
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
        }
    };

    rsx! {
        form { class: "stack-lg",
            onsubmit: on_submit,
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
                p { class: "field-hint",
                    "Pick a different server from the header selector before creating the flow if this isn't right."
                }
                // PURA-248 L8 — the raw `serverConfigId` / `virtualServerId`
                // are power-user detail. Progressive disclosure keeps the
                // section's primary line ("which server") uncluttered.
                details { class: "disclosure",
                    summary { "Show advanced identifiers" }
                    p { class: "muted",
                        "Server config ", code { "{server_config_id}" },
                        " · virtual server ", code { "{virtual_server_id}" }, "."
                    }
                }
            }

            // ── Trigger ─────────────────────────────────────────
            section { class: "card stack-md",
                h2 { "Trigger" }
                if definition_locked {
                    Banner { variant: BannerVariant::Warning,
                        "This flow is enabled, so its trigger is locked. Uncheck "
                        strong { "Enable on save" }
                        " near the bottom of this form and save to unlock it."
                    }
                }
                TriggerCards {
                    selected: *trigger_kind.read(),
                    cron_expression: cron_expression.read().clone(),
                    channel_input: channel_input.read().clone(),
                    locked: definition_locked,
                    on_select: EventHandler::new(move |kind: TriggerKind| trigger_kind.set(kind)),
                    on_cron_change: EventHandler::new(move |v: String| cron_expression.set(v)),
                    on_channel_change: EventHandler::new(move |v: String| channel_input.set(v)),
                }
            }

            // ── Actions ─────────────────────────────────────────
            section { class: "card stack-md",
                h2 { "Actions" }
                if definition_locked {
                    Banner { variant: BannerVariant::Warning,
                        "This flow is enabled, so its actions are locked. Uncheck "
                        strong { "Enable on save" }
                        " near the bottom of this form and save to unlock them."
                    }
                }
                ActionsList {
                    actions: actions.read().clone(),
                    locked: definition_locked,
                    on_change: EventHandler::new(move |list: Vec<wire::Action>| actions.set(list)),
                }
            }

            // ── Save row ────────────────────────────────────────
            if let Some(msg) = form_error.read().as_ref() {
                Banner { variant: BannerVariant::Danger, title: "Save failed".to_string(),
                    "{msg}"
                }
            }
            // PURA-248 L4 — the enable toggle is a save-time *decision*, not
            // a button. It sits on its own line above the Cancel/submit row
            // so it is not mistaken for a third action button.
            label { class: "field-inline",
                input {
                    r#type: "checkbox",
                    checked: *enabled.read(),
                    oninput: move |e| enabled.set(e.value() == "true"),
                }
                " Enable on save"
                span { class: "field-hint",
                    "An enabled flow starts responding to its trigger immediately. Its trigger and actions are then locked until you disable it again."
                }
            }
            div { class: "actions",
                Link {
                    to: Route::FlowsListPage {},
                    class: "btn btn-ghost",
                    "Cancel"
                }
                Button {
                    variant: ButtonVariant::Primary,
                    kind: ButtonType::Submit,
                    loading: *submitting.read(),
                    disabled: server_config_id == 0,
                    match props.mode {
                        FormMode::Create => "Create flow",
                        FormMode::Edit { .. } => "Save changes",
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct TriggerCardsProps {
    selected: TriggerKind,
    cron_expression: String,
    channel_input: String,
    locked: bool,
    on_select: EventHandler<TriggerKind>,
    on_cron_change: EventHandler<String>,
    on_channel_change: EventHandler<String>,
}

#[component]
fn TriggerCards(props: TriggerCardsProps) -> Element {
    let sel = props.selected;
    let on_select = props.on_select;
    let on_cron_change = props.on_cron_change;
    let on_channel_change = props.on_channel_change;
    let cron_expression = props.cron_expression.clone();
    let channel_input = props.channel_input.clone();
    let locked = props.locked;
    rsx! {
        div { class: "stack-sm", role: "radiogroup",
            "aria-label": "Trigger kind",
            {
                let cron_active = sel == TriggerKind::Cron;
                let cron_class = if cron_active { "card is-selected" } else { "card" };
                rsx! {
                    label { class: "{cron_class}",
                        input {
                            r#type: "radio",
                            name: "trigger-kind",
                            checked: cron_active,
                            disabled: locked,
                            oninput: move |_| on_select.call(TriggerKind::Cron),
                        }
                        strong { " On a schedule" }
                        p { class: "muted", "Run on a cron expression. Missed ticks during downtime are not replayed." }
                        if cron_active {
                            div { class: "stack-sm",
                                {
                                    // PURA-248 M7 — live, non-authoritative
                                    // field-count check below the input. The
                                    // server still owns the real cron parse.
                                    let cron_warning = cron_validation_message(&cron_expression);
                                    let cron_invalid = cron_warning.is_some();
                                    rsx! {
                                        label { class: "field",
                                            span { class: "field-label", "Cron expression" }
                                            input {
                                                class: "input",
                                                value: "{cron_expression}",
                                                placeholder: "0 */5 * * * *",
                                                disabled: locked,
                                                "aria-invalid": if cron_invalid { "true" } else { "false" },
                                                oninput: move |e| on_cron_change.call(e.value()),
                                            }
                                            if let Some(msg) = cron_warning.as_ref() {
                                                span {
                                                    class: "field-error",
                                                    role: "status",
                                                    "aria-live": "polite",
                                                    "{msg}"
                                                }
                                            }
                                        }
                                    }
                                }
                                div { class: "chip-row",
                                    for (label, expr) in CRON_PRESETS.iter() {
                                        button {
                                            r#type: "button",
                                            class: "chip",
                                            disabled: locked,
                                            onclick: {
                                                let expr = expr.to_string();
                                                move |_| on_cron_change.call(expr.clone())
                                            },
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            {
                let manual_active = sel == TriggerKind::ManualFire;
                let manual_class = if manual_active { "card is-selected" } else { "card" };
                rsx! {
                    label { class: "{manual_class}",
                        input {
                            r#type: "radio",
                            name: "trigger-kind",
                            checked: manual_active,
                            disabled: locked,
                            oninput: move |_| on_select.call(TriggerKind::ManualFire),
                        }
                        strong { " Manually only" }
                        p { class: "muted",
                            "Useful for testing or for actions you only want to run on demand."
                        }
                    }
                }
            }
            {
                let joined_active = sel == TriggerKind::Ts6ClientJoined;
                let joined_class = if joined_active { "card is-selected" } else { "card" };
                rsx! {
                    label { class: "{joined_class}",
                        input {
                            r#type: "radio",
                            name: "trigger-kind",
                            checked: joined_active,
                            disabled: locked,
                            oninput: move |_| on_select.call(TriggerKind::Ts6ClientJoined),
                        }
                        strong { " When a client joins" }
                        p { class: "muted",
                            "Leave channel empty to match any channel. Self-triggers are dropped, not queued."
                        }
                        if joined_active {
                            label { class: "field",
                                span { class: "field-label", "Channel id (optional)" }
                                input {
                                    class: "input",
                                    value: "{channel_input}",
                                    placeholder: "e.g. 5",
                                    disabled: locked,
                                    oninput: move |e| on_channel_change.call(e.value()),
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
struct ActionsListProps {
    actions: Vec<wire::Action>,
    locked: bool,
    on_change: EventHandler<Vec<wire::Action>>,
}

#[component]
fn ActionsList(props: ActionsListProps) -> Element {
    let acts = props.actions.clone();
    let locked = props.locked;
    let on_change = props.on_change;
    let count = acts.len();

    // Convenience closure: rebuild the list with one cell replaced.
    let replace_at = move |idx: usize, action: wire::Action, list: &Vec<wire::Action>| {
        let mut next = list.clone();
        if idx < next.len() {
            next[idx] = action;
        }
        next
    };

    rsx! {
        ol { class: "stack-md",
            for (idx, action) in acts.iter().enumerate() {
                {
                    let action = action.clone();
                    let acts_clone = acts.clone();
                    rsx! {
                        li { key: "{idx}", class: "card stack-sm",
                            div { class: "actions",
                                strong {
                                    span { class: "flow-icon", aria_hidden: "true",
                                        "{action_kind_icon(&action)}"
                                    }
                                    " Action {idx + 1} — {action_kind_label(&action)}"
                                }
                                if !locked {
                                    Button {
                                        variant: ButtonVariant::Ghost,
                                        onclick: {
                                            let acts_clone = acts_clone.clone();
                                            move |_| {
                                                let mut next = acts_clone.clone();
                                                next.remove(idx);
                                                on_change.call(next);
                                            }
                                        },
                                        "Remove"
                                    }
                                }
                            }
                            ActionEditor {
                                index: idx,
                                action: action.clone(),
                                locked,
                                on_change: EventHandler::new({
                                    let acts_clone = acts_clone.clone();
                                    move |new_action: wire::Action| {
                                        on_change.call(replace_at(idx, new_action, &acts_clone));
                                    }
                                }),
                            }
                        }
                    }
                }
            }
        }
        if !locked && count < MAX_ACTIONS {
            div { class: "actions",
                Button {
                    variant: ButtonVariant::Secondary,
                    onclick: {
                        let acts_clone = acts.clone();
                        move |_| {
                            let mut next = acts_clone.clone();
                            next.push(ActionKind::LogLine.default_action());
                            on_change.call(next);
                        }
                    },
                    "+ Add action"
                }
                span { class: "muted",
                    "Up to {MAX_ACTIONS} actions per flow ({count} used)."
                }
            }
        } else if count >= MAX_ACTIONS {
            p { class: "muted", "Hit the {MAX_ACTIONS}-action cap." }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ActionEditorProps {
    index: usize,
    action: wire::Action,
    locked: bool,
    on_change: EventHandler<wire::Action>,
}

#[component]
fn ActionEditor(props: ActionEditorProps) -> Element {
    let locked = props.locked;
    let action = props.action.clone();
    let on_change = props.on_change;

    let current_kind = match &action {
        wire::Action::Ts6Command { .. } => ActionKind::Ts6Command,
        wire::Action::MusicBotCommand { .. } => ActionKind::MusicBotCommand,
        wire::Action::WebhookOut { .. } => ActionKind::WebhookOut,
        wire::Action::LogLine { .. } => ActionKind::LogLine,
    };

    let kind_select = {
        let action = action.clone();
        rsx! {
            label { class: "field",
                span { class: "field-label", "Kind" }
                select {
                    class: "input",
                    disabled: locked,
                    value: format!("{:?}", current_kind),
                    oninput: move |e| {
                        let new_kind = match e.value().as_str() {
                            "Ts6Command" => ActionKind::Ts6Command,
                            "MusicBotCommand" => ActionKind::MusicBotCommand,
                            "WebhookOut" => ActionKind::WebhookOut,
                            _ => ActionKind::LogLine,
                        };
                        if new_kind != current_kind {
                            // Reset to defaults — switching kinds means we
                            // lose the kind-specific fields. Preserving
                            // them across kinds would require a complex
                            // form-state union; not worth the v1.1
                            // complexity (brief §3.2 doesn't ask for it).
                            on_change.call(new_kind.default_action());
                        } else {
                            on_change.call(action.clone());
                        }
                    },
                    option { value: "Ts6Command", selected: matches!(current_kind, ActionKind::Ts6Command), "TS6 command" }
                    option { value: "MusicBotCommand", selected: matches!(current_kind, ActionKind::MusicBotCommand), "Music-bot command" }
                    option { value: "WebhookOut", selected: matches!(current_kind, ActionKind::WebhookOut), "Webhook" }
                    option { value: "LogLine", selected: matches!(current_kind, ActionKind::LogLine), "Log line" }
                }
                // PURA-248 M3 — switching kind resets the action to its
                // defaults (the kind-specific fields cannot carry over).
                // Signpost it so the reset is not a silent surprise.
                span { class: "field-hint",
                    "Changing the kind clears this action's other fields."
                }
            }
        }
    };

    let body = match action.clone() {
        wire::Action::LogLine { message } => rsx! {
            label { class: "field",
                span { class: "field-label", "Message" }
                textarea {
                    class: "input",
                    rows: "2",
                    disabled: locked,
                    value: "{message}",
                    placeholder: "Logged at INFO level when this action fires.",
                    oninput: move |e| {
                        on_change.call(wire::Action::LogLine { message: e.value() });
                    },
                }
            }
        },
        wire::Action::Ts6Command { command, args } => rsx! {
            label { class: "field",
                span { class: "field-label", "Command" }
                input {
                    class: "input",
                    disabled: locked,
                    value: "{command}",
                    placeholder: "sendtextmessage",
                    oninput: {
                        let args = args.clone();
                        move |e| {
                            on_change.call(wire::Action::Ts6Command {
                                command: e.value(),
                                args: args.clone(),
                            });
                        }
                    },
                }
                span { class: "field-hint",
                    "Whitelisted server-side. Use the ", code { "${{trigger.…}}" },
                    " placeholders inside args to splat trigger data."
                }
            }
            KeyValueEditor {
                label: String::from("Args"),
                values: args.clone(),
                locked,
                on_change: EventHandler::new({
                    let command = command.clone();
                    move |new_args: serde_json::Map<String, serde_json::Value>| {
                        on_change.call(wire::Action::Ts6Command {
                            command: command.clone(),
                            args: new_args,
                        });
                    }
                }),
            }
        },
        wire::Action::MusicBotCommand {
            bot_id,
            command,
            args,
        } => rsx! {
            label { class: "field",
                span { class: "field-label", "Bot id" }
                input {
                    class: "input",
                    r#type: "number",
                    min: "0",
                    disabled: locked,
                    value: "{bot_id}",
                    oninput: {
                        let command = command.clone();
                        let args = args.clone();
                        move |e| {
                            let parsed = e.value().parse::<u64>().unwrap_or(0);
                            on_change.call(wire::Action::MusicBotCommand {
                                bot_id: parsed,
                                command: command.clone(),
                                args: args.clone(),
                            });
                        }
                    },
                }
            }
            label { class: "field",
                span { class: "field-label", "Command" }
                input {
                    class: "input",
                    disabled: locked,
                    value: "{command}",
                    placeholder: "play",
                    oninput: {
                        let args = args.clone();
                        move |e| {
                            on_change.call(wire::Action::MusicBotCommand {
                                bot_id,
                                command: e.value(),
                                args: args.clone(),
                            });
                        }
                    },
                }
            }
            KeyValueEditor {
                label: String::from("Args"),
                values: args.clone(),
                locked,
                on_change: EventHandler::new({
                    let command = command.clone();
                    move |new_args: serde_json::Map<String, serde_json::Value>| {
                        on_change.call(wire::Action::MusicBotCommand {
                            bot_id,
                            command: command.clone(),
                            args: new_args,
                        });
                    }
                }),
            }
        },
        wire::Action::WebhookOut { url, headers } => rsx! {
            label { class: "field",
                span { class: "field-label", "URL" }
                input {
                    class: "input",
                    r#type: "url",
                    disabled: locked,
                    value: "{url}",
                    placeholder: "https://example.com/hook",
                    oninput: {
                        let headers = headers.clone();
                        move |e| {
                            on_change.call(wire::Action::WebhookOut {
                                url: e.value(),
                                headers: headers.clone(),
                            });
                        }
                    },
                }
                span { class: "field-hint",
                    "URL is sent as-is; ensure it is on the manager's allow-list."
                }
            }
            HeaderListEditor {
                headers: headers.clone(),
                locked,
                on_change: EventHandler::new({
                    let url = url.clone();
                    move |new_headers: Vec<(String, String)>| {
                        on_change.call(wire::Action::WebhookOut {
                            url: url.clone(),
                            headers: new_headers,
                        });
                    }
                }),
            }
        },
    };

    rsx! {
        div { class: "stack-sm",
            {kind_select}
            {body}
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct KeyValueEditorProps {
    label: String,
    values: serde_json::Map<String, serde_json::Value>,
    locked: bool,
    on_change: EventHandler<serde_json::Map<String, serde_json::Value>>,
}

/// Operator-facing notice (PURA-248 L5) for rows that `project` dropped
/// when coercing the editable rows down to a JSON map — a value with no
/// key, or a key that collided with an earlier row. A fully blank row is
/// not counted: that is just an unfilled new row, not a drop.
fn kv_drop_notice(keyless: usize, duplicate: usize) -> Option<String> {
    match (keyless, duplicate) {
        (0, 0) => None,
        (k, 0) => Some(format!(
            "{k} row(s) have a value but no key — not saved. Add a key or remove the row."
        )),
        (0, d) => Some(format!(
            "{d} duplicate key(s) dropped — only the first row for each key is kept."
        )),
        (k, d) => Some(format!(
            "{d} duplicate key(s) dropped, {k} keyless row(s) not saved."
        )),
    }
}

#[component]
fn KeyValueEditor(props: KeyValueEditorProps) -> Element {
    let label = props.label.clone();
    let locked = props.locked;
    let on_change = props.on_change;

    // Local working state — the editable (key, value) string rows. The
    // parent stores args as a `serde_json::Map`, which cannot represent a
    // blank-keyed or duplicate row; round-tripping every keystroke through
    // the map would drop the row the operator is still typing (PURA-248
    // L5 — and previously made "+ Add arg" a no-op). So the editable rows
    // live here, and we only *project* the coerced map outward. State is
    // seeded once: the parent only ever feeds this editor its own
    // projected map, so there is no external source of truth to re-sync.
    let mut rows = use_signal(|| {
        props
            .values
            .iter()
            .map(|(k, v)| (k.clone(), value_to_string(v)))
            .collect::<Vec<(String, String)>>()
    });
    // Live notice when projecting silently dropped rows (L5).
    let mut drop_notice = use_signal(|| None::<String>);

    // Coerce the current rows to a JSON map, record what was dropped, and
    // notify the parent. Captures only `Copy` handles, so it is itself a
    // `Copy` closure and can be reused across the per-row event handlers.
    let mut project = move || {
        let current = rows.read().clone();
        let mut next = serde_json::Map::new();
        let mut seen: HashMap<String, ()> = HashMap::new();
        let mut keyless = 0usize;
        let mut duplicate = 0usize;
        for (k, v) in current.iter() {
            let key = k.trim();
            if key.is_empty() {
                if !v.trim().is_empty() {
                    keyless += 1;
                }
                continue;
            }
            if seen.contains_key(key) {
                duplicate += 1;
                continue;
            }
            seen.insert(key.to_string(), ());
            // Preserve numeric / bool / null; fall back to string.
            next.insert(key.to_string(), string_to_value(v));
        }
        drop_notice.set(kv_drop_notice(keyless, duplicate));
        on_change.call(next);
    };

    let entries = rows.read().clone();

    rsx! {
        fieldset { class: "stack-sm",
            legend { class: "field-label", "{label}" }
            // PURA-248 L6 — the JSON coercion is a footgun without copy.
            p { class: "field-hint",
                "Values are JSON-coerced — ",
                code { "5" },
                " saves as a number and ",
                code { "true" },
                " as a boolean. Wrap a value in quotes to force text."
            }
            if entries.is_empty() {
                p { class: "muted", "No args. Add a key/value pair below." }
            } else {
                ul { class: "stack-sm",
                    for (idx, (k, v)) in entries.iter().enumerate() {
                        {
                            let k_val = k.clone();
                            let v_val = v.clone();
                            rsx! {
                                li { key: "{idx}", class: "kv-row",
                                    input {
                                        class: "input",
                                        disabled: locked,
                                        // PURA-248 M1 — placeholder is not a label.
                                        "aria-label": "Arg name",
                                        value: "{k_val}",
                                        placeholder: "key",
                                        oninput: move |e| {
                                            rows.with_mut(|r| r[idx].0 = e.value());
                                            project();
                                        },
                                    }
                                    input {
                                        class: "input",
                                        disabled: locked,
                                        "aria-label": "Arg value",
                                        value: "{v_val}",
                                        placeholder: "value",
                                        oninput: move |e| {
                                            rows.with_mut(|r| r[idx].1 = e.value());
                                            project();
                                        },
                                    }
                                    if !locked {
                                        button {
                                            r#type: "button",
                                            class: "btn btn-ghost btn-sm",
                                            "aria-label": "Remove arg",
                                            onclick: move |_| {
                                                rows.with_mut(|r| { r.remove(idx); });
                                                project();
                                            },
                                            span { class: "flow-icon", aria_hidden: "true", {REMOVE_GLYPH} }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(notice) = drop_notice.read().as_ref() {
                p {
                    class: "field-error",
                    role: "status",
                    "aria-live": "polite",
                    "{notice}"
                }
            }
            if !locked {
                Button {
                    variant: ButtonVariant::Ghost,
                    onclick: move |_| {
                        rows.with_mut(|r| r.push((String::new(), String::new())));
                        project();
                    },
                    "+ Add arg"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct HeaderListEditorProps {
    headers: Vec<(String, String)>,
    locked: bool,
    on_change: EventHandler<Vec<(String, String)>>,
}

#[component]
fn HeaderListEditor(props: HeaderListEditorProps) -> Element {
    let headers = props.headers.clone();
    let locked = props.locked;
    let on_change = props.on_change;
    rsx! {
        fieldset { class: "stack-sm",
            legend { class: "field-label", "Headers" }
            if headers.is_empty() {
                p { class: "muted", "No custom headers." }
            } else {
                ul { class: "stack-sm",
                    for (idx, (k, v)) in headers.iter().enumerate() {
                        {
                            let headers = headers.clone();
                            let k_val = k.clone();
                            let v_val = v.clone();
                            rsx! {
                                li { key: "{idx}", class: "kv-row",
                                    input {
                                        class: "input",
                                        disabled: locked,
                                        // PURA-248 M1 — placeholder is not a label.
                                        "aria-label": "Header name",
                                        value: "{k_val}",
                                        placeholder: "Header-Name",
                                        oninput: {
                                            let headers = headers.clone();
                                            move |e| {
                                                let mut next = headers.clone();
                                                next[idx].0 = e.value();
                                                on_change.call(next);
                                            }
                                        },
                                    }
                                    input {
                                        class: "input",
                                        disabled: locked,
                                        "aria-label": "Header value",
                                        value: "{v_val}",
                                        placeholder: "value",
                                        oninput: {
                                            let headers = headers.clone();
                                            move |e| {
                                                let mut next = headers.clone();
                                                next[idx].1 = e.value();
                                                on_change.call(next);
                                            }
                                        },
                                    }
                                    if !locked {
                                        button {
                                            r#type: "button",
                                            class: "btn btn-ghost btn-sm",
                                            "aria-label": "Remove header",
                                            onclick: {
                                                let headers = headers.clone();
                                                move |_| {
                                                    let mut next = headers.clone();
                                                    next.remove(idx);
                                                    on_change.call(next);
                                                }
                                            },
                                            span { class: "flow-icon", aria_hidden: "true", {REMOVE_GLYPH} }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !locked {
                Button {
                    variant: ButtonVariant::Ghost,
                    onclick: {
                        let headers = headers.clone();
                        move |_| {
                            let mut next = headers.clone();
                            next.push((String::new(), String::new()));
                            on_change.call(next);
                        }
                    },
                    "+ Add header"
                }
            }
        }
    }
}

/// Render a `serde_json::Value` as a string for the kv editor. Strings
/// are unwrapped (no extra quotes); other types serialise verbatim.
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Inverse of [`value_to_string`]. Parses an input as JSON first (so
/// numbers, bools, arrays round-trip); falls back to a plain string.
fn string_to_value(s: &str) -> serde_json::Value {
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

    #[test]
    fn action_kind_defaults_round_trip_through_match() {
        for kind in [
            ActionKind::LogLine,
            ActionKind::Ts6Command,
            ActionKind::WebhookOut,
            ActionKind::MusicBotCommand,
        ] {
            let action = kind.default_action();
            match (kind, action) {
                (ActionKind::LogLine, wire::Action::LogLine { .. }) => {}
                (ActionKind::Ts6Command, wire::Action::Ts6Command { .. }) => {}
                (ActionKind::WebhookOut, wire::Action::WebhookOut { .. }) => {}
                (ActionKind::MusicBotCommand, wire::Action::MusicBotCommand { .. }) => {}
                _ => panic!("mismatched action kind"),
            }
        }
    }
}
