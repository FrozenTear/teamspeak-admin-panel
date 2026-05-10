//! Bot lifecycle state machine — PURA-118 WS-1.
//!
//! Five named states (`Disconnected`, `Connecting`, `Connected`, `InChannel`,
//! `Disconnecting`) per the issue spec. Auto-reconnect after an unexpected
//! drop is modelled as a transition back into `Connecting`, not as a sixth
//! `Reconnecting` state — keeping the spec word-for-word and letting the
//! actor task track retry attempts internally.

use serde::{Deserialize, Serialize};

/// Bot lifecycle state. The spec lives in
/// `docs/voice/music-bot-lifecycle.md`; this enum is the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotState {
    /// Initial state. No connection, no in-flight handshake.
    Disconnected,
    /// Handshake in flight (first attempt OR an auto-reconnect attempt).
    Connecting,
    /// Handshake done, sitting in the server's default channel.
    Connected,
    /// Sitting in a specific channel reached via `JoinChannel`.
    InChannel,
    /// Clean shutdown in progress (driven by `Disconnect` / `Shutdown`).
    Disconnecting,
}

impl BotState {
    /// True when an external `BotCommand` may legally drive a transition out
    /// of this state. `Connecting` and `Disconnecting` are transient and
    /// owned by the bot actor itself — external commands queue.
    pub fn accepts_external_commands(self) -> bool {
        matches!(self, BotState::Disconnected | BotState::Connected | BotState::InChannel)
    }

    /// True when a connection-level event (handshake done / lost) is
    /// meaningful in this state.
    pub fn is_online(self) -> bool {
        matches!(self, BotState::Connected | BotState::InChannel)
    }
}

/// All legal transitions, exhaustively. Used by `BotState::can_transition`
/// and by the unit tests that assert illegal transitions are rejected.
const LEGAL: &[(BotState, BotState)] = &[
    // Auto-start (supervisor spawn) or `Connect` command.
    (BotState::Disconnected, BotState::Connecting),
    // Handshake won.
    (BotState::Connecting, BotState::Connected),
    // Handshake retried (timeout / temporary failure → backoff → re-enter).
    // Self-loop is allowed because we re-enter `Connecting` after every
    // backoff sleep without bouncing through `Disconnected`.
    (BotState::Connecting, BotState::Connecting),
    // Handshake aborted by `Shutdown` / `Disconnect` before completing.
    (BotState::Connecting, BotState::Disconnecting),
    // Server channel selected via `JoinChannel`.
    (BotState::Connected, BotState::InChannel),
    // Already in a channel, switched to another via `JoinChannel`.
    (BotState::InChannel, BotState::InChannel),
    // `LeaveChannel` while in a channel.
    (BotState::InChannel, BotState::Connected),
    // Network drop while online — drives auto-reconnect.
    (BotState::Connected, BotState::Connecting),
    (BotState::InChannel, BotState::Connecting),
    // External `Disconnect` / `Shutdown` command.
    (BotState::Connected, BotState::Disconnecting),
    (BotState::InChannel, BotState::Disconnecting),
    // Clean shutdown finishes — final state. From here a `Connect`
    // command may bring us back online.
    (BotState::Disconnecting, BotState::Disconnected),
];

impl BotState {
    pub fn can_transition(self, to: BotState) -> bool {
        LEGAL.iter().any(|(f, t)| *f == self && *t == to)
    }
}

/// Error returned by `BotState::transition` when a caller asks for a
/// transition that the state machine does not allow.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("illegal bot state transition: {from:?} → {to:?}")]
pub struct IllegalTransition {
    pub from: BotState,
    pub to: BotState,
}

impl BotState {
    /// Validate the transition; returns the new state on success.
    /// Wrapping at one place so the actor task and tests share a single
    /// rule set.
    pub fn transition(self, to: BotState) -> Result<BotState, IllegalTransition> {
        if self.can_transition(to) {
            Ok(to)
        } else {
            Err(IllegalTransition { from: self, to })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every state we list here MUST have at least one outgoing edge except
    /// the terminal one (`Disconnecting` ends in `Disconnected`, which we
    /// can leave again via `Connecting`). This guards against accidentally
    /// shipping a dead state.
    #[test]
    fn every_state_has_an_outgoing_edge() {
        for s in [
            BotState::Disconnected,
            BotState::Connecting,
            BotState::Connected,
            BotState::InChannel,
            BotState::Disconnecting,
        ] {
            assert!(
                LEGAL.iter().any(|(f, _)| *f == s),
                "state {s:?} has no outgoing legal transition"
            );
        }
    }

    #[test]
    fn happy_path_disconnected_to_in_channel_to_disconnected() {
        let s = BotState::Disconnected;
        let s = s.transition(BotState::Connecting).unwrap();
        let s = s.transition(BotState::Connected).unwrap();
        let s = s.transition(BotState::InChannel).unwrap();
        let s = s.transition(BotState::Disconnecting).unwrap();
        let s = s.transition(BotState::Disconnected).unwrap();
        assert_eq!(s, BotState::Disconnected);
    }

    #[test]
    fn auto_reconnect_self_loops_via_connecting() {
        let s = BotState::Connecting;
        // Backoff retry — Connecting → Connecting is legal.
        let s = s.transition(BotState::Connecting).unwrap();
        let s = s.transition(BotState::Connected).unwrap();
        // Network drop from Connected: → Connecting (auto-reconnect).
        let s = s.transition(BotState::Connecting).unwrap();
        assert_eq!(s, BotState::Connecting);
    }

    #[test]
    fn channel_switch_is_inchannel_self_loop() {
        let s = BotState::InChannel;
        assert!(s.can_transition(BotState::InChannel));
    }

    #[test]
    fn illegal_skips_are_rejected() {
        let cases = [
            (BotState::Disconnected, BotState::Connected),
            (BotState::Disconnected, BotState::InChannel),
            (BotState::Disconnected, BotState::Disconnecting),
            (BotState::Connecting, BotState::InChannel),
            (BotState::Connecting, BotState::Disconnected),
            (BotState::Connected, BotState::Disconnected),
            (BotState::InChannel, BotState::Disconnected),
            (BotState::Disconnecting, BotState::Connecting),
            (BotState::Disconnecting, BotState::Connected),
            (BotState::Disconnecting, BotState::InChannel),
        ];
        for (from, to) in cases {
            assert!(
                !from.can_transition(to),
                "expected illegal: {from:?} → {to:?}"
            );
            assert_eq!(
                from.transition(to).unwrap_err(),
                IllegalTransition { from, to }
            );
        }
    }

    #[test]
    fn external_command_states_are_only_quiescent_ones() {
        assert!(BotState::Disconnected.accepts_external_commands());
        assert!(BotState::Connected.accepts_external_commands());
        assert!(BotState::InChannel.accepts_external_commands());
        assert!(!BotState::Connecting.accepts_external_commands());
        assert!(!BotState::Disconnecting.accepts_external_commands());
    }
}
