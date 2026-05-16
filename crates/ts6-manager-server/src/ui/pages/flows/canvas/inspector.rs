//! Inspector pane — the selected node's kind-specific config form
//! (`ui-brief.md` §4.3).
//!
//! The form mutates the editor's single `Signal<FlowGraph>` in place — the
//! canvas re-renders the moment a label or a case changes, because node
//! cards and ports are derived from that one signal.
//!
//! Footgun copy (`ui-brief.md` §6) is surfaced inline: the `delay`
//! restart warning, the `branch` first-match rule, the `parallel` fan-out
//! cap. The `err`-port footgun (#3) lives on the port itself, in
//! [`super::editor`].
//!
//! Kept deliberately bounded for this first cut: free-text and selector
//! fields are live; the `action` argument map and numeric ids commit on
//! blur (`onchange`) and discard invalid input. Reusing the richer v1.1
//! action-card field components (`ui-brief.md` §4.3) and the `transform`
//! object-construction grid are tracked follow-ups, noted in-pane.

use dioxus::prelude::*;
use serde_json::{Map, Value};
use ts6_manager_shared::flows::v2::{BranchCase, FlowGraph, NodeId, NodeKind, TransformOutput};
use ts6_manager_shared::flows::{Action, FloodScope, FloodSource, FlowId, Trigger};

use super::model;

/// The inspector pane. `read_only` mirrors the Definition-tab render and
/// the run-overlay's read-only mode (`ui-brief.md` §5) — disables editing
/// without changing the layout.
#[component]
pub fn Inspector(
    graph: Signal<FlowGraph>,
    selected: Signal<Option<NodeId>>,
    read_only: bool,
) -> Element {
    let snapshot = graph();
    let sel = selected();

    let node = sel
        .as_ref()
        .and_then(|id| snapshot.nodes.iter().find(|n| &n.id == id).cloned());

    let Some(node) = node else {
        return rsx! {
            aside { class: "fc-inspector", "aria-label": "node inspector",
                p { class: "fc-pane-title", "Inspector" }
                p { class: "fc-muted",
                    if sel.is_some() {
                        "The selected node was removed."
                    } else {
                        "Select a node to configure it."
                    }
                }
            }
        };
    };

    let node_id = node.id.clone();
    let kind = model::palette_kind(&node.kind);
    let label_value = node.label.clone().unwrap_or_default();

    rsx! {
        aside { class: "fc-inspector", "aria-label": "node inspector",
            p { class: "fc-pane-title", "Inspector" }
            h3 {
                span { class: "fc-glyph", "aria-hidden": "true", "{kind.glyph()} " }
                "{kind.label()}"
            }
            p { class: "fc-hint", "Node id: " code { "{node_id.0}" } }

            label { class: "fc-field",
                span { "Label" }
                input {
                    r#type: "text",
                    value: "{label_value}",
                    placeholder: "{node_id.0}",
                    disabled: read_only,
                    oninput: {
                        let node_id = node_id.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            if let Some(n) =
                                graph.write().nodes.iter_mut().find(|n| n.id == node_id)
                            {
                                n.label = if v.trim().is_empty() { None } else { Some(v) };
                            }
                        }
                    },
                }
            }

            KindForm { graph, node_id: node_id.clone(), kind: node.kind.clone(), read_only }
        }
    }
}

