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
//! `POST /api/flows/validate` inline linting, the run-overlay poll, save,
//! and the "Tidy" auto-layout. The toolbar carries a disabled Tidy button
//! as the placement marker.

use dioxus::prelude::*;
use ts6_manager_shared::flows::v2::{FlowGraph, NodeId, NodeKind, Position};

use super::geometry::{self, KBD_STEP, NODE_W, ZOOM_MAX, ZOOM_MIN};
use super::inspector::Inspector;
use super::model::{self, PaletteKind};
use super::style::CANVAS_CSS;

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
    css_class: &'static str,
    card_style: String,
    has_input: bool,
    in_port_top: f64,
    out_ports: Vec<OutPortRender>,
}

#[derive(Clone)]
struct OutPortRender {
    name: String,
    is_err: bool,
    top: f64,
    /// `err` port footgun copy (`ui-brief.md` §6.3) — shown only when the
    /// port has no outgoing edge.
    warn_unconnected: bool,
}

/// The reusable three-pane editor. `read_only` renders the Definition-tab
/// and run-overlay view: the canvas still pans/zooms and selects, but the
/// palette, ports, and inspector fields are inert.
#[component]
pub fn FlowCanvasEditor(initial: FlowGraph, read_only: bool) -> Element {
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
        if let Some(d) = drag() {
            if let Some(n) = graph.write().nodes.iter_mut().find(|n| n.id == d.node) {
                n.position.x = (d.start_nx + (p.x - d.start_px) / z).max(0.0);
                n.position.y = (d.start_ny + (p.y - d.start_py) / z).max(0.0);
            }
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

    // Which (node, port) pairs have at least one outgoing edge.
    let connected_ports: std::collections::HashSet<(String, String)> = snapshot
        .edges
        .iter()
        .map(|e| (e.from.node.0.clone(), e.from.port.clone()))
        .collect();

    let edge_paths: Vec<(String, String, bool)> = snapshot
        .edges
        .iter()
        .filter_map(|e| {
            let from = snapshot.nodes.iter().find(|n| n.id == e.from.node)?;
            let to = snapshot.nodes.iter().find(|n| n.id == e.to.node)?;
            let ports = model::output_ports(&from.kind);
            let idx = ports.iter().position(|p| p.name == e.from.port)?;
            let is_err = ports[idx].is_err;
            let (x1, y1) = geometry::out_port_pos(from.position, idx);
            let (x2, y2) = geometry::in_port_pos(to.position);
            Some((e.id.0.clone(), geometry::bezier(x1, y1, x2, y2), is_err))
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
                    name: p.name,
                    is_err: p.is_err,
                    top: geometry::out_port_css_top(idx),
                })
                .collect::<Vec<_>>();
            let css_class = if sel.as_ref() == Some(&n.id) {
                "fc-node selected"
            } else if kbd.as_ref().map(|(id, _)| id) == Some(&n.id) {
                "fc-node connect-src"
            } else {
                "fc-node"
            };
            let title = n.label.clone().unwrap_or_else(|| n.id.0.clone());
            NodeRender {
                id: n.id.clone(),
                glyph: pk.glyph(),
                kind_label: pk.label(),
                aria: format!("{} node {}", pk.label(), title),
                title,
                summary: node_summary(&n.kind),
                css_class,
                card_style: format!(
                    "left: {}px; top: {}px; width: {NODE_W}px; height: {}px;",
                    n.position.x,
                    n.position.y,
                    geometry::node_height(out_ports.len()),
                ),
                has_input: model::has_input(&n.kind),
                in_port_top: geometry::in_port_css_top(),
                out_ports,
            }
        })
        .collect();

    let node_count = snapshot.nodes.len();
    let edge_count = snapshot.edges.len();
    let trigger_present = model::has_trigger(&snapshot);

    rsx! {
        style { dangerous_inner_html: CANVAS_CSS }
        div { class: "{editor_class}",

            // --- palette ---------------------------------------------
            section { class: "fc-palette", "aria-label": "node palette",
                p { class: "fc-pane-title", "Palette" }
                for kind in PaletteKind::ALL {
                    button {
                        class: "fc-chip",
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
                        for (id , d , is_err) in edge_paths {
                            path {
                                key: "{id}",
                                class: if is_err { "fc-edge err" } else { "fc-edge" },
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
                                    span { class: "fc-port-label", "{port.name}" }
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
                        disabled: true,
                        title: "Auto-layout — pending PURA-267 follow-up",
                        "Tidy"
                    }
                }
            }

            // --- inspector -------------------------------------------
            Inspector { graph, selected, read_only }

            // --- status bar ------------------------------------------
            div { class: "fc-statusbar",
                span { "Nodes: {node_count} / 64" }
                span { "Edges: {edge_count} / 128" }
                span {
                    class: "fc-live",
                    role: "status",
                    "aria-live": "polite",
                    "{status}"
                }
            }
        }
    }
}

/// `/dev/flow-canvas` — debug-only mount for the v2 canvas editor while the
/// v2 HTTP surface ([PURA-266](/PURA/issues/PURA-266)) lands. Gated by
/// `cfg(debug_assertions)` at the route enum, exactly as the spike route
/// was; the production swap of `/flows/new`, `/flows/{id}/edit`, and the
/// Definition tab happens once save/validate/overlay can be wired.
#[cfg(debug_assertions)]
#[component]
pub fn DevFlowCanvasPage() -> Element {
    rsx! {
        main {
            style: "padding: 16px; max-width: 1280px; margin: 0 auto;",
            h1 { style: "font-size: 17px; margin: 0 0 4px;",
                "Flow canvas — v2 visual builder (dev mount)"
            }
            p { style: "font-size: 12px; color: #5a6378; margin: 0 0 12px;",
                "PURA-267 — three-pane canvas editor on the v2 wire types. "
                "Save / validate / run-overlay land with PURA-266's HTTP surface."
            }
            FlowCanvasEditor { initial: model::starter_graph(), read_only: false }
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
