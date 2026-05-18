//! `Tabs` + `TabPanel` — underline tab bar. `components.md` §19.
//!
//! Built for the moderation group-detail Permissions / Members / Settings
//! split (PURA-369 Phase A) but deliberately generic — `Tabs` takes an
//! opaque `Vec<TabItem>` and reports the selected id, so any ≤~6-tab detail
//! surface can reuse it.
//!
//! Activation is automatic (WAI-ARIA "tabs with automatic activation"):
//! moving the arrow-key cursor selects the tab it lands on. That suits a
//! detail page where every panel is cheap to mount; a future data-heavy
//! surface that needs manual activation would be a separate prop, not a
//! rewrite.
//!
//! Roving tabindex: the selected tab is `tabindex="0"`, the rest `-1`, so
//! Tab into the bar lands on the active tab and arrow keys move within it.

use dioxus::prelude::*;

/// One tab descriptor. `id` is the host's stable key (compared against the
/// `Tabs` `active` prop and the `TabPanel` `id`); `label` is the visible text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TabItem {
    pub id: String,
    pub label: String,
}

impl TabItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum TabStep {
    First,
    Last,
    Next,
    Prev,
}

/// Compute the tab id an arrow-key press should move to. Wraps at both ends;
/// returns `None` only when `tabs` is empty.
fn step_tab(tabs: &[TabItem], current: &str, step: TabStep) -> Option<String> {
    if tabs.is_empty() {
        return None;
    }
    let len = tabs.len();
    let cur = tabs.iter().position(|t| t.id == current);
    let idx = match step {
        TabStep::First => 0,
        TabStep::Last => len - 1,
        TabStep::Next => match cur {
            None => 0,
            Some(i) => (i + 1) % len,
        },
        TabStep::Prev => match cur {
            None => len - 1,
            Some(i) => (i + len - 1) % len,
        },
    };
    Some(tabs[idx].id.clone())
}

fn tab_id(stem: &str, id: &str) -> String {
    format!("{stem}-tab-{id}")
}

fn panel_id(stem: &str, id: &str) -> String {
    format!("{stem}-panel-{id}")
}

