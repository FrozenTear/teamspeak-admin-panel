//! SVG rendering pipeline (spec §27.3) — PURA-72 Slice B.
//!
//! Render `WidgetData` × `WidgetTheme` to a self-contained SVG document.
//! Two-pass: walk the tree once to flatten into a `Vec<Row>` so we know
//! the total height before emitting the outer `<svg>` element, then walk
//! the row list once to emit the body. Header / separators / footer wrap
//! the row block.
//!
//! **Layout constants** (§27.3):
//! - WIDTH 400 (fixed); height dynamic.
//! - PADDING 14 / HEADER_HEIGHT 72 / CHANNEL_ROW_HEIGHT 22 /
//!   CLIENT_ROW_HEIGHT 18 / FOOTER_HEIGHT 28.
//! - font-family: 'Segoe UI', 'Helvetica Neue', Arial, sans-serif.
//!
//! Every operator-supplied string (server name, channel name, client
//! nickname, spacer text) goes through [`xml_escape`] before insertion.
//! Numeric formatting uses `format!` (no operator input).

use ts6_manager_shared::widgets::{SpacerType, WidgetChannelNode, WidgetData};

use super::themes::WidgetTheme;

const WIDTH: u32 = 400;
const PADDING: u32 = 14;
const HEADER_HEIGHT: u32 = 72;
const CHANNEL_ROW_HEIGHT: u32 = 22;
const CLIENT_ROW_HEIGHT: u32 = 18;
const FOOTER_HEIGHT: u32 = 28;
const FONT_FAMILY: &str = "'Segoe UI', 'Helvetica Neue', Arial, sans-serif";
const FOOTER_CAPTION: &str = "TS6 WebUI Widget";
const ONLINE_LABEL: &str = "ONLINE";
const ONLINE_BADGE_W: u32 = 52;
const ONLINE_BADGE_H: u32 = 18;

/// Flat row variants the body emitter walks. Built by [`flatten_tree`].
#[derive(Debug, Clone)]
enum Row {
    /// Spacer — `<line>` across the body. `dasharray` is `Some(_)` for
    /// dotline/dashline, `None` for solid `line`.
    SpacerLine { dasharray: Option<&'static str> },
    /// Spacer — text (left/center/right anchored).
    SpacerText { text: String, anchor: TextAnchor },
    /// Real channel row.
    Channel {
        depth: u32,
        name: String,
        has_password: bool,
        client_count: u32,
    },
    /// Single human client under a channel.
    Client {
        depth: u32,
        nickname: String,
        is_away: bool,
        is_muted: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum TextAnchor {
    Start,
    Middle,
    End,
}

impl TextAnchor {
    fn as_attr(self) -> &'static str {
        match self {
            TextAnchor::Start => "start",
            TextAnchor::Middle => "middle",
            TextAnchor::End => "end",
        }
    }
}

impl Row {
    fn height(&self) -> u32 {
        match self {
            Row::SpacerLine { .. } | Row::SpacerText { .. } | Row::Channel { .. } => {
                CHANNEL_ROW_HEIGHT
            }
            Row::Client { .. } => CLIENT_ROW_HEIGHT,
        }
    }
}

/// Public entry point. Returns the full `<svg>` document as a string.
pub fn render(data: &WidgetData, theme: &WidgetTheme) -> String {
    let rows = flatten_tree(&data.channels, data.show_clients);
    let body_height: u32 = rows.iter().map(Row::height).sum();
    let total_height = HEADER_HEIGHT + body_height + FOOTER_HEIGHT + PADDING * 2;

    let mut out = String::with_capacity(2048 + rows.len() * 128);
    open_svg(&mut out, total_height);
    emit_outer_rect(&mut out, theme, total_height);
    emit_header(&mut out, theme, &data.server);
    emit_separator(&mut out, theme, HEADER_HEIGHT);

    let mut y = HEADER_HEIGHT + PADDING;
    for row in &rows {
        emit_row(&mut out, theme, row, y);
        y += row.height();
    }

    let footer_top = total_height - FOOTER_HEIGHT;
    emit_separator(&mut out, theme, footer_top);
    emit_footer(&mut out, theme, total_height);
    out.push_str("</svg>");
    out
}

/// Walk the tree depth-first, flattening into a renderable row list.
/// Spacers contribute one row only (no client rows even if `clients` was
/// somehow populated). Real channel rows are followed by their direct
/// client rows when `show_clients` is true.
fn flatten_tree(roots: &[WidgetChannelNode], show_clients: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for node in roots {
        walk(node, 0, show_clients, &mut rows);
    }
    rows
}

fn walk(node: &WidgetChannelNode, depth: u32, show_clients: bool, rows: &mut Vec<Row>) {
    if node.is_spacer {
        match node.spacer_type {
            SpacerType::Line => rows.push(Row::SpacerLine { dasharray: None }),
            SpacerType::Dotline => rows.push(Row::SpacerLine {
                dasharray: Some("2,4"),
            }),
            SpacerType::Dashline => rows.push(Row::SpacerLine {
                dasharray: Some("6,4"),
            }),
            SpacerType::Center => rows.push(Row::SpacerText {
                text: node.spacer_text.clone(),
                anchor: TextAnchor::Middle,
            }),
            SpacerType::Right => rows.push(Row::SpacerText {
                text: node.spacer_text.clone(),
                anchor: TextAnchor::End,
            }),
            SpacerType::Left | SpacerType::None => rows.push(Row::SpacerText {
                text: node.spacer_text.clone(),
                anchor: TextAnchor::Start,
            }),
        }
        // Spacers do not own children in TS; if present, walk anyway so a
        // pathologically-built tree still renders the descendants.
        for child in &node.children {
            walk(child, depth, show_clients, rows);
        }
        return;
    }

    let client_count = u32::try_from(node.clients.len()).unwrap_or(u32::MAX);
    rows.push(Row::Channel {
        depth,
        name: node.name.clone(),
        has_password: node.has_password,
        client_count,
    });
    if show_clients {
        for c in &node.clients {
            rows.push(Row::Client {
                depth,
                nickname: c.nickname.clone(),
                is_away: c.is_away,
                is_muted: c.is_muted,
            });
        }
    }
    for child in &node.children {
        walk(child, depth + 1, show_clients, rows);
    }
}

fn open_svg(out: &mut String, height: u32) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" \
         viewBox=\"0 0 {w} {h}\" font-family=\"{ff}\">",
        w = WIDTH,
        h = height,
        ff = xml_escape(FONT_FAMILY),
    );
}

