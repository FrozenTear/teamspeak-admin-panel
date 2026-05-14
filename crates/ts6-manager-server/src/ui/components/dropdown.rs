//! Dropdown / Menu primitives — `components.md` §13.7-§13.11.
//!
//! Eight components ship from this module: `Dropdown` (host wraps trigger +
//! menu, owns outside-click / Escape / focus return / keyboard nav), `Menu`
//! (the `role="menu"` container), `MenuItem` (action / radio / checkbox),
//! `MenuSection`, `MenuFilter`, `MenuDivider`, `MenuEmpty`, `MenuFooter`.
//!
//! State ownership matches §13.10: the host owns `open`, `active_id`, the
//! filter value, and the selected/checked state; the primitive owns the
//! outside-click + Escape + focus-return + keyboard handler.
//!
//! Phase 1 simplifications, both called out in §13.9 / §13.11:
//!
//!  - Positioning is CSS-default (flush below); a viewport-aware flip and
//!    floating-ui-style positioner is parked. The `.dropdown-anchor` rule
//!    in `components.css` takes care of below-trigger placement and the
//!    bottom-sheet variant on ≤480px viewports.
//!  - Item metadata for keyboard nav is read straight from the DOM via
//!    `.menu-item` queries inside the menu container — that keeps render
//!    order = nav order without a registration dance, and means the
//!    primitive does not care how the host iterates its data.

use dioxus::prelude::*;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MenuPlacement {
    /// Anchor flush below the trigger (default). The CSS `.dropdown-anchor`
    /// rule places the menu at `top: 100%` with `var(--space-2)` gap.
    #[default]
    Below,
    /// Anchor flush above the trigger. Used by header menus that sit at
    /// the bottom of the viewport in Phase 2.
    Above,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuItemKind {
    /// Plain action — a route link, a "+ Add server" entry, etc.
    Action,
    /// Single-select radio item (server selector, theme picker).
    Radio { checked: bool },
    /// Multi-select checkbox item (column toggles, multi-filter menus).
    Checkbox { checked: bool },
}

impl MenuItemKind {
    pub const fn role(self) -> &'static str {
        match self {
            MenuItemKind::Action => "menuitem",
            MenuItemKind::Radio { .. } => "menuitemradio",
            MenuItemKind::Checkbox { .. } => "menuitemcheckbox",
        }
    }

    /// `aria-checked` value, or `None` when the role does not take it.
    pub const fn aria_checked(self) -> Option<&'static str> {
        match self {
            MenuItemKind::Action => None,
            MenuItemKind::Radio { checked } | MenuItemKind::Checkbox { checked } => {
                Some(if checked { "true" } else { "false" })
            }
        }
    }

    pub const fn is_checked(self) -> bool {
        matches!(
            self,
            MenuItemKind::Radio { checked: true } | MenuItemKind::Checkbox { checked: true }
        )
    }
}

