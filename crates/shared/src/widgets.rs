//! Public-widget wire format (spec §26 / §27 / §7.28).
//!
//! `WidgetData` is the JSON payload returned by `GET /api/widget/:token/data`
//! and consumed by the Dioxus public-widget page (`/widget/:token`) for
//! client-side rendering. Field names mirror the spec verbatim — the public
//! contract third-party embedders rely on.
//!
//! Server-side `resvg`-based PNG and `image.svg` rasterisation use the same
//! `WidgetData` snapshot so the three formats stay consistent.

use serde::{Deserialize, Serialize};

/// One of the six built-in theme palettes (spec §3.7 / §26.3). Implementations
/// MAY append themes; MUST NOT rename or remove these six.
///
/// Wire form is the lower-cased variant name (`dark`, `light`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WidgetThemeName {
    Dark,
    Light,
    Transparent,
    Neon,
    Military,
    Minimal,
}

impl WidgetThemeName {
    pub fn as_str(self) -> &'static str {
        match self {
            WidgetThemeName::Dark => "dark",
            WidgetThemeName::Light => "light",
            WidgetThemeName::Transparent => "transparent",
            WidgetThemeName::Neon => "neon",
            WidgetThemeName::Military => "military",
            WidgetThemeName::Minimal => "minimal",
        }
    }

    /// Parse a wire-format theme name. Unknown / unsupported names fall back
    /// to `Dark` (the spec's default) — the route layer decides whether to
    /// surface this as a 4xx or just render the default.
    pub fn parse_or_default(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "dark" => WidgetThemeName::Dark,
            "light" => WidgetThemeName::Light,
            "transparent" => WidgetThemeName::Transparent,
            "neon" => WidgetThemeName::Neon,
            "military" => WidgetThemeName::Military,
            "minimal" => WidgetThemeName::Minimal,
            _ => WidgetThemeName::Dark,
        }
    }
}

/// Spacer kind detected from `[\<prefix\>spacer\<n\>]\<text\>` channel names
/// (spec §27.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpacerType {
    None,
    Line,
    Dotline,
    Dashline,
    Center,
    Left,
    Right,
}

impl SpacerType {
    pub fn as_str(self) -> &'static str {
        match self {
            SpacerType::None => "none",
            SpacerType::Line => "line",
            SpacerType::Dotline => "dotline",
            SpacerType::Dashline => "dashline",
            SpacerType::Center => "center",
            SpacerType::Left => "left",
            SpacerType::Right => "right",
        }
    }
}

/// Top-level public payload. Mirrors the spec §27.1 `WidgetData` shape so
/// the JSON route, the client-side Dioxus page, and the server-side SVG
/// renderer all read from one source of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetData {
    /// Operator-supplied widget name (`Widget.name`).
    pub name: String,
    /// One of the six theme names. Stored lowercase.
    pub theme: String,
    /// Resolved visibility flags (per-widget, from the row).
    #[serde(rename = "showChannelTree")]
    pub show_channel_tree: bool,
    #[serde(rename = "showClients")]
    pub show_clients: bool,
    #[serde(rename = "hideEmptyChannels")]
    pub hide_empty_channels: bool,
    #[serde(rename = "maxChannelDepth")]
    pub max_channel_depth: u32,
    /// Server header line (name, online count, slot total, uptime, redacted
    /// platform/version per spec §7.29).
    pub server: WidgetServer,
    /// Roots of the channel tree. Spacers are interleaved with real channels
    /// in document order. May be empty.
    pub channels: Vec<WidgetChannelNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetServer {
    pub name: String,
    #[serde(rename = "clientsOnline")]
    pub clients_online: u32,
    #[serde(rename = "maxClients")]
    pub max_clients: u32,
    /// Seconds since the virtual server started.
    #[serde(rename = "uptimeSeconds")]
    pub uptime_seconds: u64,
    /// Always the literal `"TeamSpeak"` (spec §7.29 redaction). Kept on the
    /// wire for forward-compat with embedders that key off it.
    pub platform: String,
    /// Always the empty string (spec §7.29 redaction).
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetChannelNode {
    pub cid: i64,
    pub name: String,
    /// True when `channel_flag_password == 1`.
    #[serde(rename = "hasPassword")]
    pub has_password: bool,
    /// Direct human clients in this channel (excludes query clients).
    /// Empty when `showClients=false` is applied at the data-build stage.
    pub clients: Vec<WidgetClient>,
    /// Children below `maxChannelDepth`; cap is hard — over-depth nodes
    /// drop their children entirely (spec §27.1 step 4).
    pub children: Vec<WidgetChannelNode>,
    /// `true` when the channel name matches the spacer regex.
    #[serde(rename = "isSpacer")]
    pub is_spacer: bool,
    /// Always present; `none` when the channel is real.
    #[serde(rename = "spacerType")]
    pub spacer_type: SpacerType,
    /// Spacer text after the prefix (empty for real channels).
    #[serde(rename = "spacerText")]
    pub spacer_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetClient {
    pub clid: i64,
    pub nickname: String,
    #[serde(rename = "isAway")]
    pub is_away: bool,
    #[serde(rename = "isMuted")]
    pub is_muted: bool,
}
