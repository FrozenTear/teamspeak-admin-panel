//! The three-pane visual flow editor — palette · canvas · inspector
//! (`ui-brief.md` §4).
//!
//! Built on the canvas-tech spike's Option-A model ([PURA-264](/PURA/issues/PURA-264)),
//! rebuilt against the v2 wire types: the editor's single source of truth
//! is one `Signal<FlowGraph>`. Drag-to-move rewrites `node.position`;
//! drag-to-connect pushes an [`Edge`]; the inspector edits node kinds.
//! Node cards are absolutely-positioned `div`s; edges are SVG `<path>`
//! beziers on one overlay — no JavaScript, no interop boundary.
//!
//! Pan/zoom is a single CSS `transform` on the `.fc-world` layer; because
//! port coordinates are *derived* (`geometry.rs`), the maths is unchanged
//! by the transform — drag deltas are simply divided by the zoom factor.
//!
//! Not yet wired (tracked on [PURA-267](/PURA/issues/PURA-267), needs the
//! v2 HTTP surface from [PURA-266](/PURA/issues/PURA-266)): debounced
//! `POST /api/flows/validate` inline linting, save, and the run-overlay
//! *poll*. The overlay *rendering* is wired — the `run_overlay` prop paints
//! per-node status (`canvas-visual-spec.md` §5) — it simply has no live data
//! source until PURA-266's `GET …/runs/{runId}` lands to feed it.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use ts6_manager_shared::flows::v2::{FlowGraph, NodeId, NodeKind, Position, ValidateGraphResponse};

use super::geometry::{self, KBD_STEP, NODE_W, ZOOM_MAX, ZOOM_MIN};
use super::inspector::Inspector;
use super::model::{self, PaletteKind, RunOverlayStatus};
use super::style::CANVAS_CSS;
use crate::client::dioxus::use_auth_gate;
use crate::client::flows as fl;

/// Debounce, ms, between the last structural edit and the inline
/// `POST /api/flows/validate` call (`ui-brief.md` §4.4). `use_resource`
/// cancels the still-sleeping future when the graph changes again, so a
/// burst of edits collapses to a single validation once activity settles.
const VALIDATE_DEBOUNCE_MS: u32 = 450;

/// An in-flight node drag — pointer origin + node origin, in their own
/// coordinate spaces; the delta is applied on every `pointermove`.
#[derive(Clone)]
struct DragState {
    node: NodeId,
    start_px: f64,
    start_py: f64,
    start_nx: f64,
    start_ny: f64,
}

/// An in-flight canvas pan — pointer origin + pan origin (screen space).
#[derive(Clone, Copy)]
struct PanDrag {
    start_px: f64,
    start_py: f64,
    start_panx: f64,
    start_pany: f64,
}

/// An in-flight port-to-port connection with a live preview edge.
#[derive(Clone)]
struct ConnectState {
    src_node: NodeId,
    src_port: String,
    is_err: bool,
    /// Source port centre, world coordinates — fixed for the drag.
    sx: f64,
    sy: f64,
    /// Pointer client coordinates at connect-start, for delta tracking.
    start_px: f64,
    start_py: f64,
    /// Live preview endpoint, world coordinates.
    ex: f64,
    ey: f64,
}