#[cfg(target_arch = "wasm32")]
fn focus_element(id: &str) {
    use wasm_bindgen::JsCast;
    if let Some(elem) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Some(html) = elem.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// `role="tablist"` bar. Spec contract is `components.md` §19.
///
/// `active` is host-owned; `Tabs` reports the requested selection through
/// `onselect` and never mutates it. Pair with one `TabPanel` per tab,
/// passing the same `id` stem so the `aria-controls` / `aria-labelledby`
/// wiring lines up.
#[component]
pub fn Tabs(
    /// Tab descriptors, in display order.
    tabs: Vec<TabItem>,
    /// Currently-selected tab id. Host-owned.
    active: String,
    /// Id stem shared with the paired `TabPanel`s. Tab buttons get
    /// `{stem}-tab-{id}`, panels `{stem}-panel-{id}`. Defaults to `"tabs"` —
    /// set it explicitly when more than one tab bar lives on a page.
    #[props(default = String::from("tabs"))]
    id: String,
    /// Optional `aria-label` for the tablist — use when no visible heading
    /// already names the group of tabs.
    #[props(default)]
    aria_label: Option<String>,
    /// Fired with the requested tab id on click or arrow-key move.
    onselect: EventHandler<String>,
) -> Element {
    let stem = id;

    rsx! {
        div {
            class: "tabs",
            role: "tablist",
            "aria-label": aria_label,
            for tab in tabs.iter().cloned() {
                {
                    let tabs_for_keys = tabs.clone();
                    let active_for_keys = active.clone();
                    let stem_for_keys = stem.clone();
                    let onselect_keys = onselect;
                    let is_active = tab.id == active;
                    let this_id = tab.id.clone();
                    let onselect_click = onselect;
                    rsx! {
                        button {
                            key: "{tab.id}",
                            r#type: "button",
                            class: if is_active { "tab is-active" } else { "tab" },
                            id: tab_id(&stem, &tab.id),
                            role: "tab",
                            "aria-selected": if is_active { "true" } else { "false" },
                            "aria-controls": panel_id(&stem, &tab.id),
                            tabindex: if is_active { "0" } else { "-1" },
                            onclick: move |_| onselect_click.call(this_id.clone()),
                            onkeydown: move |evt: KeyboardEvent| {
                                let step = match evt.key() {
                                    Key::ArrowRight => TabStep::Next,
                                    Key::ArrowLeft => TabStep::Prev,
                                    Key::Home => TabStep::First,
                                    Key::End => TabStep::Last,
                                    _ => return,
                                };
                                if let Some(next) =
                                    step_tab(&tabs_for_keys, &active_for_keys, step)
                                {
                                    evt.prevent_default();
                                    #[cfg(target_arch = "wasm32")]
                                    focus_element(&tab_id(&stem_for_keys, &next));
                                    #[cfg(not(target_arch = "wasm32"))]
                                    let _ = &stem_for_keys;
                                    onselect_keys.call(next);
                                }
                            },
                            "{tab.label}"
                        }
                    }
                }
            }
        }
    }
}

/// `role="tabpanel"` body paired with one `Tabs` tab.
///
/// The host renders one `TabPanel` per tab and drives `active`. Inactive
/// panels stay in the DOM with the `hidden` attribute so the tab's
/// `aria-controls` target always resolves.
#[component]
pub fn TabPanel(
    /// Tab id this panel belongs to — matches a `TabItem::id`.
    id: String,
    /// Id stem — must match the paired `Tabs` `id`.
    #[props(default = String::from("tabs"))]
    tabs_id: String,
    /// Whether this panel's tab is the selected one.
    active: bool,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: "tab-panel",
            id: panel_id(&tabs_id, &id),
            role: "tabpanel",
            "aria-labelledby": tab_id(&tabs_id, &id),
            tabindex: "0",
            hidden: !active,
            {children}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<TabItem> {
        vec![
            TabItem::new("perms", "Permissions"),
            TabItem::new("members", "Members"),
            TabItem::new("settings", "Settings"),
        ]
    }

    #[test]
    fn step_walks_and_wraps() {
        let t = items();
        assert_eq!(
            step_tab(&t, "perms", TabStep::Next).as_deref(),
            Some("members")
        );
        assert_eq!(
            step_tab(&t, "settings", TabStep::Next).as_deref(),
            Some("perms"),
            "Next from the last tab wraps to the first"
        );
        assert_eq!(
            step_tab(&t, "perms", TabStep::Prev).as_deref(),
            Some("settings"),
            "Prev from the first tab wraps to the last"
        );
        assert_eq!(
            step_tab(&t, "members", TabStep::First).as_deref(),
            Some("perms")
        );
        assert_eq!(
            step_tab(&t, "members", TabStep::Last).as_deref(),
            Some("settings")
        );
    }

    #[test]
    fn step_handles_unknown_current_and_empty() {
        let t = items();
        // An id not in the list falls back to a sane endpoint.
        assert_eq!(
            step_tab(&t, "ghost", TabStep::Next).as_deref(),
            Some("perms")
        );
        assert_eq!(
            step_tab(&t, "ghost", TabStep::Prev).as_deref(),
            Some("settings")
        );
        assert!(step_tab(&[], "perms", TabStep::Next).is_none());
    }

    fn render_bar(active: &str) -> String {
        #[component]
        fn Harness(active: String) -> Element {
            rsx! {
                Tabs {
                    tabs: items(),
                    active,
                    id: "group-detail".to_string(),
                    aria_label: "Group detail sections".to_string(),
                    onselect: move |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                active: active.to_string(),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn tablist_wires_roles_and_selection() {
        let html = render_bar("members");
        assert!(
            html.contains(r#"role="tablist""#),
            "missing role='tablist': {html}"
        );
        assert_eq!(
            html.matches(r#"role="tab""#).count(),
            3,
            "expected 3 role='tab' buttons: {html}"
        );
        assert!(
            html.contains(r#"id="group-detail-tab-members""#),
            "tab id not built from the stem: {html}"
        );
        assert!(
            html.contains(r#"aria-controls="group-detail-panel-members""#),
            "tab missing aria-controls into its panel: {html}"
        );
    }

    #[test]
    fn active_tab_is_selected_and_focusable() {
        let html = render_bar("settings");
        // The active tab carries aria-selected='true' + tabindex='0';
        // the others must be '-1' so Tab lands only on the active one.
        assert!(
            html.contains(r#"aria-selected="true""#),
            "active tab missing aria-selected='true': {html}"
        );
        assert_eq!(
            html.matches(r#"aria-selected="false""#).count(),
            2,
            "inactive tabs must report aria-selected='false': {html}"
        );
        assert_eq!(
            html.matches(r#"tabindex="-1""#).count(),
            2,
            "roving tabindex: exactly 2 inactive tabs at -1: {html}"
        );
    }

    #[test]
    fn panel_wires_back_to_its_tab() {
        #[component]
        fn PanelHarness() -> Element {
            rsx! {
                TabPanel {
                    id: "perms".to_string(),
                    tabs_id: "group-detail".to_string(),
                    active: false,
                    "permission rows"
                }
            }
        }
        let mut dom = VirtualDom::new(PanelHarness);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains(r#"role="tabpanel""#),
            "panel missing role='tabpanel': {html}"
        );
        assert!(
            html.contains(r#"id="group-detail-panel-perms""#),
            "panel id not built from the stem: {html}"
        );
        assert!(
            html.contains(r#"aria-labelledby="group-detail-tab-perms""#),
            "panel missing aria-labelledby back to its tab: {html}"
        );
        assert!(
            html.contains("hidden"),
            "inactive panel must carry the hidden attribute: {html}"
        );
    }
}