/// Dispatch to the per-kind form body.
#[component]
fn KindForm(graph: Signal<FlowGraph>, node_id: NodeId, kind: NodeKind, read_only: bool) -> Element {
    match kind {
        NodeKind::Trigger { config } => {
            rsx! { TriggerForm { graph, node_id, config, read_only } }
        }
        NodeKind::Action { config } => {
            rsx! { ActionForm { graph, node_id, config, read_only } }
        }
        NodeKind::Branch { cases } => {
            rsx! { BranchForm { graph, node_id, cases, read_only } }
        }
        NodeKind::Parallel {
            collection,
            sub_flow_id,
            max_concurrency,
        } => rsx! {
            ParallelForm { graph, node_id, collection, sub_flow_id, max_concurrency, read_only }
        },
        NodeKind::Delay { r#for } => {
            rsx! { DelayForm { graph, node_id, value: r#for, read_only } }
        }
        NodeKind::Transform { output } => {
            rsx! { TransformForm { graph, node_id, output, read_only } }
        }
        NodeKind::Subflow { sub_flow_id } => {
            rsx! { SubflowForm { graph, node_id, sub_flow_id, read_only } }
        }
    }
}

/// Write a new [`NodeKind`] onto the node `node_id` of `graph`.
fn set_kind(mut graph: Signal<FlowGraph>, node_id: &NodeId, kind: NodeKind) {
    if let Some(n) = graph.write().nodes.iter_mut().find(|n| &n.id == node_id) {
        n.kind = kind;
    }
}

/// Replace a branch node's case list, then drop edges that dangle off a
/// removed case's output port (`model::prune_dangling_edges`).
fn write_branch_cases(mut graph: Signal<FlowGraph>, node_id: &NodeId, cases: Vec<BranchCase>) {
    set_kind(graph, node_id, NodeKind::Branch { cases });
    model::prune_dangling_edges(&mut graph.write());
}

// --- Trigger -------------------------------------------------------------

#[component]
fn TriggerForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    config: Trigger,
    read_only: bool,
) -> Element {
    let current = match &config {
        Trigger::ManualFire => "manualFire",
        Trigger::Cron { .. } => "cron",
        Trigger::Ts6ClientJoined { .. } => "ts6ClientJoined",
        Trigger::Ts6ChatMessage { .. } => "ts6ChatMessage",
        Trigger::Ts6Flood { .. } => "ts6Flood",
    };
    rsx! {
        label { class: "fc-field",
            span { "Trigger event" }
            select {
                disabled: read_only,
                value: "{current}",
                onchange: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| {
                        let next = match e.value().as_str() {
                            "cron" => Trigger::Cron { expression: "0 * * * *".into() },
                            "ts6ClientJoined" => Trigger::Ts6ClientJoined { channel_id: None },
                            _ => Trigger::ManualFire,
                        };
                        set_kind(graph, &node_id, NodeKind::Trigger { config: next });
                    }
                },
                option { value: "manualFire", "Manual fire only" }
                option { value: "cron", "Cron schedule" }
                option { value: "ts6ClientJoined", "TS6 client joined" }
            }
        }
        match config {
            Trigger::ManualFire => rsx! {
                p { class: "fc-hint", "Runs only via the Fire button or POST /fire." }
            },
            Trigger::Cron { expression } => rsx! {
                label { class: "fc-field",
                    span { "Cron expression" }
                    input {
                        r#type: "text",
                        value: "{expression}",
                        disabled: read_only,
                        oninput: {
                            let node_id = node_id.clone();
                            move |e: FormEvent| set_kind(
                                graph, &node_id,
                                NodeKind::Trigger { config: Trigger::Cron { expression: e.value() } },
                            )
                        },
                    }
                }
                p { class: "fc-hint", "Validated server-side against the engine's cron dialect." }
            },
            Trigger::Ts6ClientJoined { channel_id } => rsx! {
                label { class: "fc-field",
                    span { "Channel id (blank = any channel)" }
                    input {
                        r#type: "number",
                        value: channel_id.map(|c| c.to_string()).unwrap_or_default(),
                        disabled: read_only,
                        onchange: {
                            let node_id = node_id.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                let cid = if v.trim().is_empty() { None } else { v.trim().parse().ok() };
                                set_kind(
                                    graph, &node_id,
                                    NodeKind::Trigger {
                                        config: Trigger::Ts6ClientJoined { channel_id: cid },
                                    },
                                );
                            }
                        },
                    }
                }
            },
            Trigger::Ts6ChatMessage { .. } => rsx! {
                p { class: "fc-hint",
                    "Fires on every TS6 chat message. Run context exposes trigger.message, trigger.clientNickname, trigger.channelId."
                }
            },
            Trigger::Ts6Flood { source, threshold, window_secs, scope } => {
                let src_label = match source {
                    FloodSource::ClientJoined => "clientJoined",
                    FloodSource::ChatMessage => "chatMessage",
                    FloodSource::ClientMoved => "clientMoved",
                };
                let scope_label = match scope {
                    FloodScope::Subject => "subject",
                    FloodScope::Ip => "ip",
                    FloodScope::Global => "global",
                };
                rsx! {
                    p { class: "fc-hint",
                        "Fires when the windowed counter crosses the threshold. "
                        "Source: {src_label}, threshold: {threshold}, window: {window_secs}s, scope: {scope_label}."
                    }
                }
            },
        }
    }
}

