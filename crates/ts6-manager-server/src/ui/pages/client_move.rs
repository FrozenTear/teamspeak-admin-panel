//! Move-user controls. `clientmove` only.
//!
//! Channel ↑/↓ reorder lives on the Channels page and must not call this
//! module. Error 770 (`already member of channel`) is soft-handled here
//! because it means the user is already in the picked channel.

use dioxus::prelude::*;
use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem};

use crate::client::api::ApiError;
use crate::client::clients::{self, ClientMoveOutcome};
use crate::client::dioxus::use_auth_gate;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Button, ButtonType, ButtonVariant};

use super::channels::is_spacer;

/// Channels a user can be moved into. Skips the channel they are already
/// in and spacer rows (those are layout, not destinations).
pub(crate) fn move_target_options(
    channels: &[ChannelTreeNode],
    current_cid: i64,
) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = channels
        .iter()
        .filter(|c| c.cid != current_cid)
        .filter(|c| !is_spacer(&c.channel_name))
        .map(|c| (c.cid, c.channel_name.clone()))
        .collect();
    out.sort_by(|a, b| {
        a.1.to_ascii_lowercase()
            .cmp(&b.1.to_ascii_lowercase())
            .then(a.0.cmp(&b.0))
    });
    out
}

pub(crate) fn channel_label(channels: &[ChannelTreeNode], cid: i64) -> String {
    channels
        .iter()
        .find(|c| c.cid == cid)
        .map(|c| c.channel_name.clone())
        .unwrap_or_else(|| format!("channel {cid}"))
}

/// Toast for a finished move-user attempt. 770 is an info notice, not
/// "Move failed".
pub(crate) fn client_move_toast(
    outcome: &ClientMoveOutcome,
    nick: &str,
    channel: &str,
) -> (ToastVariant, String, Option<String>) {
    match outcome {
        ClientMoveOutcome::Moved => (
            ToastVariant::Success,
            format!("Moved “{nick}” to “{channel}”"),
            None,
        ),
        ClientMoveOutcome::AlreadyThere => (
            ToastVariant::Info,
            format!("Already in “{channel}”"),
            Some(format!("{nick} is already in that channel.")),
        ),
        ClientMoveOutcome::Failed(err) => (
            ToastVariant::Danger,
            "Move user failed".into(),
            Some(format_move_error(err)),
        ),
    }
}

