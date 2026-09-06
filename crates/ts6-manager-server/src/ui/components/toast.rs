//! Transient toast surface — `components.md` §12.2.
//!
//! Toasts are the global ephemeral feedback channel — anything operator-
//! visible that does not belong inside a surface-scoped `Banner` (auth
//! errors, dashboard 502, etc.) should fan through the [`Toaster`] context.
//!
//! Each entry auto-dismisses after `DEFAULT_TIMEOUT_MS`; the user can
//! dismiss earlier via the close button. Live region attributes mirror the
//! axe-clean playbook: success → `polite`, warning/danger → `assertive`.

use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;

const DEFAULT_TIMEOUT_MS: u32 = 5_000;
/// Maximum toasts shown simultaneously. Oldest is evicted when exceeded.
const MAX_VISIBLE: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastVariant {
    Info,
    Success,
    Warning,
    Danger,
}

impl ToastVariant {
    /// Wire name written into the Report bug toast ring. Danger maps to
    /// `"error"` so the payload matches the provisional API shape.
    pub fn wire_name(self) -> &'static str {
        match self {
            ToastVariant::Info => "info",
            ToastVariant::Success => "success",
            ToastVariant::Warning => "warning",
            ToastVariant::Danger => "error",
        }
    }

    fn class(self) -> &'static str {
        match self {
            ToastVariant::Info => "toast",
            ToastVariant::Success => "toast toast-success",
            ToastVariant::Warning => "toast toast-warning",
            ToastVariant::Danger => "toast toast-danger",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            ToastVariant::Info => "i",
            ToastVariant::Success => "✓",
            ToastVariant::Warning => "!",
            ToastVariant::Danger => "×",
        }
    }

    fn live(self) -> &'static str {
        match self {
            ToastVariant::Warning | ToastVariant::Danger => "assertive",
            _ => "polite",
        }
    }
}

#[cfg(test)]
mod wire_name_tests {
    use super::ToastVariant;

    #[test]
    fn danger_maps_to_error_on_the_wire() {
        assert_eq!(ToastVariant::Danger.wire_name(), "error");
        assert_eq!(ToastVariant::Info.wire_name(), "info");
        assert_eq!(ToastVariant::Success.wire_name(), "success");
        assert_eq!(ToastVariant::Warning.wire_name(), "warning");
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToastEntry {
    pub id: u64,
    pub variant: ToastVariant,
    pub title: String,
    pub detail: Option<String>,
    /// Optional outbound link (e.g. a GitHub issue created by Report bug).
    /// Rendered as a real `<a>` in the toast body so the operator can open
    /// it in one click.
    pub href: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Toaster {
    pub items: Signal<Vec<ToastEntry>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Toaster {
    pub fn push(&self, variant: ToastVariant, title: impl Into<String>, detail: Option<String>) {
        self.push_with_link(variant, title, detail, None);
    }

    pub fn push_with_link(
        &self,
        variant: ToastVariant,
        title: impl Into<String>,
        detail: Option<String>,
        href: Option<String>,
    ) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let title = title.into();
        let ring_message = match detail.as_deref() {
            Some(d) if !d.is_empty() => format!("{title}: {d}"),
            _ => title.clone(),
        };
        crate::client::diagnostics::record_toast(variant.wire_name(), ring_message);
        let entry = ToastEntry {
            id,
            variant,
            title,
            detail,
            href,
        };
        // Bind the Copy Signal first so `write()` is not on a temporary (E0716/E0502).
        let mut items = self.items;
        {
            let mut items = items.write();
            items.push(entry);
            if items.len() > MAX_VISIBLE {
                let overflow = items.len() - MAX_VISIBLE;
                items.drain(0..overflow);
            }
        }
        schedule_dismiss(self.items, id);
    }

    pub fn dismiss(&self, id: u64) {
        let mut items = self.items;
        items.write().retain(|e| e.id != id);
    }
}

#[cfg(target_arch = "wasm32")]
fn schedule_dismiss(mut items: Signal<Vec<ToastEntry>>, id: u64) {
    use gloo_timers::future::TimeoutFuture;
    wasm_bindgen_futures::spawn_local(async move {
        TimeoutFuture::new(DEFAULT_TIMEOUT_MS).await;
        items.write().retain(|e| e.id != id);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn schedule_dismiss(_items: Signal<Vec<ToastEntry>>, _id: u64) {
    // No-op on native: SSR snapshots assert the rendered toast list so
    // the dismiss timer never has a chance to fire anyway.
}

/// Mount the toaster in context. Returns the handle for the rare caller
/// that wants to push from outside Dioxus's context graph.
pub fn provide_toaster() -> Toaster {
    let items: Signal<Vec<ToastEntry>> = use_signal(Vec::new);
    let toaster = Toaster { items };
    use_context_provider(|| toaster);
    toaster
}

pub fn use_toaster() -> Toaster {
    use_context::<Toaster>()
}

/// Stack rendered top-right of the viewport.
#[component]
pub fn ToasterRegion() -> Element {
    let toaster = use_toaster();
    let items = toaster.items;
    rsx! {
        div { class: "toast-region",
            "aria-label": "Notifications",
            for entry in items.read().iter().cloned() {
                {
                    let id = entry.id;
                    rsx! {
                        div {
                            key: "{entry.id}",
                            class: "{entry.variant.class()}",
                            role: "status",
                            "aria-live": "{entry.variant.live()}",
                            div { class: "accent-bar", "aria-hidden": "true" }
                            span { class: "icon", "aria-hidden": "true", "{entry.variant.icon()}" }
                            div {
                                div { class: "title", "{entry.title}" }
                                if let Some(d) = entry.detail.as_ref() {
                                    div { class: "detail", "{d}" }
                                }
                                if let Some(href) = entry.href.as_ref() {
                                    a {
                                        class: "toast-link",
                                        href: "{href}",
                                        target: "_blank",
                                        rel: "noopener noreferrer",
                                        "{href}"
                                    }
                                }
                            }
                            button {
                                class: "close",
                                r#type: "button",
                                "aria-label": "Dismiss notification",
                                onclick: move |_| toaster.dismiss(id),
                                "×"
                            }
                        }
                    }
                }
            }
        }
    }
}