// --- Action --------------------------------------------------------------

/// Render a `serde_json` argument map as a pretty JSON string for the
/// args textarea.
fn args_to_text(args: &Map<String, Value>) -> String {
    if args.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".to_string())
    }
}

#[component]
fn ActionForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    config: Action,
    read_only: bool,
) -> Element {
    let current = match &config {
        Action::Ts6Command { .. } => "ts6Command",
        Action::MusicBotCommand { .. } => "musicBotCommand",
        Action::WebhookOut { .. } => "webhookOut",
        Action::LogLine { .. } => "logLine",
    };
    rsx! {
        label { class: "fc-field",
            span { "Action kind" }
            select {
                disabled: read_only,
                value: "{current}",
                onchange: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| {
                        let next = match e.value().as_str() {
                            "ts6Command" => Action::Ts6Command {
                                command: String::new(), args: Map::new(),
                            },
                            "musicBotCommand" => Action::MusicBotCommand {
                                bot_id: 0, command: String::new(), args: Map::new(),
                            },
                            "webhookOut" => Action::WebhookOut {
                                url: String::new(), headers: Vec::new(),
                            },
                            _ => Action::LogLine { message: String::new() },
                        };
                        set_kind(graph, &node_id, NodeKind::Action { config: next });
                    }
                },
                option { value: "logLine", "Log line (smoke/debug)" }
                option { value: "ts6Command", "TS6 command" }
                option { value: "musicBotCommand", "Music-bot command" }
                option { value: "webhookOut", "Webhook (HTTP POST)" }
            }
        }
        match config {
            Action::LogLine { message } => rsx! {
                label { class: "fc-field",
                    span { "Message" }
                    input {
                        r#type: "text", value: "{message}", disabled: read_only,
                        oninput: {
                            let node_id = node_id.clone();
                            move |e: FormEvent| set_kind(
                                graph, &node_id,
                                NodeKind::Action { config: Action::LogLine { message: e.value() } },
                            )
                        },
                    }
                }
            },
            Action::WebhookOut { url, headers } => rsx! {
                label { class: "fc-field",
                    span { "URL" }
                    input {
                        r#type: "text", value: "{url}", disabled: read_only,
                        oninput: {
                            let node_id = node_id.clone();
                            let headers = headers.clone();
                            move |e: FormEvent| set_kind(
                                graph, &node_id,
                                NodeKind::Action {
                                    config: Action::WebhookOut {
                                        url: e.value(), headers: headers.clone(),
                                    },
                                },
                            )
                        },
                    }
                }
                p { class: "fc-hint", "Checked against the manager's SSRF allow-list at send time." }
            },
            Action::Ts6Command { command, args } => {
                let cmd_val = command.clone();
                let args_for_input = args.clone();
                let args_for_field = args.clone();
                let nid_input = node_id.clone();
                let nid_rebuild = node_id.clone();
                let nid_field = node_id.clone();
                rsx! {
                    label { class: "fc-field",
                        span { "TS6 command" }
                        input {
                            r#type: "text", value: "{cmd_val}", disabled: read_only,
                            placeholder: "sendtextmessage",
                            oninput: move |e: FormEvent| set_kind(
                                graph, &nid_input,
                                NodeKind::Action {
                                    config: Action::Ts6Command {
                                        command: e.value(), args: args_for_input.clone(),
                                    },
                                },
                            ),
                        }
                    }
                    ArgsField {
                        graph, node_id: nid_field, args: args_for_field,
                        read_only,
                        rebuild: EventHandler::new(
                            move |(cmd, m): (String, Map<String, Value>)| set_kind(
                                graph, &nid_rebuild,
                                NodeKind::Action {
                                    config: Action::Ts6Command { command: cmd, args: m },
                                },
                            ),
                        ),
                        command_for_rebuild: command,
                    }
                }
            }
            Action::MusicBotCommand { bot_id, command, args } => {
                let cmd_id_val = command.clone();
                let cmd_input_val = command.clone();
                let args_for_id = args.clone();
                let args_for_input = args.clone();
                let args_for_field = args.clone();
                let nid_id = node_id.clone();
                let nid_input = node_id.clone();
                let nid_rebuild = node_id.clone();
                let nid_field = node_id.clone();
                rsx! {
                    label { class: "fc-field",
                        span { "Music-bot id" }
                        input {
                            r#type: "number", value: "{bot_id}", disabled: read_only,
                            onchange: move |e: FormEvent| {
                                if let Ok(id) = e.value().trim().parse::<u64>() {
                                    set_kind(
                                        graph, &nid_id,
                                        NodeKind::Action {
                                            config: Action::MusicBotCommand {
                                                bot_id: id, command: cmd_id_val.clone(),
                                                args: args_for_id.clone(),
                                            },
                                        },
                                    );
                                }
                            },
                        }
                    }
                    label { class: "fc-field",
                        span { "Command" }
                        input {
                            r#type: "text", value: "{cmd_input_val}", disabled: read_only,
                            oninput: move |e: FormEvent| set_kind(
                                graph, &nid_input,
                                NodeKind::Action {
                                    config: Action::MusicBotCommand {
                                        bot_id, command: e.value(),
                                        args: args_for_input.clone(),
                                    },
                                },
                            ),
                        }
                    }
                    p { class: "fc-hint", "Argument editing for music-bot commands: use the args field below." }
                    ArgsField {
                        graph, node_id: nid_field, args: args_for_field,
                        read_only,
                        rebuild: EventHandler::new(
                            move |(cmd, m): (String, Map<String, Value>)| set_kind(
                                graph, &nid_rebuild,
                                NodeKind::Action {
                                    config: Action::MusicBotCommand {
                                        bot_id, command: cmd, args: m,
                                    },
                                },
                            ),
                        ),
                        command_for_rebuild: command,
                    }
                }
            }
        }
    }
}

