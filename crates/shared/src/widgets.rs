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

// =============================================================================
// Operator widget CRUD wire shapes (spec §7.27, §34) — used by the admin
// `/api/widgets` surface and consumed by the Widget Manager UI (PURA-92).
// =============================================================================

/// `POST /api/widgets` body. `virtualServerId` is required at the wire boundary
/// (the repo enforces a default but the create form on the FE always sends it).
/// Optional fields fall through to the per-column DEFAULTs in migration
/// `0004_chapter4_remaining_entities.surql`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWidgetRequest {
    pub name: String,
    #[serde(rename = "serverConfigId")]
    pub server_config_id: i64,
    #[serde(rename = "virtualServerId")]
    pub virtual_server_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(
        rename = "showChannelTree",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub show_channel_tree: Option<bool>,
    #[serde(
        rename = "showClients",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub show_clients: Option<bool>,
    #[serde(
        rename = "hideEmptyChannels",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hide_empty_channels: Option<bool>,
    #[serde(
        rename = "maxChannelDepth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_channel_depth: Option<i64>,
}

/// `PATCH /api/widgets/{id}` body. Every field is `Option<_>` so the caller
/// sends only what they want to change. `serverConfigId` / `virtualServerId`
/// are deliberately *not* patchable (a widget that points at a different
/// server is a new widget — recreate it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateWidgetRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(
        rename = "showChannelTree",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub show_channel_tree: Option<bool>,
    #[serde(
        rename = "showClients",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub show_clients: Option<bool>,
    #[serde(
        rename = "hideEmptyChannels",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hide_empty_channels: Option<bool>,
    #[serde(
        rename = "maxChannelDepth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_channel_depth: Option<i64>,
}

/// Three public-route URLs an operator can copy-paste into a third-party
/// embed. Paths only — the FE prepends the host. Spec §34: the Widget Manager
/// page surfaces the JSON / SVG / PNG embeds plus the public HTML page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetEmbedUrls {
    /// `/api/widget/{token}/data` — the JSON payload the SPA renders client-side.
    #[serde(rename = "dataUrl")]
    pub data_url: String,
    /// `/api/widget/{token}/image.svg` — server-rendered SVG (Slice B).
    #[serde(rename = "svgUrl")]
    pub svg_url: String,
    /// `/api/widget/{token}/image.png` — `resvg`-rasterised PNG (Slice C).
    #[serde(rename = "pngUrl")]
    pub png_url: String,
    /// `/widget/{token}` — public HTML page (Slice E) suitable for `<iframe>` embeds.
    #[serde(rename = "pageUrl")]
    pub page_url: String,
}

impl WidgetEmbedUrls {
    /// Build the four canonical URLs from a widget token. Path-relative; the
    /// FE prepends the public origin.
    pub fn for_token(token: &str) -> Self {
        Self {
            data_url: format!("/api/widget/{token}/data"),
            svg_url: format!("/api/widget/{token}/image.svg"),
            png_url: format!("/api/widget/{token}/image.png"),
            page_url: format!("/widget/{token}"),
        }
    }
}

/// Operator-side widget row. Mirrors the on-disk `widget` table verbatim and
/// adds `serverName` / `serverHost` from the join requested by spec §7.27
/// ("List widgets with their server config") plus the embed URL bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetSummary {
    pub id: i64,
    pub name: String,
    pub token: String,
    #[serde(rename = "serverConfigId")]
    pub server_config_id: i64,
    #[serde(rename = "virtualServerId")]
    pub virtual_server_id: i64,
    pub theme: String,
    #[serde(rename = "showChannelTree")]
    pub show_channel_tree: bool,
    #[serde(rename = "showClients")]
    pub show_clients: bool,
    #[serde(rename = "hideEmptyChannels")]
    pub hide_empty_channels: bool,
    #[serde(rename = "maxChannelDepth")]
    pub max_channel_depth: i64,
    /// Joined from `server_connection.name`. `None` when the underlying
    /// server row was deleted; the operator UI surfaces that as a stale row.
    #[serde(rename = "serverName", skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Joined from `server_connection.host`. Same `None` rule as `serverName`.
    #[serde(rename = "serverHost", skip_serializing_if = "Option::is_none")]
    pub server_host: Option<String>,
    #[serde(rename = "embedUrls")]
    pub embed_urls: WidgetEmbedUrls,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

