//! `Switch` — 2-state boolean control. `components.md` §18.
//!
//! Used for every boolean (`b_*`) permission row and the boolean group
//! settings in the moderation group editors (PURA-369 Phase A).
//!
//! The whole row is one `<button role="switch">`: the visible label is part
//! of the button's content, so the button's accessible name is the label and
//! clicking anywhere on the row toggles. `<button>` already answers Space and
//! Enter, so `role="switch"` needs no extra keyboard handler — the native
//! activation behaviour plus `aria-checked` is the full contract.
//!
//! The thumb slide is a `transform` transition on `.switch-thumb`; the
//! `prefers-reduced-motion` block in `tokens.css` collapses every transition
//! to ~0ms, so reduced-motion users get an instant swap with no slide for
//! free — see §18.3.

use dioxus::prelude::*;

/// 2-state toggle. Spec contract is `components.md` §18.
///
/// `checked` is host-owned — `Switch` never mutates it; it reports the
/// requested next value through `onchange` and the host writes its signal.
#[component]
pub fn Switch(
    /// Visible label. Always rendered, to the left of the track. Doubles as
    /// the button's accessible name, so it must read as a name on its own
    /// (e.g. "Can kick clients", not "Kick").
    label: String,
    /// Current on/off state. Host-owned.
    checked: bool,
    #[props(default)] disabled: bool,
    /// Optional element id — lets a `Field` label or external markup target
    /// the control. Omitted from the DOM when `None`.
    #[props(default)]
    id: Option<String>,
    /// Optional `aria-describedby` target — the id of helper or error text
    /// that describes what the toggle does.
    #[props(default)]
    described_by: Option<String>,
    /// Fired with the requested next state when the operator toggles.
    #[props(default)]
    onchange: EventHandler<bool>,
) -> Element {
    let class = if checked { "switch is-on" } else { "switch" };
    let aria_checked = if checked { "true" } else { "false" };

    rsx! {
        button {
            r#type: "button",
            class: "{class}",
            id,
            role: "switch",
            "aria-checked": "{aria_checked}",
            "aria-describedby": described_by,
            disabled,
            onclick: move |_| {
                if !disabled {
                    onchange.call(!checked);
                }
            },
            span { class: "switch-label", "{label}" }
            span { class: "switch-track", aria_hidden: "true",
                span { class: "switch-thumb" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(checked: bool, disabled: bool) -> String {
        // A tiny harness component so the `#[component]` macro's prop struct
        // is exercised the same way a real caller would build it.
        #[component]
        fn Harness(checked: bool, disabled: bool) -> Element {
            rsx! {
                Switch {
                    label: "Can kick clients".to_string(),
                    checked,
                    disabled,
                    id: "perm-b-kick".to_string(),
                    described_by: "perm-b-kick-help".to_string(),
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { checked, disabled });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn carries_switch_role_and_label() {
        let html = render(false, false);
        assert!(
            html.contains(r#"role="switch""#),
            "switch missing role='switch': {html}"
        );
        assert!(
            html.contains("Can kick clients"),
            "switch missing its visible label: {html}"
        );
        assert!(
            html.contains(r#"id="perm-b-kick""#),
            "switch dropped its id prop: {html}"
        );
        assert!(
            html.contains(r#"aria-describedby="perm-b-kick-help""#),
            "switch dropped its aria-describedby: {html}"
        );
    }

    #[test]
    fn aria_checked_tracks_state() {
        assert!(
            render(false, false).contains(r#"aria-checked="false""#),
            "off switch must report aria-checked='false'"
        );
        let on = render(true, false);
        assert!(
            on.contains(r#"aria-checked="true""#),
            "on switch must report aria-checked='true': {on}"
        );
        assert!(
            on.contains("switch is-on"),
            "on switch must carry the .is-on class: {on}"
        );
    }

    #[test]
    fn disabled_switch_renders_disabled_attr() {
        let html = render(false, true);
        assert!(
            html.contains("disabled"),
            "disabled switch missing the disabled attribute: {html}"
        );
    }
}