fn emit_outer_rect(out: &mut String, theme: &WidgetTheme, height: u32) {
    use std::fmt::Write;
    // Body rect (full canvas) in `theme.background`. The header band paints
    // over the top portion in `theme.headerBg`. `rx=10` rounds both corners.
    let _ = write!(
        out,
        "<rect x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\" rx=\"10\" fill=\"{bg}\"/>",
        w = WIDTH,
        h = height,
        bg = theme.background,
    );
    // Header band — same width, only top corners rounded. We achieve that
    // with a clipPath-free trick: paint a full-width rect whose height
    // equals HEADER_HEIGHT then a body-coloured rect below it. The outer
    // rect's `rx=10` already supplies the rounded top corners (the body
    // paint extends past the bottom corners which the outer rounded rect
    // re-clips visually because we paint in z-order header→body separator).
    let _ = write!(
        out,
        "<rect x=\"0\" y=\"0\" width=\"{w}\" height=\"{hh}\" fill=\"{hbg}\"/>",
        w = WIDTH,
        hh = HEADER_HEIGHT,
        hbg = theme.header_bg,
    );
}

fn emit_header(
    out: &mut String,
    theme: &WidgetTheme,
    server: &ts6_manager_shared::widgets::WidgetServer,
) {
    use std::fmt::Write;

    // Server name: accent, 700, size 15. Baseline ~ y=27 keeps the cap
    // height inside the upper third of the 72px header.
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"27\" fill=\"{c}\" font-size=\"15\" font-weight=\"700\">{name}</text>",
        x = PADDING,
        c = theme.accent,
        name = xml_escape(&server.name),
    );

    // ONLINE badge: filled clientColor rect 52x18, right-aligned at y=15.
    let badge_x = WIDTH - PADDING - ONLINE_BADGE_W;
    let badge_text_x = badge_x + ONLINE_BADGE_W / 2;
    let _ = write!(
        out,
        "<rect x=\"{bx}\" y=\"15\" width=\"{bw}\" height=\"{bh}\" rx=\"3\" fill=\"{cc}\"/>\
         <text x=\"{tx}\" y=\"28\" fill=\"#FFFFFF\" font-size=\"11\" font-weight=\"700\" \
         text-anchor=\"middle\" letter-spacing=\"0.6\">{label}</text>",
        bx = badge_x,
        bw = ONLINE_BADGE_W,
        bh = ONLINE_BADGE_H,
        cc = theme.client_color,
        tx = badge_text_x,
        label = ONLINE_LABEL,
    );

    // Stats line below the server name.
    let stats = format!(
        "{}/{} users • {} uptime",
        server.clients_online,
        server.max_clients,
        format_uptime(server.uptime_seconds),
    );
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"50\" fill=\"{c}\" font-size=\"11\">{t}</text>",
        x = PADDING,
        c = theme.text_secondary,
        // `format_uptime` only emits ASCII digits / spaces / `d`/`h`/`m`/`s`;
        // server.clients_online / max_clients are u32 — none of these
        // contain XML metacharacters. No escape needed for the digit/unit
        // tokens; only operator strings (server name, channel name,
        // nickname, spacer text) flow through `xml_escape`.
        t = xml_escape(&stats),
    );
}