/// The shared raw-JSON argument editor. Commits on blur; invalid JSON is
/// discarded with a hint (richer per-command arg grids are a follow-up,
/// `ui-brief.md` §4.3).
#[component]
fn ArgsField(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    args: Map<String, Value>,
    read_only: bool,
    rebuild: EventHandler<(String, Map<String, Value>)>,
    command_for_rebuild: String,
) -> Element {
    let _ = graph;
    let _ = node_id;
    rsx! {
        label { class: "fc-field",
            span { "Arguments (JSON object)" }
            textarea {
                disabled: read_only,
                onchange: move |e: FormEvent| {
                    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&e.value()) {
                        rebuild.call((command_for_rebuild.clone(), m));
                    }
                },
                "{args_to_text(&args)}"
            }
        }
        p { class: "fc-hint",
            "Committed on blur. Invalid JSON, or a non-object, is discarded. "
            "The engine validates argument schemas against its whitelist."
        }
    }
}

// --- Branch --------------------------------------------------------------

#[component]
fn BranchForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    cases: Vec<BranchCase>,
    read_only: bool,
) -> Element {
    rsx! {
        p { class: "fc-hint warn",
            "Only the first matching case runs. The others — and everything after them — are skipped."
        }
        for (idx, case) in cases.iter().cloned().enumerate() {
            div { key: "{idx}", class: "fc-case",
                div { class: "fc-case-head",
                    strong { "Case {idx + 1}" }
                    if !read_only {
                        button {
                            class: "fc-btn-sm fc-btn-danger", r#type: "button",
                            onclick: {
                                let node_id = node_id.clone();
                                let cases = cases.clone();
                                move |_| {
                                    let mut next = cases.clone();
                                    next.remove(idx);
                                    write_branch_cases(graph, &node_id, next);
                                }
                            },
                            "Remove"
                        }
                    }
                }
                label { class: "fc-field",
                    span { "Port label" }
                    input {
                        r#type: "text", value: "{case.label}", disabled: read_only,
                        oninput: {
                            let node_id = node_id.clone();
                            let cases = cases.clone();
                            move |e: FormEvent| {
                                let mut next = cases.clone();
                                next[idx].label = e.value();
                                write_branch_cases(graph, &node_id, next);
                            }
                        },
                    }
                }
                label { class: "fc-field",
                    span { "When (boolean expression)" }
                    input {
                        r#type: "text", value: "{case.when}", disabled: read_only,
                        placeholder: "trigger.channelId == 1",
                        oninput: {
                            let node_id = node_id.clone();
                            let cases = cases.clone();
                            move |e: FormEvent| {
                                let mut next = cases.clone();
                                next[idx].when = e.value();
                                write_branch_cases(graph, &node_id, next);
                            }
                        },
                    }
                }
            }
        }
        if !read_only {
            button {
                class: "fc-btn-sm", r#type: "button",
                onclick: {
                    let node_id = node_id.clone();
                    let cases = cases.clone();
                    move |_| {
                        let mut next = cases.clone();
                        next.push(BranchCase {
                            label: format!("case {}", next.len() + 1),
                            when: String::new(),
                        });
                        write_branch_cases(graph, &node_id, next);
                    }
                },
                "+ Add case"
            }
        }
        p { class: "fc-hint",
            "Each case is an output port; the implicit "
            code { "default" }
            " port runs when no case matches."
        }
    }
}

