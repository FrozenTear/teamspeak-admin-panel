use dioxus::prelude::*;

/// Plain single-line text input. `Field` is the labelled wrapper; this is the
/// raw control without surrounding chrome so `Field` can compose it.
#[component]
pub fn TextInput(
    #[props(default)] value: Option<String>,
    #[props(default)] placeholder: Option<String>,
    #[props(default)] name: Option<String>,
    #[props(default)] id: Option<String>,
    #[props(default)] autocomplete: Option<String>,
    #[props(default)] required: bool,
    #[props(default)] disabled: bool,
    #[props(default)] readonly: bool,
    #[props(default)] error: bool,
    #[props(default)] aria_describedby: Option<String>,
    #[props(default)] oninput: EventHandler<FormEvent>,
    #[props(default)] onchange: EventHandler<FormEvent>,
) -> Element {
    let class = if error { "input is-error" } else { "input" };
    rsx! {
        input {
            class: "{class}",
            r#type: "text",
            value: value.clone().unwrap_or_default(),
            placeholder: placeholder.clone().unwrap_or_default(),
            name: name.clone().unwrap_or_default(),
            id: id.clone().unwrap_or_default(),
            autocomplete: autocomplete.clone().unwrap_or_default(),
            "aria-describedby": aria_describedby.clone().unwrap_or_default(),
            "aria-invalid": if error { "true" } else { "false" },
            required,
            disabled,
            readonly,
            oninput: move |evt| oninput.call(evt),
            onchange: move |evt| onchange.call(evt),
        }
    }
}

/// Password input with optional show/hide toggle (toggle implementation lives
/// in the surface that owns the visibility state; this control is the raw
/// secret-entry field per spec §2).
///
/// Both `oninput` and `onchange` carry the same signal-setter callback at
/// every call-site: password managers and Chromium's saved-credential fill
/// route value commits through `change` only, skipping `input`. Without the
/// `onchange` mirror, a controlled signal stays empty while the DOM shows
/// the autofilled value and submit-gates wedge on `is_empty()`. See
/// PURA-208 for the field report.
#[component]
pub fn PasswordInput(
    #[props(default)] value: Option<String>,
    #[props(default)] placeholder: Option<String>,
    #[props(default)] name: Option<String>,
    #[props(default)] id: Option<String>,
    #[props(default)] autocomplete: Option<String>,
    #[props(default)] required: bool,
    #[props(default)] disabled: bool,
    #[props(default)] error: bool,
    #[props(default = false)] reveal: bool,
    #[props(default)] aria_describedby: Option<String>,
    #[props(default)] oninput: EventHandler<FormEvent>,
    #[props(default)] onchange: EventHandler<FormEvent>,
) -> Element {
    let class = if error { "input is-error" } else { "input" };
    let input_type = if reveal { "text" } else { "password" };
    rsx! {
        input {
            class: "{class}",
            r#type: "{input_type}",
            value: value.clone().unwrap_or_default(),
            placeholder: placeholder.clone().unwrap_or_default(),
            name: name.clone().unwrap_or_default(),
            id: id.clone().unwrap_or_default(),
            autocomplete: autocomplete.clone().unwrap_or_else(|| "current-password".to_string()),
            "aria-describedby": aria_describedby.clone().unwrap_or_default(),
            "aria-invalid": if error { "true" } else { "false" },
            required,
            disabled,
            oninput: move |evt| oninput.call(evt),
            onchange: move |evt| onchange.call(evt),
        }
    }
}