fn emit_separator(out: &mut String, theme: &WidgetTheme, y: u32) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{c}\" \
         stroke-width=\"1\"/>",
        x1 = 0,
        x2 = WIDTH,
        y = y,
        c = theme.border,
    );
}

fn emit_footer(out: &mut String, theme: &WidgetTheme, total_height: u32) {
    use std::fmt::Write;
    // Caption baseline ~16px below the separator at total_height-FOOTER_HEIGHT.
    let baseline = total_height - FOOTER_HEIGHT + 18;
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"9\" \
         text-anchor=\"middle\" opacity=\"0.6\">{label}</text>",
        x = WIDTH / 2,
        y = baseline,
        c = theme.text_secondary,
        label = FOOTER_CAPTION,
    );
}

fn emit_row(out: &mut String, theme: &WidgetTheme, row: &Row, top_y: u32) {
    match row {
        Row::SpacerLine { dasharray } => emit_spacer_line(out, theme, top_y, *dasharray),
        Row::SpacerText { text, anchor } => emit_spacer_text(out, theme, top_y, text, *anchor),
        Row::Channel {
            depth,
            name,
            has_password,
            client_count,
        } => emit_channel_row(
            out,
            theme,
            top_y,
            *depth,
            name,
            *has_password,
            *client_count,
        ),
        Row::Client {
            depth,
            nickname,
            is_away,
            is_muted,
        } => emit_client_row(out, theme, top_y, *depth, nickname, *is_away, *is_muted),
    }
}

fn emit_spacer_line(
    out: &mut String,
    theme: &WidgetTheme,
    top_y: u32,
    dasharray: Option<&'static str>,
) {
    use std::fmt::Write;
    // Center the line vertically inside the row.
    let mid = top_y + CHANNEL_ROW_HEIGHT / 2;
    let dash = match dasharray {
        Some(da) => format!(" stroke-dasharray=\"{da}\""),
        None => String::new(),
    };
    let _ = write!(
        out,
        "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{c}\"{dash}/>",
        x1 = PADDING,
        x2 = WIDTH - PADDING,
        y = mid,
        c = theme.border,
        dash = dash,
    );
}

fn emit_spacer_text(
    out: &mut String,
    theme: &WidgetTheme,
    top_y: u32,
    text: &str,
    anchor: TextAnchor,
) {
    use std::fmt::Write;
    let baseline = top_y + 16;
    let x = match anchor {
        TextAnchor::Start => PADDING,
        TextAnchor::Middle => WIDTH / 2,
        TextAnchor::End => WIDTH - PADDING,
    };
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"11\" font-weight=\"600\" \
         letter-spacing=\"0.5\" text-anchor=\"{a}\">{t}</text>",
        x = x,
        y = baseline,
        c = theme.text_secondary,
        a = anchor.as_attr(),
        t = xml_escape(text),
    );
}

