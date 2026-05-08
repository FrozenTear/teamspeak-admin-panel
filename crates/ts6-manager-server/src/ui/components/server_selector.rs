//! Server selector — `components.md` §13.11 + `server-selector.md`.
//!
//! Composes the Dropdown primitive with single-select radio items, the
//! optional filter (visible only at >7 servers per §13.11), and a
//! "Manage servers…" footer that routes to `/servers`. Stub data lives in
//! [`stub_servers`] until the REST/Realtime engineer ships
//! `GET /api/servers`; the consuming component ([`ServerSelector`]) does
//! not care whether the source is stub or live, so swapping it out is a
//! one-line change.
//!
//! Two visual variants exist because the chrome surfaces the selector in
//! two slots (header pill on desktop, full-width bar above the page on
//! mobile, per `components.md` §11.3 / `server-selector.md` §2.2). The
//! variants render distinct DOM ids so both instances can mount in the
//! same document without ARIA-id collisions; `display: none` keeps only
//! one visible at any breakpoint.

use dioxus::prelude::*;

use crate::ui::components::dropdown::{
    Dropdown, Menu, MenuDivider, MenuEmpty, MenuFilter, MenuFooter, MenuItem, MenuItemKind,
    MenuSection,
};

/// Filter is hidden until the list grows past this many entries. Spec
/// §13.4 ("Filter input (when used)") and §13.11 ("MenuFilter shown only
/// when servers.len() > 7").
pub const FILTER_VISIBLE_THRESHOLD: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerStatus {
    Connected,
    Reconnecting,
    Offline,
}

impl ServerStatus {
    pub const fn meta_label(self) -> &'static str {
        match self {
            ServerStatus::Connected => "connected",
            ServerStatus::Reconnecting => "reconnecting",
            ServerStatus::Offline => "offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSummary {
    pub id: String,
    pub name: String,
    pub status: ServerStatus,
}

/// Phase 1 stub list. Three entries — enough to exercise the open / radio /
/// status-meta states without unlocking the >7-entry filter, which has its
/// own unit test below.
pub fn stub_servers() -> Vec<ServerSummary> {
    vec![
        ServerSummary {
            id: "demo-1".into(),
            name: "My Community".into(),
            status: ServerStatus::Connected,
        },
        ServerSummary {
            id: "demo-2".into(),
            name: "Backup node".into(),
            status: ServerStatus::Connected,
        },
        ServerSummary {
            id: "demo-3".into(),
            name: "Tournament server".into(),
            status: ServerStatus::Reconnecting,
        },
    ]
}

/// Case-insensitive substring filter. Pulled out as a free function so the
/// filter rule is unit-testable without spinning a `VirtualDom`.
pub fn filter_servers(servers: &[ServerSummary], needle: &str) -> Vec<ServerSummary> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return servers.to_vec();
    }
    servers
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ServerSelectorVariant {
    #[default]
    Desktop,
    Mobile,
}

impl ServerSelectorVariant {
    /// DOM-id prefix per variant. Two `ServerSelector`s mount in the same
    /// document (one in the header, one in the mobile-only bar); distinct
    /// prefixes prevent the trigger / menu / item ids from colliding.
    const fn id_prefix(self) -> &'static str {
        match self {
            ServerSelectorVariant::Desktop => "server-selector-desktop",
            ServerSelectorVariant::Mobile => "server-selector-mobile",
        }
    }

    const fn extra_anchor_class(self) -> &'static str {
        match self {
            ServerSelectorVariant::Desktop => "desktop-selector",
            ServerSelectorVariant::Mobile => "mobile-selector",
        }
    }
}

