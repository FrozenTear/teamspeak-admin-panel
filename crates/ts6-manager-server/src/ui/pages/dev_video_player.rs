//! `/dev/video-player?relay=…&ns=…` — dev-only mount for PURA-143 WS-5.
//!
//! Operator-facing chrome lives in WS-7 (PURA-145, FE Video Stream UI) and
//! the public `/widget/<token>` mount in WS-8. This page exists so the
//! manual two-tab smoke test described in the WS-5 acceptance contract has
//! a reachable route inside the SPA. Gated by `cfg(debug_assertions)` at
//! the `Route` enum level — release builds (`dx serve --release` via
//! [PURA-124](/PURA/issues/PURA-124)) do NOT expose it.

#![allow(dead_code)]

use dioxus::prelude::*;

use crate::ui::components::VideoPlayer;

#[component]
pub fn DevVideoPlayerPage(relay: Option<String>, ns: Option<String>) -> Element {
    // Default to the local sidecar binding from PURA-140 / PURA-141 so the
    // page works with no query parameters when the operator runs the
    // smoke per docs/ts6-fixture.md. Both can be overridden via URL.
    let relay = relay.unwrap_or_else(|| "https://127.0.0.1:4443/anon".to_string());
    let ns = ns.unwrap_or_else(|| "pura-spike/0".to_string());
    let autoplay = !ns.is_empty();

    rsx! {
        main {
            style: "max-width: 1280px; margin: 0 auto; padding: 16px; font-family: ui-monospace, monospace; color: #e6e8eb; background: #0d0f12; min-height: 100vh;",
            h1 { style: "font-size: 16px; margin: 0 0 12px 0;", "TS6 Manager · dev video player" }
            p {
                style: "font-size: 12px; color: #8a929c; margin: 0 0 12px 0;",
                "relay: ", code { "{relay}" }, " · namespace: ", code { "{ns}" }
            }
            VideoPlayer {
                relay_url: relay,
                namespace: ns,
                autoplay: autoplay,
            }
            p {
                style: "font-size: 11px; color: #8a929c; margin: 16px 0 0 0;",
                "WS-5 (PURA-143) — manual two-tab smoke. Override via ", code { "?relay=…&ns=…" }, "."
            }
        }
    }
}