fn format_move_error(err: &ApiError) -> String {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut s = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(": ");
                s.push_str(d);
            }
            if let Some(c) = code {
                s.push_str(&format!(" (code {c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::SessionAnonymous => "Loading…".into(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Move unavailable in this view.".into(),
    }
}

#[derive(Props, Clone, PartialEq)]
pub(crate) struct ChannelDestinationFieldProps {
    id: String,
    targets: Vec<(i64, String)>,
    selected: i64,
    on_change: EventHandler<i64>,
}

/// `0` means "no destination yet". Real channel ids from TeamSpeak are
/// non-zero; `0` is the virtual root and is not a move target.
#[component]
pub(crate) fn ChannelDestinationField(props: ChannelDestinationFieldProps) -> Element {
    let on_change = props.on_change;
    rsx! {
        select {
            class: "input",
            id: "{props.id}",
            "aria-label": "Destination channel",
            value: "{props.selected}",
            onchange: move |e| {
                if let Ok(v) = e.value().parse::<i64>() {
                    on_change.call(v);
                }
            },
            option { value: "0", disabled: true, "Choose a channel" }
            for (cid, name) in props.targets.iter() {
                option { key: "{cid}", value: "{cid}", "{name}" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
pub(crate) struct MoveUserModalProps {
    server_id: i64,
    sid: i64,
    client: ClientListItem,
    channels: Vec<ChannelTreeNode>,
    on_close: EventHandler<()>,
    on_moved: EventHandler<()>,
}

#[component]
pub(crate) fn MoveUserModal(props: MoveUserModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let on_moved = props.on_moved;
    let clid = props.client.clid;
    let current_cid = props.client.cid;
    let nick = props.client.client_nickname.clone();
    let server_id = props.server_id;
    let sid = props.sid;
    let channels = props.channels.clone();
    let from = channel_label(&channels, current_cid);
    let targets = move_target_options(&channels, current_cid);

    let mut selected: Signal<i64> = use_signal(|| 0i64);
    let mut submitting: Signal<bool> = use_signal(|| false);

    let on_submit = {
        let channels = channels.clone();
        let nick = nick.clone();
        let gate = gate.clone();
        move |evt: FormEvent| {
            evt.prevent_default();
            if *submitting.read() {
                return;
            }
            let target = *selected.read();
            if target == 0 {
                return;
            }
            let channel = channel_label(&channels, target);
            submitting.set(true);
            let gate = gate.clone();
            let nick = nick.clone();
            spawn(async move {
                let outcome = if target == current_cid {
                    ClientMoveOutcome::AlreadyThere
                } else {
                    clients::move_client(gate, server_id, sid, clid, target)
                        .await
                        .into()
                };
                submitting.set(false);
                let (variant, title, detail) = client_move_toast(&outcome, &nick, &channel);
                toaster.push(variant, title, detail);
                if matches!(outcome, ClientMoveOutcome::Moved) {
                    on_moved.call(());
                } else if matches!(outcome, ClientMoveOutcome::AlreadyThere) {
                    on_close.call(());
                }
            });
        }
    };

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            form {
                class: "modal modal-sm",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: on_submit,
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "move-user-title",
                div { class: "modal-header",
                    h2 { id: "move-user-title", "Move user" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stack-md",
                    p { "Move “{props.client.client_nickname}” out of “{from}”." }
                    if targets.is_empty() {
                        p { class: "muted", "No other channel to move this user into." }
                    } else {
                        label { class: "field",
                            span { class: "field-label", "Destination" }
                            ChannelDestinationField {
                                id: "move-user-destination".to_string(),
                                targets: targets.clone(),
                                selected: *selected.read(),
                                on_change: EventHandler::new(move |cid: i64| selected.set(cid)),
                            }
                        }
                    }
                }
                div { class: "modal-footer",
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        disabled: targets.is_empty() || *selected.read() == 0,
                        "Move user"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(cid: i64, name: &str) -> ChannelTreeNode {
        ChannelTreeNode {
            cid,
            channel_name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn targets_skip_current_channel_and_spacers() {
        let rows = vec![
            ch(2, "Gaming"),
            ch(1, "Crashlanding"),
            ch(9, "[*spacer]---"),
            ch(4, "AFK Zone"),
        ];
        let targets = move_target_options(&rows, 1);
        assert_eq!(targets, vec![(4, "AFK Zone".into()), (2, "Gaming".into())]);
    }

    #[test]
    fn already_there_toast_is_info_not_a_failure() {
        let (variant, title, detail) =
            client_move_toast(&ClientMoveOutcome::AlreadyThere, "DJ-Bot", "Crashlanding");
        assert_eq!(variant, ToastVariant::Info);
        assert_eq!(title, "Already in “Crashlanding”");
        assert_eq!(
            detail.as_deref(),
            Some("DJ-Bot is already in that channel.")
        );
        assert_ne!(title, "Move user failed");
    }

    #[test]
    fn moved_toast_names_the_user_and_channel() {
        let (variant, title, detail) =
            client_move_toast(&ClientMoveOutcome::Moved, "DJ-Bot", "Gaming");
        assert_eq!(variant, ToastVariant::Success);
        assert_eq!(title, "Moved “DJ-Bot” to “Gaming”");
        assert!(detail.is_none());
    }

    #[test]
    fn failed_toast_keeps_the_upstream_code() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(2568),
            details: Some("invalid channel order".into()),
        };
        let (variant, title, detail) =
            client_move_toast(&ClientMoveOutcome::Failed(err), "DJ-Bot", "Gaming");
        assert_eq!(variant, ToastVariant::Danger);
        assert_eq!(title, "Move user failed");
        let detail = detail.unwrap();
        assert!(detail.contains("2568"));
        assert!(detail.contains("invalid channel order"));
    }
}
