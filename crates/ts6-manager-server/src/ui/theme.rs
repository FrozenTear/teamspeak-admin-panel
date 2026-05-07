use dioxus::prelude::*;

/// Active visual theme. Persisted via UI-prefs storage in a future ticket;
/// for now the choice lives in component state only.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    /// String value for the `data-theme` attribute that `tokens.css` keys off.
    pub const fn data_attr(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

/// Context handed down by [`ThemeProvider`] so descendants can read/toggle.
#[derive(Clone, Copy)]
pub struct ThemeContext {
    pub theme: Signal<Theme>,
}

/// Wraps the app in a `<div data-theme="…">` so `tokens.css` `[data-theme]`
/// selectors override the `:root` defaults. Children consume tokens via CSS
/// custom properties — there is no hex literal in component code.
#[component]
pub fn ThemeProvider(initial: Option<Theme>, children: Element) -> Element {
    let theme = use_signal(|| initial.unwrap_or_default());
    use_context_provider(|| ThemeContext { theme });

    let attr = theme.read().data_attr();
    rsx! {
        div {
            class: "theme-root",
            "data-theme": "{attr}",
            {children}
        }
    }
}

/// Read the current theme context from a descendant.
pub fn use_theme() -> ThemeContext {
    use_context::<ThemeContext>()
}

/// `prefers-reduced-motion` matchMedia hook.
///
/// On the server (and during SSR) this returns `false` — the browser is the
/// only place the media query is meaningful, and the CSS already honours the
/// `@media (prefers-reduced-motion: reduce)` block in `tokens.css` directly.
/// Components that need imperative branching (canvas tweens, JS-driven
/// timelines) read this signal once they hydrate. Browser-side matchMedia
/// wiring lands when the canvas Engineer needs it; for the scaffold slice the
/// signal is a stable `false` so component code can be written against the
/// real signature today.
pub fn use_reduced_motion() -> Signal<bool> {
    use_signal(|| false)
}