// =============================================================================
// Public widget JSON wire shape (spec §27.1) — consumed by `/api/widget/{token}/data`.
// =============================================================================

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

    /// Resolve a name to its eight-slot palette. Values are spec'd verbatim
    /// in `study-documents/design-system/widget-themes.md` (spec §3.7 /
    /// §26.3) — implementations MUST NOT alter them when reproducing the
    /// same visual identity.
    ///
    /// Lives on the shared crate (not in the server-only `widgets::themes`
    /// module) so the `wasm32` build of the `/widget/:token` SPA page
    /// (Slice E) can inline the palette without dragging the server-feature
    /// crate into the WASM bundle.
    pub fn palette(self) -> WidgetThemePalette {
        match self {
            WidgetThemeName::Dark => WidgetThemePalette::DARK,
            WidgetThemeName::Light => WidgetThemePalette::LIGHT,
            WidgetThemeName::Transparent => WidgetThemePalette::TRANSPARENT,
            WidgetThemeName::Neon => WidgetThemePalette::NEON,
            WidgetThemeName::Military => WidgetThemePalette::MILITARY,
            WidgetThemeName::Minimal => WidgetThemePalette::MINIMAL,
        }
    }
}

/// Eight-slot theme palette. Slot semantics live in
/// `study-documents/design-system/widget-themes.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidgetThemePalette {
    pub name: WidgetThemeName,
    pub background: &'static str,
    pub background_secondary: &'static str,
    pub border: &'static str,
    pub text_primary: &'static str,
    pub text_secondary: &'static str,
    pub accent: &'static str,
    pub client_color: &'static str,
    pub header_bg: &'static str,
}

impl WidgetThemePalette {
    pub const DARK: Self = Self {
        name: WidgetThemeName::Dark,
        background: "#0F1218",
        background_secondary: "#161B25",
        border: "#283040",
        text_primary: "#E8ECF4",
        text_secondary: "#7A8398",
        accent: "#3CBEEF",
        client_color: "#5DD299",
        header_bg: "#161B25",
    };

    pub const LIGHT: Self = Self {
        name: WidgetThemeName::Light,
        background: "#FAFBFE",
        background_secondary: "#F0F3F8",
        border: "#D7DCE6",
        text_primary: "#1A1F2C",
        text_secondary: "#5C6477",
        accent: "#1A8BC8",
        client_color: "#2EB872",
        header_bg: "#EFF2F8",
    };

    pub const TRANSPARENT: Self = Self {
        name: WidgetThemeName::Transparent,
        background: "rgba(0,0,0,0)",
        background_secondary: "rgba(0,0,0,0.04)",
        border: "rgba(255,255,255,0.18)",
        text_primary: "#FFFFFF",
        text_secondary: "rgba(255,255,255,0.7)",
        accent: "#7AD8FF",
        client_color: "#5DD299",
        header_bg: "rgba(0,0,0,0.40)",
    };

    pub const NEON: Self = Self {
        name: WidgetThemeName::Neon,
        background: "#06091F",
        background_secondary: "#0E1338",
        border: "#3A2A6E",
        text_primary: "#F0E8FF",
        text_secondary: "#A99CD8",
        accent: "#FF49C4",
        client_color: "#22F0AA",
        header_bg: "#1A0E3F",
    };

    pub const MILITARY: Self = Self {
        name: WidgetThemeName::Military,
        background: "#1A1E16",
        background_secondary: "#22281D",
        border: "#3A4031",
        text_primary: "#D4D9C8",
        text_secondary: "#8A9078",
        accent: "#A4B86F",
        client_color: "#C8D294",
        header_bg: "#22281D",
    };

    pub const MINIMAL: Self = Self {
        name: WidgetThemeName::Minimal,
        background: "#FFFFFF",
        background_secondary: "#F7F8FA",
        border: "#E2E5EC",
        text_primary: "#2A2F3A",
        text_secondary: "#7C8294",
        accent: "#5A6173",
        client_color: "#5C6477",
        header_bg: "#F0F2F5",
    };
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
///
/// `serverConfigId` is a panel-specific addition (not in spec §27.1): the
/// public `/widget/:token` SPA page (Slice E) needs the underlying
/// `server_connection.id` to subscribe to `server:{id}:widget` over the
/// WS hub. Surfacing it on the JSON snapshot saves the SPA a second round
/// trip and is unguessable without the token already in the URL.
//
// `PartialEq` is derived alongside the wire types so the SPA can pass these
// through Dioxus `Props`-derived components, which require `PartialEq` for
// memoisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetData {
    /// Operator-supplied widget name (`Widget.name`).
    pub name: String,
    /// One of the six theme names. Stored lowercase.
    pub theme: String,
    /// `server_connection.id` the widget points at. The SPA derives the WS
    /// topic name `server:{serverConfigId}:widget` from this.
    #[serde(rename = "serverConfigId")]
    pub server_config_id: i64,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetClient {
    pub clid: i64,
    pub nickname: String,
    #[serde(rename = "isAway")]
    pub is_away: bool,
    #[serde(rename = "isMuted")]
    pub is_muted: bool,
}
