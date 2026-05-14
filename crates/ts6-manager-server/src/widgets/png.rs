//! PNG rasterisation pipeline (spec §27.4) — PURA-72 Slice C.
//!
//! Render the Slice B SVG (per [`super::svg::render`]) onto a 400 px-wide
//! `tiny_skia::Pixmap`, then encode the pixmap to PNG. The function is
//! pure and CPU-bound; the route handler hops into [`tokio::task::spawn_blocking`]
//! so a slow rasterisation does not stall the runtime.
//!
//! Spec §27.4 mandates a graceful fallback: if the rasteriser is
//! unavailable or fails for any reason, the route serves the SVG bytes
//! at the `/image.png` URL with `Content-Type: image/svg+xml`. This
//! module surfaces that distinction via [`RasterError`]; it never
//! panics. The route owns the fallback decision and the WARN log line —
//! that keeps the renderer reusable from the upcoming public widget
//! HTML page (Slice E) without coupling the log message to a specific
//! call site.
//!
//! ## Force-disabled mode
//!
//! The `widget-png-disabled` Cargo feature short-circuits [`rasterise`]
//! to `Err(RasterError::Disabled)`. Tests run under that feature pin the
//! §27.4 fallback contract end-to-end (`/image.png` → SVG bytes + WARN
//! log) without needing to hand the renderer malformed input. Default
//! builds keep the rasteriser live.
//!
//! ## Fonts
//!
//! `usvg::Options::default()` ships with an empty `fontdb`. The
//! `system-fonts` feature is enabled at the workspace level, so the
//! first call to [`rasterise`] populates a process-wide
//! [`std::sync::OnceLock`] with `usvg::fontdb::Database::load_system_fonts`
//! and reuses it on subsequent calls. Deployments without any system
//! fonts (minimal containers) still get a usable PNG of the layout —
//! text falls back to whatever glyphs `tiny-skia` has by default —
//! and operators who need pixel-perfect typography can mount a font
//! directory at `/usr/share/fonts` or rely on the SVG fallback.

#[cfg(not(feature = "widget-png-disabled"))]
use std::sync::{Arc, OnceLock};

/// Target output width per spec §27.4. The SVG renderer (§27.3) uses
/// the same 400 px nominal width, so the rasteriser scales 1:1 in the X
/// axis and proportionally in Y based on the SVG's `viewBox` height.
pub const TARGET_WIDTH: u32 = 400;

/// Why the rasteriser declined to produce PNG bytes. Every variant is a
/// signal to the route handler to fall back to SVG (spec §27.4) — the
/// caller MUST NOT surface a 5xx for any of these.
#[derive(Debug)]
pub enum RasterError {
    /// `widget-png-disabled` feature is on. Tests pin the fallback path.
    Disabled,
    /// `usvg::Tree::from_str` rejected the SVG input.
    Parse(String),
    /// `tiny_skia::Pixmap::new` returned `None` — width/height of zero
    /// or an allocation that overflows. Should not happen for our SVG
    /// inputs but the spec's "rasteriser fails" clause covers it.
    Pixmap,
    /// `tiny_skia::Pixmap::encode_png` failed.
    Encode(String),
}

impl std::fmt::Display for RasterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RasterError::Disabled => f.write_str("widget PNG rasteriser disabled"),
            RasterError::Parse(e) => write!(f, "SVG parse failed: {e}"),
            RasterError::Pixmap => f.write_str("pixmap allocation failed"),
            RasterError::Encode(e) => write!(f, "PNG encode failed: {e}"),
        }
    }
}

impl std::error::Error for RasterError {}

/// Force-disabled build — short-circuit before pulling `resvg` into
/// scope. Compiles in tests under `--features widget-png-disabled`.
#[cfg(feature = "widget-png-disabled")]
pub fn rasterise(_svg: &str) -> Result<Vec<u8>, RasterError> {
    Err(RasterError::Disabled)
}

/// Live-rasteriser build. Parses the SVG with `usvg`, scales onto a
/// 400 px-wide `tiny_skia::Pixmap`, and encodes the result to PNG.
#[cfg(not(feature = "widget-png-disabled"))]
pub fn rasterise(svg: &str) -> Result<Vec<u8>, RasterError> {
    let opts = build_options();
    let tree = usvg::Tree::from_str(svg, &opts).map_err(|e| RasterError::Parse(e.to_string()))?;

    let svg_size = tree.size();
    let svg_width = svg_size.width().max(1.0);
    let svg_height = svg_size.height().max(1.0);
    let scale = TARGET_WIDTH as f32 / svg_width;
    let pixmap_h = (svg_height * scale).round().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(TARGET_WIDTH, pixmap_h).ok_or(RasterError::Pixmap)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    pixmap
        .encode_png()
        .map_err(|e| RasterError::Encode(e.to_string()))
}

