//! flows/dialog.rs — confirmation dialog for destructive flow actions.
//!
//! `ui-brief.md` §3.1 requires a confirm step before a flow (and its run
//! history) is destroyed, and a *second, explicit* force-delete choice
//! when a run is in flight — never a silent auto-escalation (PURA-246 B1).
//! Built on the shared `.modal` primitive from `components.css`.

use dioxus::prelude::*;
use ts6_manager_shared::flows as wire;

/// Drives the two-stage delete-confirmation state machine shared by the
/// list and detail pages.
#[derive(Clone, Copy, PartialEq)]
pub enum DeletePrompt {
    /// No dialog open.
    Closed,
    /// First stage — confirm a plain delete (`force=false`).
    Confirm(wire::FlowId),
    /// Second stage — shown only after a `run_in_flight` 409. The explicit
    /// operator choice to interrupt a live run with `force=true`.
    Force(wire::FlowId),
}

impl DeletePrompt {
    /// Whether a dialog should currently render.
    pub fn is_open(self) -> bool {
        !matches!(self, DeletePrompt::Closed)
    }

    /// Whether this is the second-stage (force-delete) prompt.
    pub fn is_force(self) -> bool {
        matches!(self, DeletePrompt::Force(_))
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct ConfirmDialogProps {
    /// Heading shown in the modal header.
    pub title: String,
    /// Body copy describing the consequence of confirming.
    pub message: String,
    /// Label for the destructive confirm button.
    pub confirm_label: String,
    /// Disables both buttons and shows a working state while the request
    /// is in flight.
    #[props(default)]
    pub busy: bool,
    pub on_confirm: EventHandler<()>,
    pub on_cancel: EventHandler<()>,
}

/// A modal `alertdialog` with a Cancel + destructive-confirm button pair.
/// Cancel is auto-focused (the safe default for a destructive prompt) and
/// `Escape` / backdrop-click dismiss it unless a request is in flight.
#[component]
pub fn ConfirmDialog(props: ConfirmDialogProps) -> Element {
    let on_cancel = props.on_cancel;
    let on_confirm = props.on_confirm;
    let busy = props.busy;
    rsx! {
        div {
            class: "modal-backdrop",
            onclick: move |_| {
                if !busy {
                    on_cancel.call(());
                }
            },
            onkeydown: move |evt| {
                if evt.key() == Key::Escape && !busy {
                    evt.prevent_default();
                    on_cancel.call(());
                }
            },
            div {
                class: "modal",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-labelledby": "flow-confirm-title",
                "aria-describedby": "flow-confirm-body",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-header",
                    h2 { id: "flow-confirm-title", "{props.title}" }
                }
                div { class: "modal-body",
                    p { id: "flow-confirm-body", "{props.message}" }
                }
                div { class: "modal-footer",
                    button {
                        r#type: "button",
                        class: "btn btn-ghost",
                        autofocus: true,
                        disabled: busy,
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-danger",
                        disabled: busy,
                        onclick: move |_| on_confirm.call(()),
                        if busy {
                            "Working…"
                        } else {
                            "{props.confirm_label}"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_prompt_open_and_force_flags() {
        assert!(!DeletePrompt::Closed.is_open());
        assert!(!DeletePrompt::Closed.is_force());

        let confirm = DeletePrompt::Confirm(wire::FlowId(7));
        assert!(confirm.is_open(), "first-stage prompt should render");
        assert!(!confirm.is_force(), "first stage is not a force prompt");

        let force = DeletePrompt::Force(wire::FlowId(7));
        assert!(force.is_open(), "force prompt should render");
        assert!(force.is_force(), "second stage is the force prompt");
    }
}
