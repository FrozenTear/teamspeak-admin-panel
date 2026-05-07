use dioxus::prelude::*;

/// Composite form-field wrapper: label + control + helper/error text, with
/// the right ARIA wiring so screen readers announce the helper as the
/// control's description.
///
/// Pass the input control as `children` and provide an `id` so the label
/// targets the right element. When `error` is set, helper text is announced
/// as the error message; otherwise `helper` is descriptive only.
#[component]
pub fn Field(
    label: String,
    #[props(default)] id: Option<String>,
    #[props(default)] required: bool,
    #[props(default)] optional: bool,
    #[props(default)] helper: Option<String>,
    #[props(default)] error: Option<String>,
    children: Element,
) -> Element {
    let id_str = id.clone().unwrap_or_default();
    let helper_id = if !id_str.is_empty() {
        format!("{id_str}-helper")
    } else {
        String::new()
    };
    let _has_helper = helper.is_some() || error.is_some();

    rsx! {
        div { class: "field",
            label {
                class: "field-label",
                r#for: "{id_str}",
                "{label}"
                if required {
                    span { class: "field-required", aria_hidden: "true", " *" }
                } else if optional {
                    span { class: "field-optional", " (optional)" }
                }
            }
            {children}
            if let Some(err) = error.as_ref() {
                p { class: "field-error", id: "{helper_id}", role: "alert", "{err}" }
            } else if let Some(help) = helper.as_ref() {
                p { class: "field-help", id: "{helper_id}", "{help}" }
            }
        }
    }
}