fn emit_channel_row(
    out: &mut String,
    theme: &WidgetTheme,
    top_y: u32,
    depth: u32,
    name: &str,
    has_password: bool,
    client_count: u32,
) {
    use std::fmt::Write;
    let indent = PADDING + depth * 16;
    let baseline = top_y + 16;

    // `#` icon
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"12\" font-weight=\"700\">#</text>",
        x = indent,
        y = baseline,
        c = theme.accent,
    );

    // Channel name, truncated.
    let max_chars = saturating_sub(36, depth.saturating_mul(2)).max(1);
    let truncated = truncate_chars(name, max_chars as usize);
    let _ = write!(
        out,
        "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"12\">{t}</text>",
        x = indent + 14,
        y = baseline,
        c = theme.text_primary,
        t = xml_escape(&truncated),
    );

    // Lock emoji and / or client count, both right-aligned. The emoji sits
    // closer to the right edge; the count sits to its left when both are
    // present so neither overlaps.
    let count_anchor_x = if has_password {
        WIDTH - PADDING - 18
    } else {
        WIDTH - PADDING
    };
    if client_count > 0 {
        let _ = write!(
            out,
            "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"10\" \
             text-anchor=\"end\">{n}</text>",
            x = count_anchor_x,
            y = baseline,
            c = theme.text_secondary,
            n = client_count,
        );
    }
    if has_password {
        let _ = write!(
            out,
            "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"12\" \
             text-anchor=\"end\">🔒</text>",
            x = WIDTH - PADDING,
            y = baseline,
            c = theme.text_secondary,
        );
    }
}

fn emit_client_row(
    out: &mut String,
    theme: &WidgetTheme,
    top_y: u32,
    depth: u32,
    nickname: &str,
    is_away: bool,
    is_muted: bool,
) {
    use std::fmt::Write;
    let indent = PADDING + depth * 16;
    let baseline = top_y + 14;

    // Filled circle (radius 3).
    let _ = write!(
        out,
        "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"3\" fill=\"{c}\"/>",
        cx = indent + 10,
        cy = top_y + 9,
        c = theme.client_color,
    );

    // Nickname + status suffixes.
    let max_chars = saturating_sub(32, depth.saturating_mul(2)).max(1);
    let truncated = truncate_chars(nickname, max_chars as usize);
    let suffix = match (is_away, is_muted) {
        (true, true) => " [away] [muted]",
        (true, false) => " [away]",
        (false, true) => " [muted]",
        (false, false) => "",
    };
    if suffix.is_empty() {
        let _ = write!(
            out,
            "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"11\">{t}</text>",
            x = indent + 18,
            y = baseline,
            c = theme.client_color,
            t = xml_escape(&truncated),
        );
    } else {
        let _ = write!(
            out,
            "<text x=\"{x}\" y=\"{y}\" fill=\"{c}\" font-size=\"11\">\
             {t}<tspan fill=\"{cs}\">{s}</tspan>\
             </text>",
            x = indent + 18,
            y = baseline,
            c = theme.client_color,
            cs = theme.text_secondary,
            t = xml_escape(&truncated),
            s = xml_escape(suffix),
        );
    }
}

/// XML-escape `&`, `<`, `>`, `"`, `'`. Spec §27.3 mandates this on every
/// operator-supplied string in the SVG.
pub fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render `seconds` as a human-readable uptime — `Xd Yh` for ≥1 day,
/// `Xh Ym` for ≥1 hour, otherwise `Xm`. Mirrors the dashboard helper so
/// public + operator surfaces match.
fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

fn saturating_sub(a: u32, b: u32) -> u32 {
    a.saturating_sub(b)
}

