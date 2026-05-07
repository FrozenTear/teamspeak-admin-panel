use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ButtonVariant {
    #[default]
    Primary,
    Secondary,
    Ghost,
    Danger,
    Link,
}

impl ButtonVariant {
    const fn class(self) -> &'static str {
        match self {
            ButtonVariant::Primary => "btn btn-primary",
            ButtonVariant::Secondary => "btn btn-secondary",
            ButtonVariant::Ghost => "btn btn-ghost",
            ButtonVariant::Danger => "btn btn-danger",
            ButtonVariant::Link => "btn btn-link",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ButtonSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl ButtonSize {
    const fn class(self) -> &'static str {
        match self {
            ButtonSize::Small => "btn-sm",
            ButtonSize::Medium => "",
            ButtonSize::Large => "btn-lg",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ButtonType {
    #[default]
    Button,
    Submit,
    Reset,
}

impl ButtonType {
    const fn attr(self) -> &'static str {
        match self {
            ButtonType::Button => "button",
            ButtonType::Submit => "submit",
            ButtonType::Reset => "reset",
        }
    }
}

#[component]
pub fn Button(
    #[props(default)] variant: ButtonVariant,
    #[props(default)] size: ButtonSize,
    #[props(default)] kind: ButtonType,
    #[props(default)] disabled: bool,
    #[props(default)] loading: bool,
    #[props(default)] block: bool,
    #[props(default)] onclick: EventHandler<MouseEvent>,
    children: Element,
) -> Element {
    let mut class = String::from(variant.class());
    let size_class = size.class();
    if !size_class.is_empty() {
        class.push(' ');
        class.push_str(size_class);
    }
    if block {
        class.push_str(" btn-block");
    }

    let aria_busy = if loading { "true" } else { "false" };
    let is_disabled = disabled || loading;

    rsx! {
        button {
            class: "{class}",
            r#type: "{kind.attr()}",
            disabled: is_disabled,
            "aria-busy": "{aria_busy}",
            onclick: move |evt| onclick.call(evt),
            if loading {
                span { class: "spinner is-sm", aria_hidden: "true" }
            }
            {children}
        }
    }
}