/// State shared from `Dropdown` to its descendants. Not part of the public
/// API — `MenuItem` reads this to know which trigger to return focus to and
/// which `active_id` to compare against; `Menu` reads it to fill in
/// `aria-activedescendant` reactively.
#[derive(Clone)]
struct DropdownContext {
    open: Signal<bool>,
    active_id: Signal<Option<String>>,
    close_on_select: bool,
    trigger_id: String,
    menu_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuItemMeta {
    id: String,
    label: String,
    disabled: bool,
}

#[derive(Clone, Copy, Debug)]
enum NavStep {
    First,
    Last,
    Next,
    Prev,
}

/// Compute the next focused item id given the current focus + a step.
/// Skips `disabled` items; wraps at boundaries.
fn next_active_id(items: &[MenuItemMeta], current: Option<&str>, step: NavStep) -> Option<String> {
    let candidates: Vec<&MenuItemMeta> = items.iter().filter(|m| !m.disabled).collect();
    if candidates.is_empty() {
        return None;
    }
    let cur_idx = current.and_then(|id| candidates.iter().position(|m| m.id == id));
    let len = candidates.len();
    let next_idx = match step {
        NavStep::First => 0,
        NavStep::Last => len - 1,
        NavStep::Next => match cur_idx {
            None => 0,
            Some(i) => (i + 1) % len,
        },
        NavStep::Prev => match cur_idx {
            None => len - 1,
            Some(i) => (i + len - 1) % len,
        },
    };
    Some(candidates[next_idx].id.clone())
}

/// Type-ahead jump: find the first non-disabled item whose label
/// (case-insensitive) starts with `prefix`, searching forward from the
/// position after the current active item (wraps).
fn typeahead_match(items: &[MenuItemMeta], current: Option<&str>, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let needle = prefix.to_lowercase();
    let candidates: Vec<&MenuItemMeta> = items.iter().filter(|m| !m.disabled).collect();
    if candidates.is_empty() {
        return None;
    }
    let start = current
        .and_then(|id| candidates.iter().position(|m| m.id == id))
        .map(|i| (i + 1) % candidates.len())
        .unwrap_or(0);
    for offset in 0..candidates.len() {
        let idx = (start + offset) % candidates.len();
        if candidates[idx].label.to_lowercase().starts_with(&needle) {
            return Some(candidates[idx].id.clone());
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
fn collect_menu_items(menu_id: &str) -> Vec<MenuItemMeta> {
    let Some(window) = web_sys::window() else {
        return Vec::new();
    };
    let Some(document) = window.document() else {
        return Vec::new();
    };
    let Some(menu) = document.get_element_by_id(menu_id) else {
        return Vec::new();
    };
    let Ok(nodes) = menu.query_selector_all(".menu-item") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(nodes.length() as usize);
    for i in 0..nodes.length() {
        let Some(node) = nodes.item(i) else { continue };
        let Some(elem) = node.dyn_ref::<web_sys::Element>() else {
            continue;
        };
        let id = elem.id();
        if id.is_empty() {
            continue;
        }
        let disabled = elem.get_attribute("aria-disabled").as_deref() == Some("true");
        let label = elem
            .query_selector(".label")
            .ok()
            .flatten()
            .and_then(|l| l.text_content())
            .or_else(|| elem.text_content())
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(MenuItemMeta {
            id,
            label,
            disabled,
        });
    }
    out
}

#[cfg(target_arch = "wasm32")]
fn focus_element(id: &str) {
    if let Some(elem) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Some(html) = elem.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn click_element(id: &str) {
    if let Some(elem) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Some(html) = elem.dyn_ref::<web_sys::HtmlElement>() {
            html.click();
        }
    }
}

/// True when the menu currently contains a `.menu-filter` input. Type-ahead
/// is suppressed in that case so printable characters fall through to the
/// filter where they belong.
#[cfg(target_arch = "wasm32")]
fn menu_has_filter(menu_id: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(menu_id))
        .and_then(|m| m.query_selector(".menu-filter").ok().flatten())
        .is_some()
}

/// Trigger + popover wrapper. Spec contract is `components.md` §13.10.
///
/// The host pre-builds the trigger element with the right ARIA attributes
/// (`aria-haspopup="menu"`, `aria-expanded` driven from `open`,
/// `aria-controls` pointing at `menu_id`). Dropdown only owns the wrapper
/// chrome and behaviour — it never mutates the trigger's attributes.
///
/// The `active_id` prop is added on top of the §13.10 listing because the
/// keyboard handler that lives in this primitive needs a write handle to the
/// host's signal; without it the host would either have to subscribe via
/// context (worse ergonomics) or let the primitive own the signal (worse
/// SSR / test surface). Documented here so future readers see the deviation
/// explicitly.
#[component]
pub fn Dropdown(
    trigger_id: String,
    menu_id: String,
    open: Signal<bool>,
    active_id: Signal<Option<String>>,
    trigger: Element,
    children: Element,
    #[props(default)] placement: MenuPlacement,
    #[props(default = true)] close_on_select: bool,
) -> Element {
    use_context_provider(|| DropdownContext {
        open,
        active_id,
        close_on_select,
        trigger_id: trigger_id.clone(),
        menu_id: menu_id.clone(),
    });

    let placement_class = match placement {
        MenuPlacement::Below => "is-below",
        MenuPlacement::Above => "is-above",
    };

    // Outside-click closer. `use_hook` runs once on mount; the callback reads
    // the live `open` signal each invocation so the same listener handles
    // every open/close cycle for this Dropdown instance.
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::closure::Closure;

        let menu_id_outside = menu_id.clone();
        let trigger_id_outside = trigger_id.clone();
        let mut open_outside = open;
        use_hook(move || {
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |evt: web_sys::Event| {
                if !*open_outside.peek() {
                    return;
                }
                let Some(target) = evt.target() else { return };
                let Some(node) = target.dyn_ref::<web_sys::Node>() else {
                    return;
                };
                let Some(document) = web_sys::window().and_then(|w| w.document()) else {
                    return;
                };
                let menu = document.get_element_by_id(&menu_id_outside);
                let trigger = document.get_element_by_id(&trigger_id_outside);
                let inside = menu.as_ref().map_or(false, |m| m.contains(Some(node)))
                    || trigger.as_ref().map_or(false, |t| t.contains(Some(node)));
                if !inside {
                    open_outside.set(false);
                }
            });
            if let Some(document) = web_sys::window().and_then(|w| w.document()) {
                let _ = document
                    .add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
            }
            // Phase 1 simplification: the listener is leaked for the lifetime
            // of the page. The Dropdown lives as long as the authenticated
            // shell, so this is bounded; replace with a Drop-aware handle
            // when more dropdowns mount and unmount frequently.
            cb.forget();
        });
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(unused_variables))]
    let menu_id_for_keys = menu_id.clone();
    #[cfg_attr(not(target_arch = "wasm32"), allow(unused_variables))]
    let trigger_id_for_keys = trigger_id.clone();
    let mut open_keys = open;
    let mut active_keys = active_id;