/// Truncate `s` to `max_chars` Unicode scalar values. If truncation
/// happens, append a single `…` (U+2026). `max_chars` is treated as a hard
/// budget that includes the ellipsis.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let take = max_chars - 1;
    let mut out = String::with_capacity(s.len());
    out.extend(s.chars().take(take));
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::themes::{WIDGET_THEME_DARK, WIDGET_THEME_NEON, theme_for};
    use ts6_manager_shared::widgets::{
        SpacerType, WidgetChannelNode, WidgetClient, WidgetData, WidgetServer,
    };

    fn server() -> WidgetServer {
        WidgetServer {
            name: "Test Server".into(),
            clients_online: 4,
            max_clients: 32,
            uptime_seconds: 90_000, // 1d 1h
            platform: "TeamSpeak".into(),
            version: String::new(),
        }
    }

    fn channel(
        cid: i64,
        name: &str,
        password: bool,
        clients: Vec<WidgetClient>,
    ) -> WidgetChannelNode {
        WidgetChannelNode {
            cid,
            name: name.into(),
            has_password: password,
            clients,
            children: Vec::new(),
            is_spacer: false,
            spacer_type: SpacerType::None,
            spacer_text: String::new(),
        }
    }

    fn spacer(text: &str, ty: SpacerType) -> WidgetChannelNode {
        WidgetChannelNode {
            cid: 0,
            name: format!("[cspacer]{text}"),
            has_password: false,
            clients: Vec::new(),
            children: Vec::new(),
            is_spacer: true,
            spacer_type: ty,
            spacer_text: text.into(),
        }
    }

    fn client(nickname: &str) -> WidgetClient {
        WidgetClient {
            clid: 1,
            nickname: nickname.into(),
            is_away: false,
            is_muted: false,
        }
    }

    fn fixture() -> WidgetData {
        let lobby = channel(1, "Lobby", true, vec![]);
        let voice = channel(2, "Voice", false, vec![client("Alice"), client("Bob")]);
        let sp = spacer("Hello", SpacerType::Center);
        WidgetData {
            name: "Test Widget".into(),
            theme: "dark".into(),
            server_config_id: 1,
            show_channel_tree: true,
            show_clients: true,
            hide_empty_channels: false,
            max_channel_depth: 5,
            server: server(),
            channels: vec![lobby, sp, voice],
        }
    }

    #[test]
    fn renders_full_svg_envelope() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(svg.starts_with("<svg "));
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains("width=\"400\""));
        assert!(svg.ends_with("</svg>"));
    }

    #[test]
    fn header_carries_server_name_and_online_badge() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(svg.contains(">Test Server</text>"));
        assert!(svg.contains(">ONLINE</text>"));
        assert!(svg.contains("4/32 users"));
        assert!(svg.contains("1d 1h uptime"));
    }

    #[test]
    fn password_channel_emits_lock_emoji() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        // 🔒 is U+1F512 — survives as the literal char in the SVG body.
        assert!(
            svg.contains("🔒"),
            "expected lock emoji on password channel"
        );
    }

    #[test]
    fn named_channels_render() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(svg.contains(">Lobby</text>"));
        assert!(svg.contains(">Voice</text>"));
    }

    #[test]
    fn center_spacer_emits_middle_anchor_text() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(
            svg.contains("text-anchor=\"middle\"") && svg.contains(">Hello</text>"),
            "expected centered spacer text"
        );
    }

    #[test]
    fn line_spacer_emits_dasharray() {
        let mut data = fixture();
        data.channels = vec![spacer("---", SpacerType::Dashline)];
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(svg.contains("stroke-dasharray=\"6,4\""));
    }

    #[test]
    fn dotline_spacer_emits_dasharray() {
        let mut data = fixture();
        data.channels = vec![spacer("...", SpacerType::Dotline)];
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(svg.contains("stroke-dasharray=\"2,4\""));
    }

    #[test]
    fn solid_line_spacer_has_no_dasharray() {
        let mut data = fixture();
        data.channels = vec![spacer("===", SpacerType::Line)];
        let svg = render(&data, &WIDGET_THEME_DARK);
        // The header separator emits a `<line>` with no dash; assert there
        // is at least one body line that also lacks a dash by checking we
        // don't stamp the dotline/dashline dasharrays.
        assert!(!svg.contains("stroke-dasharray=\"2,4\""));
        assert!(!svg.contains("stroke-dasharray=\"6,4\""));
    }

    #[test]
    fn client_row_emits_circle_and_nickname() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(svg.contains(">Alice</text>"));
        assert!(svg.contains(">Bob</text>"));
        assert!(svg.contains("<circle "));
    }

    #[test]
    fn away_and_muted_render_as_suffix_tspans() {
        let mut data = fixture();
        data.channels = vec![channel(
            10,
            "Lobby",
            false,
            vec![WidgetClient {
                clid: 1,
                nickname: "Alice".into(),
                is_away: true,
                is_muted: true,
            }],
        )];
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(
            svg.contains("[away]") && svg.contains("[muted]"),
            "expected away+muted suffixes in the rendered SVG"
        );
        assert!(svg.contains("</tspan>"));
    }

    #[test]
    fn theme_switch_changes_fill_colour_only() {
        let dark = render(&fixture(), &WIDGET_THEME_DARK);
        let neon = render(&fixture(), &WIDGET_THEME_NEON);
        assert!(dark.contains(WIDGET_THEME_DARK.accent));
        assert!(neon.contains(WIDGET_THEME_NEON.accent));
        assert_ne!(dark, neon, "theme switch must alter the rendered SVG");
    }

    #[test]
    fn xml_escape_handles_metacharacters() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn server_name_is_xml_escaped() {
        let mut data = fixture();
        data.server.name = "<script>alert(1)</script>".into();
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(svg.contains("&lt;script&gt;"));
        assert!(!svg.contains("<script>"));
    }

    #[test]
    fn channel_name_is_xml_escaped() {
        let mut data = fixture();
        data.channels = vec![channel(1, "<bad>", false, vec![])];
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(svg.contains("&lt;bad&gt;"));
        assert!(
            !svg.contains("<bad>"),
            "raw `<bad>` must not survive into the SVG output"
        );
    }

    #[test]
    fn nickname_is_xml_escaped() {
        let mut data = fixture();
        data.channels = vec![channel(
            1,
            "Lobby",
            false,
            vec![WidgetClient {
                clid: 1,
                nickname: "<img>".into(),
                is_away: false,
                is_muted: false,
            }],
        )];
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(svg.contains("&lt;img&gt;"));
    }

    #[test]
    fn truncation_appends_ellipsis() {
        // 5-char budget; 10-char input. truncate_chars should yield 4
        // chars + "…".
        let s = truncate_chars("abcdefghij", 5);
        assert_eq!(s.chars().count(), 5);
        assert!(s.ends_with('…'));
        assert!(s.starts_with("abcd"));
    }

    #[test]
    fn truncation_passes_through_short_inputs() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcde", 5), "abcde");
    }

    #[test]
    fn footer_caption_present() {
        let svg = render(&fixture(), &WIDGET_THEME_DARK);
        assert!(svg.contains(">TS6 WebUI Widget</text>"));
        assert!(svg.contains("opacity=\"0.6\""));
    }

    #[test]
    fn show_clients_false_omits_circle_for_clients() {
        let mut data = fixture();
        data.show_clients = false;
        // Drain client lists at the data layer (Slice A would do this; in
        // this test the SVG layer also walks the rows respecting the flag).
        for ch in data.channels.iter_mut() {
            ch.clients.clear();
        }
        let svg = render(&data, &WIDGET_THEME_DARK);
        assert!(!svg.contains("<circle "));
    }

    #[test]
    fn theme_for_used_indirectly_by_renderer_is_pure_data() {
        // Sanity: theme_for + render is composable without state.
        let svg = render(
            &fixture(),
            theme_for(ts6_manager_shared::widgets::WidgetThemeName::Light),
        );
        assert!(svg.contains("<svg "));
    }

    #[test]
    fn dynamic_height_grows_with_row_count() {
        let mut small = fixture();
        small.channels = vec![channel(1, "Lobby", false, vec![])];
        let small_svg = render(&small, &WIDGET_THEME_DARK);

        let mut large = fixture();
        large.channels = (0..10)
            .map(|i| {
                channel(
                    i + 1,
                    &format!("Room {i}"),
                    false,
                    vec![client("X"), client("Y")],
                )
            })
            .collect();
        let large_svg = render(&large, &WIDGET_THEME_DARK);

        let small_h = extract_height(&small_svg);
        let large_h = extract_height(&large_svg);
        assert!(
            large_h > small_h,
            "expected larger fixture to produce a taller SVG (small={small_h}, large={large_h})"
        );
    }

    fn extract_height(svg: &str) -> u32 {
        let key = "height=\"";
        let idx = svg.find(key).expect("height attr present");
        let rest = &svg[idx + key.len()..];
        let end = rest.find('"').expect("closing quote");
        rest[..end].parse().expect("u32 height")
    }
}
