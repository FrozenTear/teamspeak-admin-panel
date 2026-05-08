//! Six-theme palette registry — values verbatim from
//! `study-documents/design-system/widget-themes.md` (spec §3.7 / §26.3).
//!
//! Names MUST NOT be renamed or removed; values are invented (cleanroom).
//! The SVG renderer (Slice B) and the embed page (Slice E) consume
//! [`WidgetTheme`] references; the JSON wire payload only round-trips the
//! theme **name** so client-side renderers can re-read the palette from a
//! shared registry without bloating every response.

use ts6_manager_shared::widgets::WidgetThemeName;

/// Eight-slot palette consumed by the SVG renderer and the embed page.
///
/// Slot semantics live in the design-system doc and the §27.3 SVG layout
/// table.
#[derive(Debug, Clone, Copy)]
pub struct WidgetTheme {
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

pub const WIDGET_THEME_DARK: WidgetTheme = WidgetTheme {
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

pub const WIDGET_THEME_LIGHT: WidgetTheme = WidgetTheme {
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

pub const WIDGET_THEME_TRANSPARENT: WidgetTheme = WidgetTheme {
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

pub const WIDGET_THEME_NEON: WidgetTheme = WidgetTheme {
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

pub const WIDGET_THEME_MILITARY: WidgetTheme = WidgetTheme {
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

pub const WIDGET_THEME_MINIMAL: WidgetTheme = WidgetTheme {
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

/// Resolve a theme name to its palette. Unknown names fall back to
/// [`WIDGET_THEME_DARK`] — the same default the spec uses for `Widget.theme`.
pub fn theme_for(name: WidgetThemeName) -> &'static WidgetTheme {
    match name {
        WidgetThemeName::Dark => &WIDGET_THEME_DARK,
        WidgetThemeName::Light => &WIDGET_THEME_LIGHT,
        WidgetThemeName::Transparent => &WIDGET_THEME_TRANSPARENT,
        WidgetThemeName::Neon => &WIDGET_THEME_NEON,
        WidgetThemeName::Military => &WIDGET_THEME_MILITARY,
        WidgetThemeName::Minimal => &WIDGET_THEME_MINIMAL,
    }
}

/// All six themes in spec order. Useful for the operator picker thumbnail
/// strip (Slice G).
pub const ALL_THEMES: [&WidgetTheme; 6] = [
    &WIDGET_THEME_DARK,
    &WIDGET_THEME_LIGHT,
    &WIDGET_THEME_TRANSPARENT,
    &WIDGET_THEME_NEON,
    &WIDGET_THEME_MILITARY,
    &WIDGET_THEME_MINIMAL,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_six_names() {
        for &theme in ALL_THEMES.iter() {
            let parsed = WidgetThemeName::parse_or_default(theme.name.as_str());
            assert_eq!(
                parsed.as_str(),
                theme.name.as_str(),
                "name round-trip failed for {}",
                theme.name.as_str()
            );
            assert_eq!(theme_for(parsed).name.as_str(), theme.name.as_str());
        }
    }

    #[test]
    fn unknown_falls_back_to_dark() {
        assert!(matches!(
            WidgetThemeName::parse_or_default("auto"),
            WidgetThemeName::Dark
        ));
        assert!(matches!(
            WidgetThemeName::parse_or_default(""),
            WidgetThemeName::Dark
        ));
    }
}
