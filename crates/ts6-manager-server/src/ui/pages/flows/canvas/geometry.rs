//! Canvas geometry — derived port coordinates and bezier edge paths.
//!
//! The Option-A spike ([PURA-264](/PURA/issues/PURA-264)) proved the key
//! property carried into production here: **port centres are derived, never
//! measured**. A port's pixel position is `node.position + a fixed offset`,
//! and the same constants drive both the CSS dot placement and the SVG edge
//! endpoints — so HTML ports and SVG edges stay pixel-aligned with zero
//! `getBoundingClientRect` calls, and the maths survives pan/zoom unchanged
//! (the pan/zoom transform is a single CSS `transform` on the world layer).

use ts6_manager_shared::flows::v2::Position;

/// Node card width, px (canvas/world coordinates).
pub const NODE_W: f64 = 208.0;
/// Node header height, px — the drag handle.
pub const HEAD_H: f64 = 34.0;
/// Y offset of the (single) input port centre from the node's top edge.
pub const PORT_IN_DY: f64 = 56.0;
/// Y offset of the first output port centre from the node's top edge.
pub const PORT_OUT_DY0: f64 = 56.0;
/// Vertical spacing between successive output ports.
pub const PORT_OUT_STEP: f64 = 28.0;
/// Port dot radius, px.
pub const PORT_R: f64 = 7.0;
/// Keyboard nudge step (arrow keys), px.
pub const KBD_STEP: f64 = 16.0;
/// Minimum and maximum wheel-zoom factors (`ui-brief.md` §4.2 — "clamped").
pub const ZOOM_MIN: f64 = 0.35;
pub const ZOOM_MAX: f64 = 2.5;

/// Node card height for a node with `out_count` output ports. The card
/// grows downward so every output port dot has room — a `branch` with many
/// cases is taller than an `action`.
pub fn node_height(out_count: usize) -> f64 {
    let ports_extent = PORT_OUT_DY0 + (out_count.max(1) as f64) * PORT_OUT_STEP;
    ports_extent.max(96.0)
}

/// Centre of output port `idx` in world coordinates.
pub fn out_port_pos(node: Position, idx: usize) -> (f64, f64) {
    (
        node.x + NODE_W,
        node.y + PORT_OUT_DY0 + idx as f64 * PORT_OUT_STEP,
    )
}

/// Centre of the input port in world coordinates.
pub fn in_port_pos(node: Position) -> (f64, f64) {
    (node.x, node.y + PORT_IN_DY)
}

/// CSS `top:` for output port `idx`, relative to the node card box.
pub fn out_port_css_top(idx: usize) -> f64 {
    PORT_OUT_DY0 + idx as f64 * PORT_OUT_STEP - PORT_R
}

/// CSS `top:` for the input port, relative to the node card box.
pub fn in_port_css_top() -> f64 {
    PORT_IN_DY - PORT_R
}

/// A cubic bezier path string with horizontal control handles — the
/// React-Flow edge look. Handle length scales with the horizontal span,
/// floored so short or backward edges still curve cleanly.
pub fn bezier(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    let c = ((x2 - x1).abs() / 2.0).max(48.0);
    format!(
        "M {x1:.1} {y1:.1} C {:.1} {y1:.1} {:.1} {y2:.1} {x2:.1} {y2:.1}",
        x1 + c,
        x2 - c
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_ports_step_down_from_a_fixed_origin() {
        let n = Position { x: 100.0, y: 40.0 };
        let (x0, y0) = out_port_pos(n, 0);
        let (x1, y1) = out_port_pos(n, 1);
        // All output ports sit on the node's right edge.
        assert_eq!(x0, 100.0 + NODE_W);
        assert_eq!(x1, 100.0 + NODE_W);
        // Successive ports are one step apart.
        assert_eq!(y1 - y0, PORT_OUT_STEP);
    }

    #[test]
    fn node_grows_to_fit_its_output_ports() {
        // A two-port action and a six-case branch get different heights.
        assert!(node_height(6) > node_height(2));
        // A single-port node still gets the minimum card height.
        assert_eq!(node_height(1), 96.0);
    }

    #[test]
    fn bezier_emits_a_cubic_path_through_both_endpoints() {
        let d = bezier(0.0, 0.0, 200.0, 100.0);
        assert!(d.starts_with("M 0.0 0.0 C"), "got: {d}");
        assert!(d.ends_with("200.0 100.0"), "got: {d}");
    }
}
