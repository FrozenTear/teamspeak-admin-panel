use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum BannerVariant {
    #[default]
    Info,
    Success,
    Warning,
    Danger,
}

impl BannerVariant {
    const fn class(self) -> &'static str {
        match self {
            BannerVariant::Info => "banner banner-info",
            BannerVariant::Success => "banner banner-success",
            BannerVariant::Warning => "banner banner-warning",
            BannerVariant::Danger => "banner banner-danger",
        }
    }

    const fn role(self) -> &'static str {
        match self {
            BannerVariant::Danger | BannerVariant::Warning => "alert",
            _ => "status",
        }
    }
}

/// Inline banner per `components.md` §12. Use for surface-scoped messaging
/// (auth errors, setup-step warnings, dashboard degraded-state notices).
/// Toasts are for transient global feedback — see the future `Toast` primitive.
#[component]
pub fn Banner(
    #[props(default)] variant: BannerVariant,
    #[props(default)] title: Option<String>,
    children: Element,
) -> Element {
    rsx! {
        div { class: "{variant.class()}", role: "{variant.role()}",
            div { class: "body",
                if let Some(t) = title.as_ref() {
                    div { class: "title", "{t}" }
                }
                {children}
            }
        }
    }
}