    let onkeydown = move |evt: KeyboardEvent| {
        if !*open_keys.read() {
            return;
        }
        let key = evt.key();
        let cur = active_keys.read().clone();

        #[cfg(target_arch = "wasm32")]
        let items = collect_menu_items(&menu_id_for_keys);
        #[cfg(not(target_arch = "wasm32"))]
        let items: Vec<MenuItemMeta> = Vec::new();

        match key {
            Key::ArrowDown => {
                evt.prevent_default();
                active_keys.set(next_active_id(&items, cur.as_deref(), NavStep::Next));
            }
            Key::ArrowUp => {
                evt.prevent_default();
                active_keys.set(next_active_id(&items, cur.as_deref(), NavStep::Prev));
            }
            Key::Home => {
                evt.prevent_default();
                active_keys.set(next_active_id(&items, None, NavStep::First));
            }
            Key::End => {
                evt.prevent_default();
                active_keys.set(next_active_id(&items, None, NavStep::Last));
            }
            Key::Escape => {
                evt.prevent_default();
                open_keys.set(false);
                #[cfg(target_arch = "wasm32")]
                focus_element(&trigger_id_for_keys);
            }
            Key::Enter => {
                if let Some(id) = cur.as_ref() {
                    evt.prevent_default();
                    #[cfg(target_arch = "wasm32")]
                    click_element(id);
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = id;
                }
            }
            Key::Character(ref s) if !s.is_empty() => {
                #[cfg(target_arch = "wasm32")]
                {
                    if !menu_has_filter(&menu_id_for_keys) {
                        if let Some(next) = typeahead_match(&items, cur.as_deref(), s) {
                            evt.prevent_default();
                            active_keys.set(Some(next));
                        }
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = (s, &items);
                }
            }
            _ => {}
        }
    };

    let is_open = *open.read();

    rsx! {
        div {
            class: "dropdown-anchor {placement_class}",
            onkeydown,
            {trigger}
            if is_open {
                {children}
            }
        }
    }
}

#[component]
pub fn Menu(
    id: String,
    labelled_by: String,
    /// Static `aria-activedescendant` value — host falls back here when
    /// there is no `Dropdown` ancestor (SSR / standalone use). Live
    /// keyboard nav reads from the `Dropdown` context.
    active_id: Option<String>,
    children: Element,
) -> Element {
    let live = try_consume_context::<DropdownContext>();
    let resolved = live
        .as_ref()
        .and_then(|ctx| ctx.active_id.read().clone())
        .or(active_id);
    let active_attr = resolved.unwrap_or_default();

    // When the menu mounts, give it focus so keydowns bubble to the wrapper
    // when there is no `MenuFilter` to take focus first. The closure clones
    // the id so the `rsx!` below can still read the original.
    let id_for_effect = id.clone();
    use_effect(move || {
        #[cfg(target_arch = "wasm32")]
        {
            let id_for_focus = id_for_effect.clone();
            wasm_bindgen_futures::spawn_local(async move {
                focus_element_if_no_filter(&id_for_focus);
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = &id_for_effect;
        }
    });

    rsx! {
        div {
            class: "menu",
            id: "{id}",
            role: "menu",
            tabindex: "-1",
            "aria-labelledby": "{labelled_by}",
            "aria-activedescendant": "{active_attr}",
            {children}
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn focus_element_if_no_filter(menu_id: &str) {
    if menu_has_filter(menu_id) {
        // The filter input has its own autofocus; do not steal focus.
        return;
    }
    focus_element(menu_id);
}

#[component]
pub fn MenuItem(
    id: String,
    kind: MenuItemKind,
    #[props(default)] disabled: bool,
    #[props(default)] danger: bool,
    #[props(default)] rich: bool,
    #[props(default)] onselect: EventHandler<()>,
    children: Element,
) -> Element {
    let ctx = try_consume_context::<DropdownContext>();

    let active_id = ctx.as_ref().map(|c| c.active_id);
    let open_signal = ctx.as_ref().map(|c| c.open);
    let close_on_select = ctx.as_ref().map(|c| c.close_on_select).unwrap_or(true);
    let trigger_id = ctx
        .as_ref()
        .map(|c| c.trigger_id.clone())
        .unwrap_or_default();

    let is_active = active_id
        .as_ref()
        .and_then(|sig| sig.read().clone())
        .as_deref()
        == Some(id.as_str());

    let mut class = String::from("menu-item");
    if rich {
        class.push_str(" is-rich");
    }
    if danger {
        class.push_str(" is-danger");
    }
    if is_active {
        class.push_str(" is-active");
    }

    let role = kind.role();
    // `aria-checked` is left empty for `Action` items — the role itself
    // tells AT not to read a checked state. Browsers ignore the empty
    // attribute on `menuitem`. Tested in the unit suite below.
    let aria_checked = kind.aria_checked().unwrap_or("");
    let aria_disabled = if disabled { "true" } else { "false" };

    let id_for_active = id.clone();
    let id_attr = id.clone();
    let mut active_for_hover = active_id;
    let onmouseenter = move |_| {
        if disabled {
            return;
        }
        if let Some(sig) = active_for_hover.as_mut() {
            sig.set(Some(id_for_active.clone()));
        }
    };

    let mut open_for_click = open_signal;
    #[cfg_attr(not(target_arch = "wasm32"), allow(unused_variables))]
    let trigger_for_click = trigger_id.clone();
    let onclick = move |_evt: MouseEvent| {
        if disabled {
            return;
        }
        onselect.call(());
        // Checkbox items keep the menu open on toggle so the user can
        // multi-select; everything else closes when `close_on_select` is on.
        if close_on_select && !matches!(kind, MenuItemKind::Checkbox { .. }) {
            if let Some(sig) = open_for_click.as_mut() {
                sig.set(false);
            }
            #[cfg(target_arch = "wasm32")]
            if !trigger_for_click.is_empty() {
                focus_element(&trigger_for_click);
            }
        }
    };

    rsx! {
        button {
            r#type: "button",
            class: "{class}",
            id: "{id_attr}",
            role: "{role}",
            tabindex: "-1",
            "aria-checked": "{aria_checked}",
            "aria-disabled": "{aria_disabled}",
            onclick,
            onmouseenter,
            {children}
        }
    }
}

#[component]
pub fn MenuSection(label: String, children: Element) -> Element {
    rsx! {
        div { class: "menu-section-label", "{label}" }
        {children}
    }
}

#[component]
pub fn MenuFilter(
    value: Signal<String>,
    #[props(default)] placeholder: Option<String>,
    /// Optional `aria-label` override; defaults to the placeholder so the
    /// input still announces a name when the placeholder is empty.
    #[props(default)]
    aria_label: Option<String>,
) -> Element {
    let placeholder_text = placeholder.clone().unwrap_or_default();
    let label = aria_label
        .clone()
        .or_else(|| placeholder.clone())
        .unwrap_or_else(|| "Filter".to_string());
    let mut signal = value;
    let current = signal.read().clone();

    rsx! {
        input {
            class: "menu-filter",
            r#type: "text",
            placeholder: "{placeholder_text}",
            value: "{current}",
            autofocus: true,
            "aria-label": "{label}",
            oninput: move |evt| signal.set(evt.value()),
        }
    }
}

#[component]
pub fn MenuDivider() -> Element {
    rsx! { div { class: "menu-divider", role: "separator" } }
}

#[component]
pub fn MenuEmpty(#[props(default = String::from("No matches"))] text: String) -> Element {
    rsx! { div { class: "menu-empty", role: "presentation", "{text}" } }
}

#[component]
pub fn MenuFooter(children: Element) -> Element {
    rsx! { div { class: "menu-footer", {children} } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, label: &str, disabled: bool) -> MenuItemMeta {
        MenuItemMeta {
            id: id.into(),
            label: label.into(),
            disabled,
        }
    }

    #[test]
    fn role_and_aria_checked_per_kind() {
        assert_eq!(MenuItemKind::Action.role(), "menuitem");
        assert!(MenuItemKind::Action.aria_checked().is_none());

        let radio_on = MenuItemKind::Radio { checked: true };
        assert_eq!(radio_on.role(), "menuitemradio");
        assert_eq!(radio_on.aria_checked(), Some("true"));
        assert!(radio_on.is_checked());

        let radio_off = MenuItemKind::Radio { checked: false };
        assert_eq!(radio_off.aria_checked(), Some("false"));
        assert!(!radio_off.is_checked());

        let cb = MenuItemKind::Checkbox { checked: true };
        assert_eq!(cb.role(), "menuitemcheckbox");
        assert_eq!(cb.aria_checked(), Some("true"));
    }

    #[test]
    fn arrow_nav_walks_items_and_wraps() {
        let items = vec![
            meta("a", "Alpha", false),
            meta("b", "Bravo", false),
            meta("c", "Charlie", false),
        ];
        // No focus → ↓ lands on first.
        assert_eq!(
            next_active_id(&items, None, NavStep::Next),
            Some("a".into())
        );
        // Mid-list → ↓ advances.
        assert_eq!(
            next_active_id(&items, Some("a"), NavStep::Next),
            Some("b".into())
        );
        // Tail → ↓ wraps to head.
        assert_eq!(
            next_active_id(&items, Some("c"), NavStep::Next),
            Some("a".into())
        );
        // Head → ↑ wraps to tail.
        assert_eq!(
            next_active_id(&items, Some("a"), NavStep::Prev),
            Some("c".into())
        );
        // Home / End jump regardless of current.
        assert_eq!(
            next_active_id(&items, Some("b"), NavStep::First),
            Some("a".into())
        );
        assert_eq!(
            next_active_id(&items, Some("b"), NavStep::Last),
            Some("c".into())
        );
    }

    #[test]
    fn arrow_nav_skips_disabled_items() {
        let items = vec![
            meta("a", "Alpha", false),
            meta("b", "Bravo", true),
            meta("c", "Charlie", false),
        ];
        // ↓ from a should jump straight to c.
        assert_eq!(
            next_active_id(&items, Some("a"), NavStep::Next),
            Some("c".into())
        );
        // First / last skip disabled too.
        assert_eq!(
            next_active_id(&items, None, NavStep::First),
            Some("a".into())
        );
        assert_eq!(
            next_active_id(&items, None, NavStep::Last),
            Some("c".into())
        );
    }

    #[test]
    fn arrow_nav_returns_none_when_all_disabled() {
        let items = vec![meta("a", "Alpha", true), meta("b", "Bravo", true)];
        assert!(next_active_id(&items, None, NavStep::First).is_none());
        assert!(next_active_id(&items, Some("a"), NavStep::Next).is_none());
    }

    #[test]
    fn typeahead_jumps_to_first_prefix_match() {
        let items = vec![
            meta("a", "Alpha", false),
            meta("b", "Bravo", false),
            meta("c", "Charlie", false),
        ];
        assert_eq!(typeahead_match(&items, None, "B"), Some("b".into()));
        // Case-insensitive.
        assert_eq!(typeahead_match(&items, None, "c"), Some("c".into()));
        // No match → None.
        assert!(typeahead_match(&items, None, "z").is_none());
    }

    #[test]
    fn typeahead_starts_search_after_current_active() {
        let items = vec![
            meta("a1", "Alpha", false),
            meta("a2", "Alpine", false),
            meta("b", "Bravo", false),
        ];
        // Sitting on "a1", pressing "a" → land on "a2", not "a1".
        assert_eq!(typeahead_match(&items, Some("a1"), "a"), Some("a2".into()));
        // Sitting on "a2", "a" wraps back to "a1".
        assert_eq!(typeahead_match(&items, Some("a2"), "a"), Some("a1".into()));
    }

    #[test]
    fn typeahead_skips_disabled() {
        let items = vec![meta("a", "Alpha", true), meta("b", "Beta", false)];
        // Disabled "Alpha" must not be a typeahead target even though it
        // is a prefix match for "a".
        assert!(typeahead_match(&items, None, "a").is_none());
        assert_eq!(typeahead_match(&items, None, "b"), Some("b".into()));
    }

    #[test]
    fn typeahead_empty_prefix_is_noop() {
        let items = vec![meta("a", "Alpha", false)];
        assert!(typeahead_match(&items, None, "").is_none());
    }

    // ── Open-menu markup snapshot ────────────────────────────────────────
    //
    // Mounts a `Menu` outside a `Dropdown` (so the prop fallback is the
    // active source for `aria-activedescendant`) and SSR-renders it. This
    // is the cheapest way to pin the role attributes + activedescendant
    // wiring per `components.md` §13.8 without simulating a click and
    // re-rendering an open Dropdown.

    #[component]
    fn StaticMenuHarness() -> Element {
        rsx! {
            Menu {
                id: "snap-menu".to_string(),
                labelled_by: "snap-trigger".to_string(),
                active_id: Some("snap-item-2".to_string()),
                MenuItem {
                    id: "snap-item-1".to_string(),
                    kind: MenuItemKind::Radio { checked: true },
                    rich: true,
                    span { class: "check", "✓" }
                    span { class: "label", "Alpha" }
                }
                MenuItem {
                    id: "snap-item-2".to_string(),
                    kind: MenuItemKind::Action,
                    span { class: "label", "Bravo" }
                }
                MenuItem {
                    id: "snap-item-3".to_string(),
                    kind: MenuItemKind::Checkbox { checked: false },
                    disabled: true,
                    span { class: "label", "Charlie" }
                }
                MenuDivider {}
                MenuFooter {
                    a { class: "btn btn-secondary btn-sm", href: "/manage", "Manage" }
                }
            }
        }
    }

    fn render_static_menu() -> String {
        let mut dom = VirtualDom::new(StaticMenuHarness);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn menu_carries_role_and_aria_activedescendant() {
        let html = render_static_menu();
        assert!(
            html.contains(r#"role="menu""#),
            "menu container missing role='menu': {html}"
        );
        assert!(
            html.contains(r#"aria-labelledby="snap-trigger""#),
            "menu container missing aria-labelledby pointing at trigger: {html}"
        );
        assert!(
            html.contains(r#"aria-activedescendant="snap-item-2""#),
            "menu container missing aria-activedescendant for active item: {html}"
        );
    }

    #[test]
    fn radio_item_renders_menuitemradio_with_checked() {
        let html = render_static_menu();
        assert!(
            html.contains(r#"id="snap-item-1""#),
            "radio item missing its id: {html}"
        );
        assert!(
            html.contains(r#"role="menuitemradio""#),
            "radio item missing role='menuitemradio': {html}"
        );
        assert!(
            html.contains(r#"aria-checked="true""#),
            "checked radio item missing aria-checked='true': {html}"
        );
    }

    #[test]
    fn action_item_renders_menuitem_role() {
        let html = render_static_menu();
        // The action item must carry `role="menuitem"`; we look for the id
        // first to anchor on the right element.
        assert!(
            html.contains(r#"id="snap-item-2""#),
            "action item id missing: {html}"
        );
        assert!(
            html.contains(r#"role="menuitem""#),
            "action item missing role='menuitem': {html}"
        );
    }

    #[test]
    fn disabled_item_uses_aria_disabled_not_html_disabled() {
        let html = render_static_menu();
        // `aria-disabled="true"` is the correct disabled signal — the HTML
        // `disabled` attribute would remove the item from the focus order
        // and break aria-activedescendant nav (see §13.8).
        assert!(
            html.contains(r#"id="snap-item-3""#),
            "disabled item id missing: {html}"
        );
        assert!(
            html.contains(r#"aria-disabled="true""#),
            "disabled item missing aria-disabled='true': {html}"
        );
    }

    #[test]
    fn divider_is_separator_role() {
        let html = render_static_menu();
        assert!(
            html.contains(r#"role="separator""#),
            "menu divider missing role='separator': {html}"
        );
    }

    #[test]
    fn menu_items_keep_tabindex_minus_one() {
        let html = render_static_menu();
        // Spec §13.10: tabindex='-1' on items keeps Tab-traversal sane —
        // arrow-key nav uses aria-activedescendant rather than focus.
        let count = html.matches(r#"tabindex="-1""#).count();
        // Three items + the menu container itself = 4 minimum.
        assert!(
            count >= 4,
            "expected ≥4 tabindex='-1' (3 items + menu container), got {count}: {html}"
        );
    }
}