/// PNG signature used for verification (spec §27.4 / §27.6 round-trip).
/// First 8 bytes of every PNG file: 0x89 P N G CR LF SUB LF.
pub const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[cfg(not(feature = "widget-png-disabled"))]
fn build_options() -> usvg::Options<'static> {
    usvg::Options {
        fontdb: system_fontdb(),
        ..usvg::Options::default()
    }
}

#[cfg(not(feature = "widget-png-disabled"))]
fn system_fontdb() -> Arc<usvg::fontdb::Database> {
    static FONTDB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTDB
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid SVG that exercises the full pipeline (parse →
    /// pixmap → encode) without depending on the Slice B renderer's
    /// shape. The widget renderer's own SVG is exercised end-to-end in
    /// the route-level test alongside this one.
    // `r##"..."##` (not `r#"..."#`) so the SVG body's `"#102030"` /
    // `"#3CBEEF"` hex colour literals don't terminate the raw string.
    const TINY_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="100" viewBox="0 0 400 100"><rect x="0" y="0" width="400" height="100" fill="#102030"/><circle cx="200" cy="50" r="30" fill="#3CBEEF"/></svg>"##;

    #[cfg(not(feature = "widget-png-disabled"))]
    #[test]
    fn round_trip_emits_png_signature() {
        let bytes = rasterise(TINY_SVG).expect("rasterise should succeed for a valid SVG");
        assert!(
            bytes.len() > PNG_SIGNATURE.len(),
            "expected non-empty PNG output, got {} bytes",
            bytes.len()
        );
        assert_eq!(
            &bytes[..PNG_SIGNATURE.len()],
            &PNG_SIGNATURE,
            "PNG byte stream must begin with the PNG signature"
        );
    }

    #[cfg(not(feature = "widget-png-disabled"))]
    #[test]
    fn slice_b_svg_round_trips_to_png() {
        // Wire the actual Slice B renderer into the round trip so any
        // future change to the SVG envelope (header, footer, badge)
        // that breaks `usvg` parsing is caught here, not at runtime.
        use crate::widgets::svg;
        use crate::widgets::themes::WIDGET_THEME_DARK;
        use ts6_manager_shared::widgets::{
            SpacerType, WidgetChannelNode, WidgetClient, WidgetData, WidgetServer,
        };

        let data = WidgetData {
            name: "Round-trip".into(),
            theme: "dark".into(),
            server_config_id: 1,
            show_channel_tree: true,
            show_clients: true,
            hide_empty_channels: false,
            max_channel_depth: 5,
            server: WidgetServer {
                name: "Round-Trip Server".into(),
                clients_online: 1,
                max_clients: 32,
                uptime_seconds: 3600,
                platform: "TeamSpeak".into(),
                version: String::new(),
            },
            channels: vec![
                WidgetChannelNode {
                    cid: 1,
                    name: "Lobby".into(),
                    has_password: true,
                    clients: vec![WidgetClient {
                        clid: 1,
                        nickname: "Alice".into(),
                        is_away: false,
                        is_muted: false,
                    }],
                    children: Vec::new(),
                    is_spacer: false,
                    spacer_type: SpacerType::None,
                    spacer_text: String::new(),
                },
                WidgetChannelNode {
                    cid: 2,
                    name: "[cspacer]Hello".into(),
                    has_password: false,
                    clients: Vec::new(),
                    children: Vec::new(),
                    is_spacer: true,
                    spacer_type: SpacerType::Center,
                    spacer_text: "Hello".into(),
                },
            ],
        };
        let svg = svg::render(&data, &WIDGET_THEME_DARK);
        let png = rasterise(&svg).expect("Slice B SVG should rasterise");
        assert_eq!(&png[..PNG_SIGNATURE.len()], &PNG_SIGNATURE);
    }

    #[cfg(not(feature = "widget-png-disabled"))]
    #[test]
    fn malformed_svg_returns_parse_error() {
        let err = rasterise("<not-a-real-svg>").expect_err("malformed SVG must fail");
        assert!(
            matches!(err, RasterError::Parse(_)),
            "expected Parse error, got {err:?}"
        );
    }

    #[cfg(feature = "widget-png-disabled")]
    #[test]
    fn disabled_feature_short_circuits_to_err() {
        let err = rasterise(TINY_SVG).expect_err("disabled rasteriser must error");
        assert!(matches!(err, RasterError::Disabled));
    }
}
