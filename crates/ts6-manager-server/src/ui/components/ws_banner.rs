//! Non-blocking WS reconnect banner — PURA-73 / spec §28.4.
//!
//! Reads [`crate::client::ws::ConnectionState`] from context. While the
//! socket is `Disconnected` the banner is visible; on recovery
//! (`Connected`) it disappears immediately. `Connecting` and `Unauthorized`
//! are also surfaced for the small frames that lead up to a stable state.

use dioxus::prelude::*;

use crate::client::ws::{ConnectionState, use_ws_hub};

#[component]
pub fn WsReconnectBanner() -> Element {
    let hub = use_ws_hub();
    let state_signal = hub.state();
    let state = *state_signal.read();
    match state {
        ConnectionState::Connected => rsx! { "" },
        ConnectionState::Connecting => rsx! {
            div { class: "ws-banner",
                role: "status",
                "aria-live": "polite",
                span { class: "spinner is-sm", "aria-hidden": "true" }
                span { "Connecting to live updates…" }
            }
        },
        ConnectionState::Disconnected => rsx! {
            div { class: "ws-banner ws-banner-warn",
                role: "status",
                "aria-live": "polite",
                span { class: "spinner is-sm", "aria-hidden": "true" }
                span { "Live updates paused — retrying connection." }
            }
        },
        ConnectionState::Unauthorized => rsx! {
            div { class: "ws-banner ws-banner-error",
                role: "alert",
                span { class: "icon", "aria-hidden": "true", "!" }
                span {
                    "Live updates unavailable. Sign in again if this persists."
                }
            }
        },
    }
}
