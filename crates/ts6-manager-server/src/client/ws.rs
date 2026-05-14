//! Browser WS client for the operator hub — PURA-73.
//!
//! Owns one [`gloo_net::websocket::futures::WebSocket`] per session. Drives
//! the four-frame protocol from [`crate::ws::session`] (subscribe /
//! unsubscribe / ping / pong) and surfaces inbound envelopes through
//! per-topic [`futures::channel::mpsc`] streams the UI consumes.
//!
//! ## Reconnect contract
//!
//! Spec §28.4 — "non-blocking banner when the hub drops, auto-reconnect with
//! backoff, hide on recovery". Each disconnect bumps a state field that the
//! reconnect banner reads via context; the reconnect loop schedules the next
//! attempt with exponential backoff (`INITIAL_BACKOFF_MS` doubling up to
//! `MAX_BACKOFF_MS`, ±25 % jitter to avoid the reconnect-storm pattern called
//! out in PURA-84). Successful reconnect drops the banner immediately.
//!
//! `lastEventId` per topic is preserved across reconnects so the ring-buffer
//! replay path in [`crate::ws::envelope::RingBuffer`] fills the gap with the
//! exact set of envelopes the client missed.
//!
//! ## Native target
//!
//! Compiles on both wasm32 and native. The native build never opens a
//! socket; `subscribe` returns a stream that never yields. SSR snapshot
//! tests render against this stub so they don't spawn browser-only futures.

#![allow(dead_code)]

#[cfg(target_arch = "wasm32")]
use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::dioxus::use_session;
use crate::client::store::AuthState;

/// Initial reconnect backoff. Tight enough that a transient blip resolves
/// in well under a second.
const INITIAL_BACKOFF_MS: u32 = 250;
/// Cap. After ~5 doublings we sit at 8s — long enough not to hammer a
/// genuinely-down server, short enough that a recovering server gets picked
/// up promptly.
const MAX_BACKOFF_MS: u32 = 8_000;

/// Wire envelope from [`crate::ws::envelope`]. Mirrors the server-side
/// shape verbatim — `id`, `topic`, `type`, `data`, `ts`. We deserialise into
/// a free-form `serde_json::Value` so per-page consumers can decode the
/// `data` payload against their own typed struct.
#[derive(Debug, Clone, Deserialize)]
pub struct WsEvent {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub topic: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub ts: i64,
}

/// Outbound client frame — matches the `kind`-tagged shape parsed by
/// [`crate::ws::session::ClientFrame`].
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum OutFrame<'a> {
    Subscribe {
        topic: &'a str,
        #[serde(rename = "lastEventId", skip_serializing_if = "Option::is_none")]
        last_event_id: Option<u64>,
    },
    Unsubscribe {
        topic: &'a str,
    },
}

/// Connection state observable to the UI. The reconnect banner reads the
/// `Disconnected` variant; the activity feed clears its in-flight indicator
/// once `Connected` flips back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial state + every reconnect attempt that has not yet upgraded.
    Connecting,
    /// Socket is open and authenticated.
    Connected,
    /// Socket has dropped; reconnect loop has scheduled the next attempt.
    Disconnected,
    /// Auth path rejected the credential. Reconnect loop is paused; the
    /// AppShell auth gate will bounce the user to /login on the next 401.
    Unauthorized,
}

#[cfg(target_arch = "wasm32")]
enum HubCommand {
    Subscribe {
        topic: String,
        sink: mpsc::UnboundedSender<WsEvent>,
    },
    Unsubscribe {
        topic: String,
    },
}

/// Shared hub state.
#[derive(Clone)]
pub struct WsHub {
    inner: Arc<WsHubInner>,
}

struct WsHubInner {
    state: SyncSignal<ConnectionState>,
    #[cfg(target_arch = "wasm32")]
    cmd: futures::lock::Mutex<Option<mpsc::UnboundedSender<HubCommand>>>,
}