// --- Parallel ------------------------------------------------------------

#[component]
fn ParallelForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    collection: String,
    sub_flow_id: FlowId,
    max_concurrency: u8,
    read_only: bool,
) -> Element {
    rsx! {
        label { class: "fc-field",
            span { "Collection expression" }
            input {
                r#type: "text", value: "{collection}", disabled: read_only,
                placeholder: "trigger.newClients",
                oninput: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| set_kind(
                        graph, &node_id,
                        NodeKind::Parallel {
                            collection: e.value(), sub_flow_id, max_concurrency,
                        },
                    )
                },
            }
        }
        label { class: "fc-field",
            span { "Sub-flow id" }
            input {
                r#type: "number", value: "{sub_flow_id.0}", disabled: read_only,
                onchange: {
                    let node_id = node_id.clone();
                    let collection = collection.clone();
                    move |e: FormEvent| {
                        if let Ok(id) = e.value().trim().parse::<i64>() {
                            set_kind(
                                graph, &node_id,
                                NodeKind::Parallel {
                                    collection: collection.clone(),
                                    sub_flow_id: FlowId(id), max_concurrency,
                                },
                            );
                        }
                    }
                },
            }
        }
        label { class: "fc-field",
            span { "Max concurrency (1–16)" }
            input {
                r#type: "number", min: "1", max: "16",
                value: "{max_concurrency}", disabled: read_only,
                onchange: {
                    let node_id = node_id.clone();
                    let collection = collection.clone();
                    move |e: FormEvent| {
                        if let Ok(mc) = e.value().trim().parse::<u8>() {
                            set_kind(
                                graph, &node_id,
                                NodeKind::Parallel {
                                    collection: collection.clone(),
                                    sub_flow_id,
                                    max_concurrency: mc.clamp(1, 16),
                                },
                            );
                        }
                    }
                },
            }
        }
        p { class: "fc-hint warn",
            "At most 256 items; at most 16 run at once."
        }
    }
}

