use dioxus::prelude::*;

use crate::client::dioxus::SessionStorage;
use crate::client::ui_prefs;

/// Active visual theme. Persisted via [`crate::client::ui_prefs`] under the
/// `ts6-manager.ui.theme` `localStorage` key so the operator's choice
/// survives reloads (spec §28.3).
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

/// Props for [`ThemeProvider`]. `initial` is honoured only when the provider
/// owns a non-persistent backend (tests, the SSR pass before hydration); the
/// usual runtime path discards `initial` and reads from `storage` so the
/// browser's persisted preference always wins on hydration.
#[derive(Props, Clone)]
pub struct ThemeProviderProps {
    /// Storage backend for the persisted preference. Pull this from the same
    /// `DioxusSession::storage` clone used for the auth blob — they share a
    /// backend so test setups need only mount one `MemoryStore`.
    pub storage: SessionStorage,
    /// Override the hydrated value. Callers normally leave this `None`; tests
    /// that mount an empty `MemoryStore` use `Some(...)` to assert the
    /// override-vs-storage precedence.
    #[props(default)]
    pub initial: Option<Theme>,
    pub children: Element,
}

// Manual `PartialEq` because `SessionStorage = Arc<dyn Storage + …>` has no
// derivable equality. Identity via `Arc::ptr_eq` matches the runtime model:
// the App-wide storage is built once in `provide_session` and cloned into
// every consumer, so a "different storage" only happens when the test
// harness deliberately swaps it.
impl PartialEq for ThemeProviderProps {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.storage, &other.storage)
            && self.initial == other.initial
            && self.children == other.children
    }
}

/// Wraps the app in a `<div data-theme="…">` so `tokens.css` `[data-theme]`
/// selectors override the `:root` defaults. Children consume tokens via CSS
/// custom properties — there is no hex literal in component code.
///
/// On mount, hydrates the theme signal from `storage` (falling back to
/// `initial` and then to [`Theme::default`]). On every signal change a
/// `use_effect` writes the current theme back through
/// [`crate::client::ui_prefs::save_theme`], so the toggle button and any
/// future programmatic theme change automatically persist.
#[component]
pub fn ThemeProvider(props: ThemeProviderProps) -> Element {
    let storage = props.storage.clone();
    let hydrated = ui_prefs::load_theme(&*storage)
        .or(props.initial)
        .unwrap_or_default();
    let theme: Signal<Theme> = use_signal(|| hydrated);
    use_context_provider(|| ThemeContext { theme });

    // Persist on every change. The first effect run no-ops because the
    // hydrated value matches what's already in storage (or harmlessly seeds
    // the key on a first-ever boot). Subsequent runs catch toggles from the
    // header button.
    let storage_for_effect = props.storage.clone();
    use_effect(move || {
        let current = *theme.read();
        ui_prefs::save_theme(&*storage_for_effect, current);
    });

    let attr = theme.read().data_attr();
    rsx! {
        div {
            class: "theme-root",
            "data-theme": "{attr}",
            {props.children}
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