/// A one-line body summary for a node card.
fn node_summary(kind: &NodeKind) -> String {
    use ts6_manager_shared::flows::{Action, Trigger};
    match kind {
        NodeKind::Trigger { config } => match config {
            Trigger::ManualFire => "manual fire".into(),
            Trigger::Cron { expression } => format!("cron · {expression}"),
            Trigger::Ts6ClientJoined { channel_id } => match channel_id {
                Some(c) => format!("client joined · channel {c}"),
                None => "client joined · any channel".into(),
            },
            Trigger::Ts6ChatMessage { channel_id, .. } => match channel_id {
                Some(c) => format!("chat message · channel {c}"),
                None => "chat message · any channel".into(),
            },
            Trigger::Ts6Flood { source, threshold, window_secs, .. } => {
                let src = match source {
                    ts6_manager_shared::flows::FloodSource::ClientJoined => "joins",
                    ts6_manager_shared::flows::FloodSource::ChatMessage => "messages",
                    ts6_manager_shared::flows::FloodSource::ClientMoved => "moves",
                };
                format!("flood · {threshold} {src}/{window_secs}s")
            }
        },
        NodeKind::Action { config } => match config {
            Action::LogLine { .. } => "log line".into(),
            Action::WebhookOut { url, .. } => format!("webhook · {url}"),
            Action::Ts6Command { command, .. } => format!("ts6 · {command}"),
            Action::MusicBotCommand { command, .. } => format!("music bot · {command}"),
        },
        NodeKind::Branch { cases } => format!("{} case(s) + default", cases.len()),
        NodeKind::Parallel { collection, .. } => format!("fan out · {collection}"),
        NodeKind::Delay { r#for } => format!("wait {for}", for = r#for),
        NodeKind::Transform { .. } => "reshape data".into(),
        NodeKind::Subflow { sub_flow_id } => format!("run flow #{}", sub_flow_id.0),
    }
}

/// Precomputed render data for one node card.
#[derive(Clone)]
struct NodeRender {
    id: NodeId,
    glyph: &'static str,
    kind_label: &'static str,
    title: String,
    summary: String,
    aria: String,
    /// Full `class` attribute — `fc-node`, the `fc-node--{family}` tint, the
    /// `selected`/`connect-src` state, and the `fc-node--{status}` run
    /// modifier when the overlay is live. A `String`, since the family and
    /// status modifiers are data-driven (`canvas-visual-spec.md` §3, §5).
    css_class: String,
    card_style: String,
    has_input: bool,
    in_port_top: f64,
    out_ports: Vec<OutPortRender>,
    /// The node's run-overlay status, when a run overlay is being painted.
    /// Drives the `fc-node--{status}` class and the `.fc-node-status` row.
    run_status: Option<RunOverlayStatus>,
}

#[derive(Clone)]
struct OutPortRender {
    name: String,
    is_err: bool,
    top: f64,
    /// The visible `.fc-port-label` text. For the `err` port this is the
    /// literal "on error" — distinct by label as well as shape, never colour
    /// alone (`canvas-visual-spec.md` §4, WCAG 1.4.1) — while the wire port
    /// id stays the raw `"err"`.
    label: String,
    /// `err` port footgun copy (`ui-brief.md` §6.3) — shown only when the
    /// port has no outgoing edge.
    warn_unconnected: bool,
}

/// The reusable three-pane editor. `read_only` renders the Definition-tab
/// and run-overlay view: the canvas still pans/zooms and selects, but the
/// palette, ports, and inspector fields are inert.
///
/// `run_overlay` is the optional run-overlay layer — a per-node status map
/// (`canvas-visual-spec.md` §5). It is `None` for plain editing; PURA-266's
/// `GET …/runs/{runId}` poll supplies `Some(map)` to paint a run. As an
/// `Option` prop it defaults to `None`, so plain-editor call sites are
/// unaffected.
/// `on_save` is the optional save hook. When `Some`, the toolbar shows a
/// Save button — disabled while any validation error is outstanding
/// (`ui-brief.md` §4.4) — and clicking it hands the current [`FlowGraph`]
/// to the caller, which owns the create-vs-update API call and the
/// flow-level fields (name, server). `None` (the dev mount, the
/// Definition-tab render) shows no Save button.
#[component]
pub fn FlowCanvasEditor(
    initial: FlowGraph,
    read_only: bool,
    run_overlay: Option<HashMap<NodeId, RunOverlayStatus>>,
    on_save: Option<EventHandler<FlowGraph>>,
) -> Element {
    let mut graph = use_signal(|| initial.clone());
    let mut selected = use_signal(|| None::<NodeId>);
    let mut drag = use_signal(|| None::<DragState>);
    let mut pan_drag = use_signal(|| None::<PanDrag>);
    let mut connect = use_signal(|| None::<ConnectState>);
    let mut kbd_src = use_signal(|| None::<(NodeId, String)>);
    let mut pan = use_signal(|| (0.0_f64, 0.0_f64));
    let mut zoom = use_signal(|| 1.0_f64);
    let mut status = use_signal(|| {
        "Drag a palette chip to add a node · drag a port to wire · scroll to zoom".to_string()
    });

    // --- inline validation (ui-brief.md §4.4) ---------------------------
    // A debounced `POST /api/flows/validate` after every edit. `use_resource`
    // re-runs the closure whenever a signal it reads (`graph`) changes and
    // cancels the previous, still-sleeping future — so a burst of edits (a
    // drag, a fast sequence of clicks) collapses to one validation call once
    // activity settles. `read_only` surfaces skip it: nothing is saved there.
    let gate = use_auth_gate();
    let validation = use_resource(move || {
        let snap = graph();
        let gate = gate.clone();
        async move {
            if read_only || snap.nodes.is_empty() {
                return None;
            }
            gloo_timers::future::TimeoutFuture::new(VALIDATE_DEBOUNCE_MS).await;
            fl::validate_graph(gate, &snap).await.ok()
        }
    });

    // --- shared mutators ------------------------------------------------
    let mut add_from_palette = move |kind: PaletteKind| {
        let pos = Position {
            x: 140.0 + (graph.read().nodes.len() as f64 % 4.0) * 40.0,
            y: 120.0 + graph.read().nodes.len() as f64 * 26.0,
        };
        let id = model::add_node(&mut graph.write(), kind, pos);
        selected.set(Some(id.clone()));
        status.set(format!("Added {} node {}", kind.label(), id.0));
    };

    let mut nudge = move |id: &NodeId, dx: f64, dy: f64| {
        if let Some(n) = graph.write().nodes.iter_mut().find(|n| &n.id == id) {
            n.position.x = (n.position.x + dx).max(0.0);
            n.position.y = (n.position.y + dy).max(0.0);
        }
    };

    let mut delete_node = move |id: &NodeId| {
        model::remove_node(&mut graph.write(), id);
        if selected().as_ref() == Some(id) {
            selected.set(None);
        }
        status.set(format!("Deleted node {} and its edges", id.0));
    };

    // --- canvas pointer handlers ---------------------------------------
    let on_canvas_down = move |evt: PointerEvent| {
        // Reached only on empty canvas — node cards/ports stop propagation.
        let p = evt.client_coordinates();
        let (panx, pany) = pan();
        pan_drag.set(Some(PanDrag {
            start_px: p.x,
            start_py: p.y,
            start_panx: panx,
            start_pany: pany,
        }));
    };
    let on_canvas_move = move |evt: PointerEvent| {
        let p = evt.client_coordinates();
        let z = zoom();
        if let Some(d) = drag()
            && let Some(n) = graph.write().nodes.iter_mut().find(|n| n.id == d.node)
        {
            n.position.x = (d.start_nx + (p.x - d.start_px) / z).max(0.0);
            n.position.y = (d.start_ny + (p.y - d.start_py) / z).max(0.0);
        }
        if let Some(pd) = pan_drag() {
            pan.set((
                pd.start_panx + (p.x - pd.start_px),
                pd.start_pany + (p.y - pd.start_py),
            ));
        }
        if let Some(mut c) = connect() {
            c.ex = c.sx + (p.x - c.start_px) / z;
            c.ey = c.sy + (p.y - c.start_py) / z;
            connect.set(Some(c));
        }
    };
    let on_canvas_up = move |_evt: PointerEvent| {
        drag.set(None);
        pan_drag.set(None);
        if connect.write().take().is_some() {
            status.set("Connection cancelled — released on empty canvas".to_string());
        }
    };
    let on_wheel = move |evt: WheelEvent| {
        evt.prevent_default();
        let dy = evt.data().delta().strip_units().y;
        let factor = if dy < 0.0 { 1.1 } else { 1.0 / 1.1 };
        zoom.set((zoom() * factor).clamp(ZOOM_MIN, ZOOM_MAX));
    };

    // --- snapshot + precomputed render data ----------------------------
    let snapshot = graph();
    let sel = selected();
    let kbd = kbd_src();
    let (panx, pany) = pan();
    let z = zoom();
    let connect_state = connect();
    let editor_class = if read_only {
        "fc-editor read-only"
    } else {
        "fc-editor"
    };

    // Validation result for this render: `None` while the first debounced
    // call is still in flight (or on a read-only surface).
    let validation_snapshot: Option<ValidateGraphResponse> = validation.read().clone().flatten();
    let mut err_nodes: HashSet<String> = HashSet::new();
    let mut err_edges: HashSet<String> = HashSet::new();
    let mut warn_nodes: HashSet<String> = HashSet::new();
    let mut warn_edges: HashSet<String> = HashSet::new();
    if let Some(v) = &validation_snapshot {
        for e in &v.errors {
            if let Some(n) = &e.node {
                err_nodes.insert(n.clone());
            }
            for n in &e.nodes {
                err_nodes.insert(n.clone());
            }
            if let Some(ed) = &e.edge {
                err_edges.insert(ed.clone());
            }
        }
        for w in &v.warnings {
            if let Some(n) = &w.node {
                warn_nodes.insert(n.clone());
            }
            for n in &w.nodes {
                warn_nodes.insert(n.clone());
            }
            if let Some(ed) = &w.edge {
                warn_edges.insert(ed.clone());
            }
        }
    }
    let error_count = validation_snapshot
        .as_ref()
        .map(|v| v.errors.len())
        .unwrap_or(0);
    let warning_count = validation_snapshot
        .as_ref()
        .map(|v| v.warnings.len())
        .unwrap_or(0);
    let has_errors = error_count > 0;
    // The problems summary line + the first few messages for the banner.
    let problem_messages: Vec<(bool, String)> = validation_snapshot
        .as_ref()
        .map(|v| {
            v.errors
                .iter()
                .map(|e| (true, e.message.clone()))
                .chain(v.warnings.iter().map(|w| (false, w.message.clone())))
                .take(6)
                .collect()
        })
        .unwrap_or_default();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let validation_summary = if read_only {
        String::new()
    } else if validation_snapshot.is_none() {
        "Checking\u{2026}".to_string()
    } else if has_errors {
        format!("{error_count} problem{}", plural(error_count))
    } else if warning_count > 0 {
        format!(
            "Valid \u{00b7} {warning_count} warning{}",
            plural(warning_count)
        )
    } else {
        "Valid".to_string()
    };
    let validation_class = if has_errors {
        "fc-vstate err"
    } else if warning_count > 0 {
        "fc-vstate warn"
    } else {
        "fc-vstate ok"
    };

    // Which (node, port) pairs have at least one outgoing edge.
    let connected_ports: std::collections::HashSet<(String, String)> = snapshot
        .edges
        .iter()
        .map(|e| (e.from.node.0.clone(), e.from.port.clone()))
        .collect();

    // Each edge: (id, bezier path, full class). The class folds in the
    // `err`-port styling and the validation state (`invalid` highlights a
    // cycle edge red, `warned` an amber type-hint mismatch — ui-brief §4.4).
    let edge_paths: Vec<(String, String, String)> = snapshot
        .edges
        .iter()
        .filter_map(|e| {
            let from = snapshot.nodes.iter().find(|n| n.id == e.from.node)?;
            let to = snapshot.nodes.iter().find(|n| n.id == e.to.node)?;
            let ports = model::output_ports(&from.kind);
            let idx = ports.iter().position(|p| p.name == e.from.port)?;
            let (x1, y1) = geometry::out_port_pos(from.position, idx);
            let (x2, y2) = geometry::in_port_pos(to.position);
            let mut class = "fc-edge".to_string();
            if ports[idx].is_err {
                class.push_str(" err");
            }
            if err_edges.contains(&e.id.0) {
                class.push_str(" invalid");
            } else if warn_edges.contains(&e.id.0) {
                class.push_str(" warned");
            }
            Some((e.id.0.clone(), geometry::bezier(x1, y1, x2, y2), class))
        })
        .collect();

    let node_renders: Vec<NodeRender> = snapshot
        .nodes
        .iter()
        .map(|n| {
            let pk = model::palette_kind(&n.kind);
            let out_ports = model::output_ports(&n.kind)
                .into_iter()
                .enumerate()
                .map(|(idx, p)| OutPortRender {
                    warn_unconnected: p.is_err
                        && !connected_ports.contains(&(n.id.0.clone(), p.name.clone())),
                    // The err port reads "on error" — a label cue, not the
                    // raw wire id; circles vs the err port's square carry
                    // the shape cue (canvas-visual-spec §4).
                    label: if p.is_err {
                        "on error".to_string()
                    } else {
                        p.name.clone()
                    },
                    name: p.name,
                    is_err: p.is_err,
                    top: geometry::out_port_css_top(idx),
                })
                .collect::<Vec<_>>();
            let run_status = run_overlay.as_ref().and_then(|m| m.get(&n.id)).copied();
            // fc-node · family tint · selection/connect state · run status —
            // family and status are data-driven, hence the String build.
            let mut css_class = format!("fc-node fc-node--{}", pk.family());
            if sel.as_ref() == Some(&n.id) {
                css_class.push_str(" selected");
            } else if kbd.as_ref().map(|(id, _)| id) == Some(&n.id) {
                css_class.push_str(" connect-src");
            }
            if let Some(st) = run_status {
                css_class.push_str(" fc-node--");
                css_class.push_str(st.class_suffix());
            }
            // Validation highlight — a cycle/port error rings the node red,
            // a warning amber (`ui-brief.md` §4.4). Errors win over warnings.
            if err_nodes.contains(&n.id.0) {
                css_class.push_str(" invalid");
            } else if warn_nodes.contains(&n.id.0) {
                css_class.push_str(" warned");
            }
            // The overlay status row adds a body line — grow the card so it
            // does not spill past the rounded border.
            let card_h = geometry::node_height(out_ports.len())
                + if run_status.is_some() { 24.0 } else { 0.0 };
            let title = n.label.clone().unwrap_or_else(|| n.id.0.clone());
            // The run status rides in the node's accessible name too, so it
            // is not glyph-/colour-only for assistive tech (WCAG 1.4.1).
            let aria = match run_status {
                Some(st) => format!("{} node {} \u{2014} {}", pk.label(), title, st.label()),
                None => format!("{} node {}", pk.label(), title),
            };
            NodeRender {
                id: n.id.clone(),
                glyph: pk.glyph(),
                kind_label: pk.label(),
                aria,
                title,
                summary: node_summary(&n.kind),
                css_class,
                card_style: format!(
                    "left: {}px; top: {}px; width: {NODE_W}px; height: {card_h}px;",
                    n.position.x, n.position.y,
                ),
                has_input: model::has_input(&n.kind),
                in_port_top: geometry::in_port_css_top(),
                out_ports,
                run_status,
            }
        })
        .collect();

    let node_count = snapshot.nodes.len();
    let edge_count = snapshot.edges.len();
    let trigger_present = model::has_trigger(&snapshot);

    rsx! {
        style { dangerous_inner_html: CANVAS_CSS }
        style { dangerous_inner_html: VALIDATION_CSS }
        div { class: "{editor_class}",

            // --- palette ---------------------------------------------
            section { class: "fc-palette", "aria-label": "node palette",
                p { class: "fc-pane-title", "Palette" }
                for kind in PaletteKind::ALL {
                    button {
                        class: "fc-chip fc-chip--{kind.family()}",
                        r#type: "button",
                        disabled: read_only
                            || (kind == PaletteKind::Trigger && trigger_present),
                        title: if kind == PaletteKind::Trigger && trigger_present {
                            "A flow has exactly one trigger"
                        } else {
                            kind.description()
                        },
                        onclick: move |_| add_from_palette(kind),
                        span { class: "fc-chip-head",
                            span { class: "fc-glyph", "aria-hidden": "true", "{kind.glyph()}" }
                            "{kind.label()}"
                        }
                        span { class: "fc-chip-desc", "{kind.description()}" }
                    }
                }
            }

            // --- canvas ----------------------------------------------
            div {
                class: if pan_drag.read().is_some() { "fc-canvas-wrap panning" } else { "fc-canvas-wrap" },
                onpointerdown: on_canvas_down,
                onpointermove: on_canvas_move,
                onpointerup: on_canvas_up,
                onpointerleave: on_canvas_up,
                onwheel: on_wheel,

                div {
                    class: "fc-world",
                    style: "transform: translate({panx}px, {pany}px) scale({z});",

                    svg { class: "fc-edges", width: "4000", height: "3000",
                        for (id , d , class) in edge_paths {
                            path {
                                key: "{id}",
                                class: "{class}",
                                d: "{d}",
                            }
                        }
                        if let Some(c) = connect_state {
                            path {
                                class: "fc-edge preview",
                                d: geometry::bezier(c.sx, c.sy, c.ex, c.ey),
                            }
                        }
                    }

                    for nr in node_renders {
                        div {
                            key: "{nr.id.0}",
                            class: "{nr.css_class}",
                            style: "{nr.card_style}",
                            tabindex: "0",
                            role: "group",
                            "aria-label": "{nr.aria}",
                            onpointerdown: {
                                let id = nr.id.clone();
                                move |evt: PointerEvent| {
                                    evt.stop_propagation();
                                    selected.set(Some(id.clone()));
                                }
                            },
                            onfocus: {
                                let id = nr.id.clone();
                                move |_| selected.set(Some(id.clone()))
                            },
                            onkeydown: {
                                let id = nr.id.clone();
                                move |evt: KeyboardEvent| {
                                    if read_only {
                                        return;
                                    }
                                    match evt.key() {
                                        Key::ArrowUp => { evt.prevent_default(); nudge(&id, 0.0, -KBD_STEP); }
                                        Key::ArrowDown => { evt.prevent_default(); nudge(&id, 0.0, KBD_STEP); }
                                        Key::ArrowLeft => { evt.prevent_default(); nudge(&id, -KBD_STEP, 0.0); }
                                        Key::ArrowRight => { evt.prevent_default(); nudge(&id, KBD_STEP, 0.0); }
                                        Key::Character(s) if s == "c" => {
                                            evt.prevent_default();
                                            match kbd_src() {
                                                None => {
                                                    kbd_src.set(Some((id.clone(), "out".to_string())));
                                                    selected.set(Some(id.clone()));
                                                    status.set(format!(
                                                        "Connect mode: source {} (out port) — Tab to a target, press c",
                                                        id.0,
                                                    ));
                                                }
                                                Some((src, _)) if src == id => {
                                                    kbd_src.set(None);
                                                    status.set("Connect cancelled (source = target)".to_string());
                                                }
                                                Some((src, port)) => {
                                                    let made = model::connect(
                                                        &mut graph.write(), &src, &port, &id,
                                                    );
                                                    kbd_src.set(None);
                                                    status.set(match made {
                                                        Some(_) => format!("Connected {} → {} (keyboard)", src.0, id.0),
                                                        None => format!("Edge {} → {} rejected (self-loop, duplicate, or no input)", src.0, id.0),
                                                    });
                                                }
                                            }
                                        }
                                        Key::Escape => {
                                            kbd_src.set(None);
                                            status.set("Connect mode cleared".to_string());
                                        }
                                        Key::Delete | Key::Backspace => {
                                            evt.prevent_default();
                                            delete_node(&id);
                                        }
                                        _ => {}
                                    }
                                }
                            },

                            div {
                                class: "fc-node-head",
                                onpointerdown: {
                                    let id = nr.id.clone();
                                    move |evt: PointerEvent| {
                                        evt.stop_propagation();
                                        if read_only {
                                            selected.set(Some(id.clone()));
                                            return;
                                        }
                                        let p = evt.client_coordinates();
                                        let origin = graph.read().nodes.iter()
                                            .find(|n| n.id == id)
                                            .map(|n| (n.position.x, n.position.y));
                                        if let Some((nx, ny)) = origin {
                                            drag.set(Some(DragState {
                                                node: id.clone(),
                                                start_px: p.x, start_py: p.y,
                                                start_nx: nx, start_ny: ny,
                                            }));
                                            selected.set(Some(id.clone()));
                                        }
                                    }
                                },
                                span { class: "fc-glyph", "aria-hidden": "true", "{nr.glyph}" }
                                span { class: "fc-node-title", "{nr.title}" }
                            }
                            div { class: "fc-node-body",
                                div { "{nr.kind_label}" }
                                div { "{nr.summary}" }
                                // Run-overlay status row — glyph + text label,
                                // never colour alone (canvas-visual-spec §5).
                                if let Some(st) = nr.run_status {
                                    div { class: "fc-node-status", role: "status",
                                        span { "aria-hidden": "true", "{st.glyph()}" }
                                        span { "{st.label()}" }
                                    }
                                }
                            }

                            if nr.has_input {
                                div {
                                    class: "fc-port in",
                                    style: "top: {nr.in_port_top}px;",
                                    "aria-label": "input port in",
                                    onpointerup: {
                                        let id = nr.id.clone();
                                        move |evt: PointerEvent| {
                                            evt.stop_propagation();
                                            drag.set(None);
                                            pan_drag.set(None);
                                            if let Some(c) = connect.write().take() {
                                                let made = model::connect(
                                                    &mut graph.write(), &c.src_node, &c.src_port, &id,
                                                );
                                                status.set(match made {
                                                    Some(_) => format!("Connected {} → {}", c.src_node.0, id.0),
                                                    None => "Connection rejected (self-loop, duplicate, or no input)".to_string(),
                                                });
                                            }
                                        }
                                    },
                                }
                            }

                            for port in nr.out_ports {
                                div {
                                    class: if port.is_err { "fc-port out err" } else { "fc-port out" },
                                    style: "top: {port.top}px;",
                                    "aria-label": "output port {port.name}",
                                    title: if port.warn_unconnected {
                                        "If this errors, the run stops here. Wire this port to handle the error instead."
                                    } else {
                                        ""
                                    },
                                    onpointerdown: {
                                        let id = nr.id.clone();
                                        let port_name = port.name.clone();
                                        let is_err = port.is_err;
                                        move |evt: PointerEvent| {
                                            evt.stop_propagation();
                                            if read_only {
                                                return;
                                            }
                                            let p = evt.client_coordinates();
                                            let pos = graph.read().nodes.iter()
                                                .find(|n| n.id == id)
                                                .map(|n| {
                                                    let ports = model::output_ports(&n.kind);
                                                    let idx = ports.iter()
                                                        .position(|pp| pp.name == port_name)
                                                        .unwrap_or(0);
                                                    geometry::out_port_pos(n.position, idx)
                                                });
                                            if let Some((sx, sy)) = pos {
                                                connect.set(Some(ConnectState {
                                                    src_node: id.clone(),
                                                    src_port: port_name.clone(),
                                                    is_err,
                                                    sx, sy,
                                                    start_px: p.x, start_py: p.y,
                                                    ex: sx, ey: sy,
                                                }));
                                                status.set(format!("Wiring from {}:{}", id.0, port_name));
                                            }
                                        }
                                    },
                                    span { class: "fc-port-label", "{port.label}" }
                                }
                            }
                        }
                    }
                }

                // --- canvas toolbar (pan/zoom + Tidy placeholder) -----
                div { class: "fc-toolbar", role: "group", "aria-label": "canvas controls",
                    button {
                        r#type: "button", "aria-label": "zoom out",
                        onclick: move |_| zoom.set((zoom() / 1.2).clamp(ZOOM_MIN, ZOOM_MAX)),
                        "\u{2212}"
                    }
                    span { class: "fc-zoom-label", "{(z * 100.0) as i64}%" }
                    button {
                        r#type: "button", "aria-label": "zoom in",
                        onclick: move |_| zoom.set((zoom() * 1.2).clamp(ZOOM_MIN, ZOOM_MAX)),
                        "+"
                    }
                    button {
                        r#type: "button", "aria-label": "reset view",
                        onclick: move |_| { zoom.set(1.0); pan.set((0.0, 0.0)); },
                        "\u{2302}"
                    }
                    button {
                        r#type: "button", "aria-label": "tidy layout",
                        disabled: read_only,
                        title: "Layered auto-layout — left-to-right by depth",
                        onclick: move |_| {
                            model::tidy_layout(&mut graph.write());
                            status.set("Tidied — nodes laid out left-to-right by depth".to_string());
                        },
                        "Tidy"
                    }
                }
            }

            // --- inspector -------------------------------------------
            Inspector { graph, selected, read_only }

            // --- problems banner (ui-brief.md §4.4) ------------------
            // Inline lint results — errors block Save, warnings do not.
            // Rendered only when there is something to say.
            if !read_only && !problem_messages.is_empty() {
                div {
                    class: if has_errors { "fc-problems" } else { "fc-problems warn" },
                    role: "status",
                    "aria-live": "polite",
                    strong {
                        if has_errors {
                            "{error_count} problem{plural(error_count)} — fix to save"
                        } else {
                            "{warning_count} warning{plural(warning_count)} — save still allowed"
                        }
                    }
                    ul {
                        for (idx , (is_err , msg)) in problem_messages.iter().enumerate() {
                            li {
                                key: "{idx}",
                                class: if *is_err { "fc-problem err" } else { "fc-problem warn" },
                                "{msg}"
                            }
                        }
                    }
                }
            }

            // --- status bar ------------------------------------------
            div { class: "fc-statusbar",
                span { "Nodes: {node_count} / 64" }
                span { "Edges: {edge_count} / 128" }
                if !read_only {
                    span { class: "{validation_class}", "{validation_summary}" }
                }
                span {
                    class: "fc-live",
                    role: "status",
                    "aria-live": "polite",
                    "{status}"
                }
                if let Some(handler) = on_save {
                    button {
                        class: "fc-save",
                        r#type: "button",
                        disabled: read_only || has_errors,
                        title: if has_errors {
                            "Fix the outstanding problems before saving"
                        } else {
                            "Save this flow"
                        },
                        onclick: move |_| {
                            if !has_errors {
                                handler.call(graph());
                                status.set("Saving\u{2026}".to_string());
                            }
                        },
                        "Save"
                    }
                }
            }
        }
    }
}

/// Inline-validation + Save styling (`ui-brief.md` §4.4). Kept separate
/// from the PURA-276 design sheet (`style.rs`): that file is UXDesigner's,
/// this is the editor's own behavioural CSS. Token-aligned all the same —
/// every value resolves from `tokens.css`, scoped under `.fc-*`.
const VALIDATION_CSS: &str = r#"
.fc-node.invalid { box-shadow: var(--shadow-md), 0 0 0 2px var(--danger-fg); }
.fc-node.warned  { box-shadow: var(--shadow-md), 0 0 0 2px var(--warning-fg); }
.fc-edge.invalid { stroke: var(--danger-fg); stroke-width: 3; }
.fc-edge.warned  { stroke: var(--warning-fg); stroke-width: 2; stroke-dasharray: 4 3; }
.fc-vstate { font-weight: var(--weight-semibold); }
.fc-vstate.err  { color: var(--danger-fg); }
.fc-vstate.warn { color: var(--warning-fg); }
.fc-vstate.ok   { color: var(--success-fg); }
.fc-save { margin-left: auto; padding: var(--space-2) var(--space-5);
  border: 1px solid var(--accent-fg); border-radius: var(--radius-md);
  background: var(--accent-fg); color: var(--bg-surface);
  font: inherit; font-weight: var(--weight-semibold); cursor: pointer; }
.fc-save:disabled { opacity: .5; cursor: not-allowed; }
.fc-problems { grid-column: 1 / -1; padding: var(--space-3) var(--space-4);
  background: var(--danger-bg); color: var(--danger-fg);
  border-top: 1px solid var(--border-subtle);
  font-family: var(--font-sans); font-size: var(--text-2xs);
  line-height: var(--lh-2xs); }
.fc-problems.warn { background: var(--warning-bg); color: var(--warning-fg); }
.fc-problems ul { margin: var(--space-1) 0 0; padding-left: var(--space-5); }
.fc-problems li { margin-top: 2px; }
"#;

/// A static demo graph for the dev mount — one node of every kind, each
/// tagged with a run-overlay status. Renders every family colour and every
/// overlay state on one screen so QA can run the greyscale / live-font /
/// both-theme checks (PURA-279 acceptance) without a live run.
#[cfg(debug_assertions)]
fn overlay_demo() -> (FlowGraph, HashMap<NodeId, RunOverlayStatus>) {
    let mut graph = FlowGraph {
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    let mut overlay = HashMap::new();
    // Five of the seven get a status — covering all five overlay states;
    // `transform`/`subflow` stay un-run so the no-overlay card also shows.
    let demo: [(PaletteKind, Option<RunOverlayStatus>); 7] = [
        (PaletteKind::Trigger, Some(RunOverlayStatus::Ok)),
        (PaletteKind::Action, Some(RunOverlayStatus::Running)),
        (PaletteKind::Branch, Some(RunOverlayStatus::Skipped)),
        (PaletteKind::Parallel, Some(RunOverlayStatus::Errored)),
        (PaletteKind::Delay, Some(RunOverlayStatus::Interrupted)),
        (PaletteKind::Transform, None),
        (PaletteKind::Subflow, None),
    ];
    for (i, (kind, status)) in demo.into_iter().enumerate() {
        let pos = Position {
            x: 48.0 + (i % 4) as f64 * 248.0,
            y: 40.0 + (i / 4) as f64 * 232.0,
        };
        let id = model::add_node(&mut graph, kind, pos);
        if let Some(st) = status {
            overlay.insert(id, st);
        }
    }
    (graph, overlay)
}

/// `/dev/flow-canvas` — debug-only mount for the v2 canvas editor while the
/// v2 HTTP surface ([PURA-266](/PURA/issues/PURA-266)) lands. Gated by
/// `cfg(debug_assertions)` at the route enum, exactly as the spike route
/// was; the production swap of `/flows/new`, `/flows/{id}/edit`, and the
/// Definition tab happens once save/validate/overlay can be wired.
#[cfg(debug_assertions)]
#[component]
pub fn DevFlowCanvasPage() -> Element {
    let (demo_graph, demo_overlay) = overlay_demo();
    rsx! {
        main {
            style: "padding: 16px; max-width: 1280px; margin: 0 auto;",
            h1 { style: "font-size: 17px; margin: 0 0 4px;",
                "Flow canvas — v2 visual builder (dev mount)"
            }
            p { style: "font-size: 12px; color: #5a6378; margin: 0 0 12px;",
                "PURA-267 — three-pane canvas editor on the v2 wire types. "
                "Save / validate / run-overlay poll land with PURA-266's HTTP surface."
            }
            FlowCanvasEditor { initial: model::starter_graph(), read_only: false }

            h2 { style: "font-size: 15px; margin: 24px 0 4px;",
                "Run-overlay demo — seven kinds, five statuses"
            }
            p { style: "font-size: 12px; color: #5a6378; margin: 0 0 12px;",
                "Static read-only render for the PURA-279 QA pass: per-kind family "
                "colour, the err-port \"on error\" label, and all five run statuses "
                "(greyscale + live-font check)."
            }
            FlowCanvasEditor {
                initial: demo_graph,
                read_only: true,
                run_overlay: Some(demo_overlay),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts6_manager_shared::flows::Trigger;

    #[test]
    fn node_summary_is_a_legible_one_liner_per_kind() {
        assert_eq!(
            node_summary(&NodeKind::Trigger {
                config: Trigger::ManualFire
            }),
            "manual fire"
        );
        assert_eq!(
            node_summary(&NodeKind::Delay {
                r#for: "45s".into()
            }),
            "wait 45s"
        );
    }
}