// --- Delay ---------------------------------------------------------------

#[component]
fn DelayForm(graph: Signal<FlowGraph>, node_id: NodeId, value: String, read_only: bool) -> Element {
    rsx! {
        label { class: "fc-field",
            span { "Wait for" }
            input {
                r#type: "text", value: "{value}", disabled: read_only,
                placeholder: "30s",
                oninput: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| set_kind(
                        graph, &node_id, NodeKind::Delay { r#for: e.value() },
                    )
                },
            }
        }
        p { class: "fc-hint warn",
            "If the manager restarts while this is waiting, the run is interrupted. "
            "Keep waits short — the maximum is 15 minutes."
        }
    }
}

// --- Transform -----------------------------------------------------------

#[component]
fn TransformForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    output: TransformOutput,
    read_only: bool,
) -> Element {
    let expr_text = match &output {
        TransformOutput::Expr(e) => e.clone(),
        // Object construction is shown read-only here; the field grid is a
        // tracked follow-up (`ui-brief.md` §4.3).
        TransformOutput::Object(map) => serde_json::to_string_pretty(map).unwrap_or_default(),
    };
    let is_object = matches!(output, TransformOutput::Object(_));
    rsx! {
        label { class: "fc-field",
            span { if is_object { "Output object (field → expression)" } else { "Output expression" } }
            textarea {
                disabled: read_only || is_object,
                oninput: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| {
                        if !is_object {
                            set_kind(
                                graph, &node_id,
                                NodeKind::Transform {
                                    output: TransformOutput::Expr(e.value()),
                                },
                            );
                        }
                    }
                },
                "{expr_text}"
            }
        }
        if is_object {
            p { class: "fc-hint",
                "This node uses object-construction output. The field grid editor "
                "is a follow-up; edit object transforms via the API for now."
            }
        } else {
            p { class: "fc-hint",
                "A single expression producing any JSON value. Side-effect free."
            }
        }
    }
}

// --- Sub-flow ------------------------------------------------------------

#[component]
fn SubflowForm(
    graph: Signal<FlowGraph>,
    node_id: NodeId,
    sub_flow_id: FlowId,
    read_only: bool,
) -> Element {
    rsx! {
        label { class: "fc-field",
            span { "Sub-flow id" }
            input {
                r#type: "number", value: "{sub_flow_id.0}", disabled: read_only,
                onchange: {
                    let node_id = node_id.clone();
                    move |e: FormEvent| {
                        if let Ok(id) = e.value().trim().parse::<i64>() {
                            set_kind(
                                graph, &node_id,
                                NodeKind::Subflow { sub_flow_id: FlowId(id) },
                            );
                        }
                    }
                },
            }
        }
        p { class: "fc-hint warn",
            "A flow cannot call itself, directly or indirectly — validation blocks a reference cycle."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_args_render_as_an_empty_object() {
        assert_eq!(args_to_text(&Map::new()), "{}");
    }

    #[test]
    fn populated_args_pretty_print() {
        let mut m = Map::new();
        m.insert("clid".into(), Value::from(7));
        let text = args_to_text(&m);
        assert!(text.contains("\"clid\""), "got: {text}");
    }

    #[test]
    fn palette_kind_round_trips_through_a_default_node_kind() {
        // Every palette kind's default config maps back to the same kind.
        for pk in model::PaletteKind::ALL {
            assert_eq!(model::palette_kind(&pk.default_node_kind()), pk);
        }
    }
}