/// Header server-selector. Stub-data backed for Phase 1; switching the
/// source to the live `GET /api/servers` feed is a Wave-2 follow-up.
#[allow(non_snake_case)]
#[component]
pub fn ServerSelector(
    #[props(default = ServerSelectorVariant::Desktop)] variant: ServerSelectorVariant,
) -> Element {
    let servers = use_signal(stub_servers);
    let mut open = use_signal(|| false);
    let mut active_id = use_signal(|| None::<String>);
    let mut filter = use_signal(String::new);
    let selected = use_signal(|| None::<String>);

    let prefix = variant.id_prefix();
    let trigger_id = format!("{prefix}-trigger");
    let menu_id = format!("{prefix}-menu");

    // Reset the filter and active cursor whenever the menu closes so the
    // next open starts clean — matches the "filter autofocuses on open"
    // contract in §13.4.
    use_effect(move || {
        if !*open.read() {
            filter.set(String::new());
            active_id.set(None);
        }
    });

    let servers_snap = servers.read().clone();
    let filter_text = filter.read().clone();
    let filtered = filter_servers(&servers_snap, &filter_text);
    let show_filter = servers_snap.len() > FILTER_VISIBLE_THRESHOLD;
    let selected_id = selected.read().clone();
    let selected_name = selected_id
        .as_ref()
        .and_then(|id| {
            servers_snap
                .iter()
                .find(|s| &s.id == id)
                .map(|s| s.name.clone())
        })
        .unwrap_or_else(|| "Select a server".to_string());

    let aria_expanded = if *open.read() { "true" } else { "false" };
    let trigger_id_for_attr = trigger_id.clone();
    let menu_id_for_attr = menu_id.clone();

    let toggle_open = move |_| {
        let next = !*open.read();
        if next {
            // Initialise active_id to the selected server (if any), else
            // the first item — matches the spec's "selected = active on
            // open" expectation so screen readers don't see an unfocused
            // menu on first arrow press.
            let initial = selected
                .read()
                .clone()
                .or_else(|| servers.read().first().map(|s| s.id.clone()));
            active_id.set(initial);
        }
        open.set(next);
    };

    let anchor_class = format!("server-selector-anchor {}", variant.extra_anchor_class());

    rsx! {
        div { class: "{anchor_class}",
            Dropdown {
                trigger_id: trigger_id.clone(),
                menu_id: menu_id.clone(),
                open: open,
                active_id: active_id,
                trigger: rsx! {
                    button {
                        class: "selector",
                        r#type: "button",
                        id: "{trigger_id_for_attr}",
                        "aria-haspopup": "menu",
                        "aria-expanded": "{aria_expanded}",
                        "aria-controls": "{menu_id_for_attr}",
                        "aria-label": "Switch server",
                        onclick: toggle_open,
                        span { class: "mark", "⬢" }
                        span { class: "label", "{selected_name}" }
                        span { class: "chev", "▾" }
                    }
                },

                Menu {
                    id: menu_id.clone(),
                    labelled_by: trigger_id.clone(),
                    active_id: active_id.read().clone(),

                    if show_filter {
                        MenuFilter {
                            value: filter,
                            placeholder: "Filter servers…".to_string(),
                            aria_label: "Filter servers".to_string(),
                        }
                    }

                    if filtered.is_empty() {
                        if filter_text.trim().is_empty() {
                            MenuEmpty { text: "No servers available.".to_string() }
                        } else {
                            MenuEmpty { text: format!("No servers match \"{}\".", filter_text.trim()) }
                        }
                    } else {
                        MenuSection { label: "Servers".to_string(),
                            for s in filtered.iter() {
                                ServerSelectorItem {
                                    key: "{s.id}",
                                    summary: s.clone(),
                                    prefix: prefix.to_string(),
                                    checked: selected_id.as_deref() == Some(s.id.as_str()),
                                    onselect: {
                                        let server_id = s.id.clone();
                                        let mut selected_for_pick = selected;
                                        move |()| selected_for_pick.set(Some(server_id.clone()))
                                    },
                                }
                            }
                        }
                    }

                    MenuDivider {}
                    MenuFooter {
                        a {
                            class: "btn btn-secondary btn-sm",
                            href: "/servers",
                            "Manage servers…"
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServerSelectorItemProps {
    summary: ServerSummary,
    prefix: String,
    checked: bool,
    onselect: EventHandler<()>,
}

#[allow(non_snake_case)]
#[component]
fn ServerSelectorItem(props: ServerSelectorItemProps) -> Element {
    let id = format!("{}-item-{}", props.prefix, props.summary.id);
    let summary = props.summary.clone();
    let checked = props.checked;
    let check_glyph = if checked { "✓" } else { "" };
    rsx! {
        MenuItem {
            id: id,
            kind: MenuItemKind::Radio { checked },
            rich: true,
            onselect: props.onselect,
            span { class: "check", "{check_glyph}" }
            span { class: "label", "{summary.name}" }
            span { class: "meta", "{summary.status.meta_label()}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_visible_threshold_is_seven() {
        // Pin §13.11's literal "> 7" so a future tweak to the threshold
        // forces a docs/code review of the rationale (Hick's law applied
        // to short menus).
        assert_eq!(FILTER_VISIBLE_THRESHOLD, 7);
    }

    #[test]
    fn stub_servers_below_filter_threshold() {
        // The stub list is small enough that the filter input stays hidden
        // — important so visual snapshots match the "no filter" state of
        // the chrome until the live API ships.
        assert!(stub_servers().len() <= FILTER_VISIBLE_THRESHOLD);
    }

    #[test]
    fn filter_servers_matches_substring_case_insensitively() {
        let list = stub_servers();
        let hits = filter_servers(&list, "tour");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Tournament server");
    }

    #[test]
    fn filter_servers_empty_needle_returns_all() {
        let list = stub_servers();
        let hits = filter_servers(&list, "   ");
        assert_eq!(hits.len(), list.len());
    }

    #[test]
    fn filter_servers_no_match_returns_empty() {
        let list = stub_servers();
        let hits = filter_servers(&list, "staging-007");
        assert!(hits.is_empty());
    }

    #[test]
    fn server_status_meta_labels_match_preview() {
        // Pin label strings — they're rendered into `.meta` and must match
        // the visual-truth gallery's text exactly.
        assert_eq!(ServerStatus::Connected.meta_label(), "connected");
        assert_eq!(ServerStatus::Reconnecting.meta_label(), "reconnecting");
        assert_eq!(ServerStatus::Offline.meta_label(), "offline");
    }

    #[test]
    fn variants_emit_distinct_id_prefixes() {
        assert_ne!(
            ServerSelectorVariant::Desktop.id_prefix(),
            ServerSelectorVariant::Mobile.id_prefix(),
            "two ServerSelectors mount in the same document; their DOM ids must not collide"
        );
    }

    // ── SSR markup contract ────────────────────────────────────────────
    //
    // Renders the closed-state ServerSelector inside a synthetic Router
    // (for `Link`/`use_route` consumers downstream — none today, but the
    // dropdown's children may compose ordinary Dioxus router primitives in
    // the future). Snapshots the trigger ARIA attributes so a regression
    // breaks loudly.

    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum SelectorTestRoute {
        #[route("/")]
        SelectorHarness {},
    }

    #[component]
    fn SelectorHarness() -> Element {
        rsx! { ServerSelector { variant: ServerSelectorVariant::Desktop } }
    }

    fn render_selector_harness() -> String {
        let mut dom = VirtualDom::new(|| {
            rsx! { Router::<SelectorTestRoute> {} }
        });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn trigger_carries_full_aria_haspopup_contract() {
        let html = render_selector_harness();
        assert!(
            html.contains(r#"aria-haspopup="menu""#),
            "trigger missing aria-haspopup='menu': {html}"
        );
        assert!(
            html.contains(r#"aria-expanded="false""#),
            "closed selector should expose aria-expanded='false': {html}"
        );
        assert!(
            html.contains(r#"aria-controls="server-selector-desktop-menu""#),
            "trigger must aria-control its own menu id: {html}"
        );
        assert!(
            html.contains(r#"aria-label="Switch server""#),
            "trigger missing aria-label='Switch server': {html}"
        );
    }

    #[test]
    fn trigger_id_matches_aria_controls_target() {
        let html = render_selector_harness();
        assert!(
            html.contains(r#"id="server-selector-desktop-trigger""#),
            "trigger id must match the menu's aria-labelledby reference: {html}"
        );
    }
}
