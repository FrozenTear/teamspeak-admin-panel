//! Phone-width primary navigation.
//!
//! Desktop keeps the sidebar in the grid. At `max-width: 768px` the stylesheet
//! hides that sidebar and shows the header button. The button toggles
//! `.app.is-nav-open`, which reveals the same sidebar as a left drawer.
//! There is one set of links: [`super::sidebar::Sidebar`] renders them, and
//! activating one closes the drawer after the router follows it.

use dioxus::prelude::*;

use super::sidebar::NAV_LANDMARK_ID;

/// `id` of the header button that shows the primary nav on a narrow viewport.
/// `aria-controls` on that button points at [`NAV_LANDMARK_ID`].
pub(crate) const NAV_TOGGLE_ID: &str = "nav-toggle";

/// Shared open flag for the phone drawer. Desktop never reads it for layout;
/// the open class is scoped to the narrow breakpoint in CSS.
#[derive(Clone, Copy)]
pub(crate) struct NavDrawer {
    open: Signal<bool>,
}

impl NavDrawer {
    /// Mount the drawer signal, publish it for the header and the sidebar,
    /// and listen for Escape. Call once per [`super::AppShell`] render,
    /// before any early return, so the hook order stays stable.
    pub(crate) fn provide() -> Self {
        let drawer = Self {
            open: use_signal(|| false),
        };
        use_context_provider(|| drawer);
        bind_escape_listener(drawer);
        let drawer_for_focus = drawer;
        use_effect(move || {
            if drawer_for_focus.is_open() {
                focus_element_id(NAV_LANDMARK_ID);
            }
        });
        drawer
    }

    /// Harness constructor so a header test can render the open and closed
    /// labels without mounting the document listener.
    #[cfg(test)]
    pub(crate) fn new(open: Signal<bool>) -> Self {
        Self { open }
    }

    pub(crate) fn is_open(self) -> bool {
        *self.open.read()
    }

    pub(crate) fn close(self) {
        let mut open = self.open;
        open.set(false);
    }

    pub(crate) fn toggle(self) {
        let mut open = self.open;
        let next = !*open.peek();
        open.set(next);
    }

    /// Escape: close, then return focus to the header button.
    pub(crate) fn close_from_escape(self) {
        let mut open = self.open;
        if !*open.peek() {
            return;
        }
        open.set(false);
        focus_element_id(NAV_TOGGLE_ID);
    }

    /// A primary-nav link was activated. On a phone the drawer is covering
    /// the page, so close it and move focus to `<main>`. Desktop leaves
    /// focus alone: the drawer is not open, and the sidebar stays put.
    pub(crate) fn close_for_navigation(self) {
        let mut open = self.open;
        if !*open.peek() {
            return;
        }
        open.set(false);
        focus_main();
    }
}

fn bind_escape_listener(drawer: NavDrawer) {
    #[cfg(target_arch = "wasm32")]
    {
        use std::rc::Rc;

        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let mut open_key = drawer.open;
        let (key_rc, key_fn) = use_hook(move || {
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |evt: web_sys::Event| {
                if !*open_key.peek() || !event_is_escape(&evt) {
                    return;
                }
                evt.prevent_default();
                open_key.set(false);
                focus_element_id(NAV_TOGGLE_ID);
            });
            let func = cb.as_ref().unchecked_ref::<js_sys::Function>().clone();
            if let Some(document) = web_sys::window().and_then(|w| w.document()) {
                let _ = document.add_event_listener_with_callback("keydown", &func);
            }
            (Rc::new(cb), func)
        });
        use_drop(move || {
            if let Some(document) = web_sys::window().and_then(|w| w.document()) {
                let _ = document.remove_event_listener_with_callback("keydown", &key_fn);
            }
            drop(key_rc);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = drawer;
    }
}

#[cfg(target_arch = "wasm32")]
fn event_is_escape(evt: &web_sys::Event) -> bool {
    js_sys::Reflect::get(evt, &wasm_bindgen::JsValue::from_str("key"))
        .ok()
        .and_then(|value| value.as_string())
        .as_deref()
        == Some("Escape")
}

pub(crate) fn focus_element_id(id: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;

        let Some(html) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(id))
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            return;
        };
        let _ = html.focus();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = id;
    }
}

fn focus_main() {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;

        let Some(html) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.query_selector("main.main").ok().flatten())
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            return;
        };
        let _ = html.focus();
    }
}