impl WsHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WsHubInner {
                state: SyncSignal::new_maybe_sync(ConnectionState::Connecting),
                #[cfg(target_arch = "wasm32")]
                cmd: futures::lock::Mutex::new(None),
            }),
        }
    }

    /// Connection-state signal. Mounted into context so the reconnect
    /// banner re-renders on every state transition without each consumer
    /// owning its own subscription.
    pub fn state(&self) -> SyncSignal<ConnectionState> {
        self.inner.state
    }

    /// Drop the existing connection (if any) and re-arm the runtime task
    /// against the supplied access token. Called from `use_ws_lifecycle`
    /// on every Anonymous → Authenticated transition.
    #[cfg(target_arch = "wasm32")]
    pub fn rearm(&self, access_token: String) {
        let inner = self.inner.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let (cmd_tx, cmd_rx) = mpsc::unbounded::<HubCommand>();
            // Replacing the slot drops the previous tx, which causes any
            // still-running runtime loop's `cmd_rx.next()` to resolve `None`
            // and exit cleanly via `DriveExit::ShuttingDown`.
            *inner.cmd.lock().await = Some(cmd_tx);
            run_loop(inner, cmd_rx, access_token).await;
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn rearm(&self, _access_token: String) {
        // No-op on native — SSR / unit tests never open a socket.
    }

    /// Subscribe to a topic. The returned receiver yields every envelope
    /// whose `topic` matches; dropping the [`SubscriptionHandle`] sends an
    /// unsubscribe so the server stops fanning to this connection.
    #[cfg(target_arch = "wasm32")]
    pub async fn subscribe(&self, topic: impl Into<String>) -> SubscriptionHandle {
        let topic: String = topic.into();
        let (tx, rx) = mpsc::unbounded::<WsEvent>();
        let cmd_tx = self.inner.cmd.lock().await.clone();
        if let Some(cmd) = cmd_tx.as_ref() {
            let _ = cmd.unbounded_send(HubCommand::Subscribe {
                topic: topic.clone(),
                sink: tx,
            });
        }
        SubscriptionHandle {
            topic,
            rx: Some(rx),
            cmd: cmd_tx,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn subscribe(&self, topic: impl Into<String>) -> SubscriptionHandle {
        let (_tx, rx) = mpsc::unbounded::<WsEvent>();
        SubscriptionHandle {
            topic: topic.into(),
            rx: Some(rx),
        }
    }
}

/// Subscription guard. `Drop` queues an `unsubscribe` for the topic so a
/// page that unmounts before the user navigates away doesn't keep the
/// hub fanning out events to a discarded receiver.
pub struct SubscriptionHandle {
    topic: String,
    rx: Option<mpsc::UnboundedReceiver<WsEvent>>,
    #[cfg(target_arch = "wasm32")]
    cmd: Option<mpsc::UnboundedSender<HubCommand>>,
}

impl SubscriptionHandle {
    pub fn take_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<WsEvent>> {
        self.rx.take()
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(cmd) = self.cmd.as_ref() {
                let _ = cmd.unbounded_send(HubCommand::Unsubscribe {
                    topic: self.topic.clone(),
                });
            }
        }
    }
}

/// Build + provide a [`WsHub`] in Dioxus context.
pub fn provide_ws_hub() -> WsHub {
    let hub = WsHub::new();
    use_context_provider(|| hub.clone());
    hub
}

pub fn use_ws_hub() -> WsHub {
    use_context::<WsHub>()
}

/// Spawn (or replace) the runtime task whenever the session transitions
/// from anonymous to authenticated. Token refreshes do NOT re-arm — the
/// WS handshake authenticates once on connect and a JWT rotation alone
/// doesn't invalidate the open connection.
///
/// `last_authed` is read with `peek()` (no subscription) and only written
/// when the value actually flips. Reading it via `read()` would subscribe
/// the effect to the same signal it writes to, and `Signal::set` notifies
/// subscribers even when the value is unchanged — that combination becomes
/// a self-retriggering loop that pegs the renderer's main thread on every
/// render. PURA-132 traced the headless deadlock to this loop.
pub fn use_ws_lifecycle(hub: WsHub) {
    let session = use_session();
    let mut last_authed: Signal<bool> = use_signal(|| false);
    use_effect(move || {
        let (now_authed, access) = match &*session.state.read() {
            AuthState::Authenticated { access, .. } => (true, Some(access.clone())),
            AuthState::Anonymous => (false, None),
        };
        let prev = *last_authed.peek();
        if prev == now_authed {
            return;
        }
        if !prev
            && now_authed
            && let Some(t) = access
        {
            hub.rearm(t);
        }
        last_authed.set(now_authed);
    });
}

#[cfg(target_arch = "wasm32")]
struct TopicStateInner {
    last_event_id: Option<u64>,
    sinks: Vec<mpsc::UnboundedSender<WsEvent>>,
}

#[cfg(target_arch = "wasm32")]
async fn run_loop(
    inner: Arc<WsHubInner>,
    mut cmd_rx: mpsc::UnboundedReceiver<HubCommand>,
    access_token: String,
) {
    use gloo_net::websocket::Message;
    use gloo_net::websocket::futures::WebSocket;

    // SyncSignal<T> is Copy (Dioxus's intentional design — see the
    // Signal-by-value semantics on the existing DioxusSession). Take the
    // copy here so we have an owned `mut` handle for `.set(...)`.
    let mut state = inner.state;

    let url = match build_ws_url(&access_token) {
        Some(u) => u,
        None => {
            state.set(ConnectionState::Unauthorized);
            return;
        }
    };

    let mut subs: HashMap<String, TopicStateInner> = HashMap::new();
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    loop {
        state.set(ConnectionState::Connecting);
        let mut socket = match WebSocket::open(&url) {
            Ok(s) => s,
            Err(_) => {
                state.set(ConnectionState::Disconnected);
                sleep_backoff(&mut backoff_ms).await;
                continue;
            }
        };
        state.set(ConnectionState::Connected);
        backoff_ms = INITIAL_BACKOFF_MS;

        let mut send_failed = false;
        for (topic, st) in subs.iter() {
            let frame = OutFrame::Subscribe {
                topic: topic.as_str(),
                last_event_id: st.last_event_id,
            };
            if let Ok(j) = serde_json::to_string(&frame) {
                use futures::SinkExt;
                if socket.send(Message::Text(j)).await.is_err() {
                    send_failed = true;
                    break;
                }
            }
        }
        if send_failed {
            state.set(ConnectionState::Disconnected);
            sleep_backoff(&mut backoff_ms).await;
            continue;
        }

        match drive_socket(&mut socket, &mut cmd_rx, &mut subs).await {
            DriveExit::Unauthorized => {
                state.set(ConnectionState::Unauthorized);
                return;
            }
            DriveExit::Reconnect => {
                state.set(ConnectionState::Disconnected);
                sleep_backoff(&mut backoff_ms).await;
            }
            DriveExit::ShuttingDown => return,
        }
    }
}

#[cfg(target_arch = "wasm32")]
enum DriveExit {
    Reconnect,
    Unauthorized,
    ShuttingDown,
}

#[cfg(target_arch = "wasm32")]
async fn drive_socket(
    socket: &mut gloo_net::websocket::futures::WebSocket,
    cmd_rx: &mut mpsc::UnboundedReceiver<HubCommand>,
    subs: &mut HashMap<String, TopicStateInner>,
) -> DriveExit {
    use futures::future::Either;
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;
    use gloo_net::websocket::Message;
    loop {
        let inbound = socket.next();
        let cmd = cmd_rx.next();
        futures::pin_mut!(inbound, cmd);
        match futures::future::select(inbound, cmd).await {
            Either::Left((res, _)) => match res {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(env) = serde_json::from_str::<WsEvent>(&text) {
                        if env.id != 0 {
                            if let Some(st) = subs.get_mut(&env.topic) {
                                st.last_event_id = Some(env.id);
                            }
                        }
                        if let Some(st) = subs.get_mut(&env.topic) {
                            st.sinks.retain(|s| !s.is_closed());
                            for sink in st.sinks.iter() {
                                let _ = sink.unbounded_send(env.clone());
                            }
                        }
                    }
                }
                Some(Ok(Message::Bytes(_))) => {}
                Some(Err(_)) | None => return DriveExit::Reconnect,
            },
            Either::Right((cmd_opt, _)) => {
                let Some(cmd) = cmd_opt else {
                    return DriveExit::ShuttingDown;
                };
                match cmd {
                    HubCommand::Subscribe { topic, sink } => {
                        let entry = subs
                            .entry(topic.clone())
                            .or_insert_with(|| TopicStateInner {
                                last_event_id: None,
                                sinks: Vec::new(),
                            });
                        entry.sinks.push(sink);
                        let frame = OutFrame::Subscribe {
                            topic: topic.as_str(),
                            last_event_id: entry.last_event_id,
                        };
                        if let Ok(j) = serde_json::to_string(&frame) {
                            if socket.send(Message::Text(j)).await.is_err() {
                                return DriveExit::Reconnect;
                            }
                        }
                    }
                    HubCommand::Unsubscribe { topic } => {
                        let still_has_sinks = subs
                            .get_mut(&topic)
                            .map(|st| {
                                st.sinks.retain(|s| !s.is_closed());
                                !st.sinks.is_empty()
                            })
                            .unwrap_or(false);
                        if !still_has_sinks {
                            subs.remove(&topic);
                            let frame = OutFrame::Unsubscribe {
                                topic: topic.as_str(),
                            };
                            if let Ok(j) = serde_json::to_string(&frame) {
                                if socket.send(Message::Text(j)).await.is_err() {
                                    return DriveExit::Reconnect;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn sleep_backoff(backoff_ms: &mut u32) {
    use gloo_timers::future::TimeoutFuture;
    let jitter_pct = pseudo_random_pct();
    let delay = ((*backoff_ms as f32) * jitter_pct).round() as u32;
    TimeoutFuture::new(delay).await;
    *backoff_ms = (*backoff_ms * 2).min(MAX_BACKOFF_MS);
}

#[cfg(target_arch = "wasm32")]
fn pseudo_random_pct() -> f32 {
    let r = js_sys::Math::random() as f32;
    0.75 + r * 0.5
}

#[cfg(target_arch = "wasm32")]
fn build_ws_url(token: &str) -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let proto = location.protocol().ok()?;
    let host = location.host().ok()?;
    let scheme = if proto == "https:" { "wss" } else { "ws" };
    Some(format!(
        "{scheme}://{host}/api/ws?token={}",
        urlencoding::encode(token)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_frame_subscribe_serialises_with_kind_field() {
        let frame = OutFrame::Subscribe {
            topic: "server:1:clients",
            last_event_id: Some(42),
        };
        let s = serde_json::to_string(&frame).unwrap();
        assert!(s.contains(r#""kind":"subscribe""#), "got: {s}");
        assert!(s.contains(r#""topic":"server:1:clients""#), "got: {s}");
        assert!(s.contains(r#""lastEventId":42"#), "got: {s}");
    }

    #[test]
    fn out_frame_subscribe_omits_last_event_id_when_none() {
        let frame = OutFrame::Subscribe {
            topic: "server:1:logs",
            last_event_id: None,
        };
        let s = serde_json::to_string(&frame).unwrap();
        assert!(!s.contains("lastEventId"), "must omit, got: {s}");
    }

    #[test]
    fn ws_event_decodes_envelope_shape() {
        let body = r#"{"id":7,"topic":"server:1:clients","type":"ts:client:kicked_from_server","data":{"clid":12},"ts":1700000000}"#;
        let env: WsEvent = serde_json::from_str(body).unwrap();
        assert_eq!(env.id, 7);
        assert_eq!(env.topic, "server:1:clients");
        assert_eq!(env.kind, "ts:client:kicked_from_server");
        assert_eq!(env.data["clid"], 12);
    }

    #[test]
    fn ws_event_decodes_dropped_envelope() {
        // Server-side `dropped` envelope from session.rs has id = 0 and an
        // empty topic; the decoder must not blow up on those defaults.
        let body = r#"{"type":"dropped","data":{"reason":"send-queue-overflow"}}"#;
        let env: WsEvent = serde_json::from_str(body).unwrap();
        assert_eq!(env.id, 0);
        assert_eq!(env.kind, "dropped");
        assert_eq!(env.data["reason"], "send-queue-overflow");
    }
}
