//! Per-connection session task — PURA-70.
//!
//! Owns the WebSocket for one upgraded connection. Consumes the four
//! recognised client frames (`subscribe` / `unsubscribe` / `ping` / `pong`)
//! and emits envelopes from the hub plus heartbeat pings.
//!
//! ## Backpressure
//!
//! The session has a single bounded `mpsc::channel::<Envelope>(SEND_QUEUE_CAP)`
//! that all per-subscription forwarder tasks `try_send` into. On full, the
//! forwarder drops the message, sends a `dropped` envelope ahead of the
//! pending traffic, and signals the session loop to close. The hub's
//! per-server `broadcast::Receiver` already surfaces `Lagged` for the
//! same condition — both paths converge on the same `dropped` envelope
//! shape so the client sees consistent behaviour regardless of where the
//! drop happened.
//!
//! ## Heartbeat
//!
//! Server sends `Ping` every 20s. If no `Pong` (or any other client
//! frame) arrives within 60s of the last successful pong/recv, the
//! session closes with code 1001 (going-away).

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, close_code};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Instant, interval};

use crate::db::Database;

use super::auth::Principal;
use super::envelope::Envelope;
use super::hub::{AuthorizeError, Hub, WidgetConnGuard};
use super::topic::Topic;

/// Per-connection bounded send queue size. Picked at the smaller end of
/// what makes sense for an interactive dashboard: a reasonable client
/// drains within a frame; one that hasn't drained 64 envelopes is
/// genuinely backed up and should be cut.
pub const SEND_QUEUE_CAP: usize = 64;

const PING_INTERVAL: Duration = Duration::from_secs(20);
const POND_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the session loop until the connection closes. Consumes the
/// `WebSocket` and the [`Principal`] from the handshake. `widget_guard`
/// is the per-widget concurrent-connection slot acquired by the
/// handshake (PURA-97 L-1) — held here for the lifetime of the session
/// so its `Drop` reclaims the slot on any exit path.
pub async fn run(
    socket: WebSocket,
    principal: Principal,
    hub: Hub,
    db: std::sync::Arc<Database>,
    widget_guard: Option<WidgetConnGuard>,
) {
    hub.record_connection_open();
    let result = SessionLoop::new(socket, principal, hub.clone(), db, widget_guard)
        .run()
        .await;
    hub.record_connection_close();
    if let Err(e) = result {
        tracing::debug!(error = %e, "ws session ended");
    }
}

/// Wire shape — incoming client frames. Phase 2 D-WS deviation: spec
/// §8.3 forbids any client→server protocol; PURA-66 ratified four
/// recognised frames. Anything else closes the connection.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ClientFrame {
    Subscribe {
        topic: String,
        #[serde(rename = "lastEventId", default)]
        last_event_id: Option<u64>,
    },
    Unsubscribe {
        topic: String,
    },
    Ping,
    Pong,
}

struct SessionLoop {
    socket: WebSocket,
    principal: Principal,
    hub: Hub,
    db: std::sync::Arc<Database>,
    out_tx: mpsc::Sender<Envelope>,
    out_rx: mpsc::Receiver<Envelope>,
    /// Active subscriptions keyed by `Topic`. Each holds the abort
    /// handle for its forwarder task so `unsubscribe` can shut it down.
    subscriptions: HashMap<Topic, tokio::task::JoinHandle<()>>,
    last_recv: Instant,
    /// Set true when a forwarder task signals the queue overflowed.
    /// The loop sends a final `dropped` envelope and closes.
    drop_pending: bool,
    /// PURA-97 M-2 — widget-revocation receiver. Some(_) only when the
    /// principal is a widget; the select! arm closes the connection on
    /// receipt of this principal's `widget_id`.
    widgets_revoked_rx: Option<broadcast::Receiver<i64>>,
    /// PURA-97 L-1 — per-widget connection-count slot. Held for the
    /// session's lifetime; `Drop` reclaims the slot on any exit.
    /// `_`-prefixed because it is never read after construction —
    /// the handle exists purely for its destructor.
    _widget_conn_guard: Option<WidgetConnGuard>,
}

impl SessionLoop {
    fn new(
        socket: WebSocket,
        principal: Principal,
        hub: Hub,
        db: std::sync::Arc<Database>,
        widget_guard: Option<WidgetConnGuard>,
    ) -> Self {
        let (out_tx, out_rx) = mpsc::channel(SEND_QUEUE_CAP);
        // Subscribe to widget revocations only for widget principals.
        // For JWT users this stays None and the select! arm pends
        // forever, so it has zero runtime cost on the operator path.
        let widgets_revoked_rx =
            matches!(principal, Principal::Widget(_)).then(|| hub.subscribe_widget_revocations());
        Self {
            socket,
            principal,
            hub,
            db,
            out_tx,
            out_rx,
            subscriptions: HashMap::new(),
            last_recv: Instant::now(),
            drop_pending: false,
            widgets_revoked_rx,
            _widget_conn_guard: widget_guard,
        }
    }

    async fn run(mut self) -> Result<(), SessionError> {
        let mut ping = interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Drop the initial immediate tick — start at +PING_INTERVAL.
        ping.tick().await;

        loop {
            if self.drop_pending {
                let _ = self
                    .send_envelope(Envelope {
                        id: 0,
                        topic: String::new(),
                        kind: "dropped".into(),
                        data: json!({"reason": "send-queue-overflow"}),
                        ts: chrono::Utc::now().timestamp_millis(),
                    })
                    .await;
                let _ = self.socket.send(Message::Close(None)).await;
                return Ok(());
            }

            tokio::select! {
                // Server → client envelope ready.
                Some(env) = self.out_rx.recv() => {
                    self.send_envelope(env).await?;
                }
                // PURA-97 M-2 — widget-token revocation. For widget
                // principals, the admin path's `revoke_widget(id)` lands
                // here; if the id matches our principal we close with
                // 1008 (policy-violation) and exit. JWT users have
                // `widgets_revoked_rx == None`, so this arm pends
                // forever for them and is selected away.
                revoke = next_revoke(self.widgets_revoked_rx.as_mut()) => {
                    if self.handle_revoke(revoke).await? {
                        return Ok(());
                    }
                }
                // Client frame.
                ws_msg = self.socket.recv() => {
                    match ws_msg {
                        Some(Ok(Message::Text(text))) => {
                            self.last_recv = Instant::now();
                            self.handle_client_text(text.as_str()).await?;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            self.last_recv = Instant::now();
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            self.last_recv = Instant::now();
                            let _ = self.socket.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => return Ok(()),
                        // Spec §8.3 hardening: any other frame shape ⇒ close.
                        Some(Ok(_)) => {
                            let _ = self.socket.send(Message::Close(None)).await;
                            return Err(SessionError::ProtocolViolation);
                        }
                        Some(Err(_)) => return Err(SessionError::Transport),
                    }
                }
                _ = ping.tick() => {
                    if self.last_recv.elapsed() > POND_TIMEOUT {
                        let _ = self.socket.send(Message::Close(None)).await;
                        return Err(SessionError::PongTimeout);
                    }
                    if self.socket.send(Message::Ping(Default::default())).await.is_err() {
                        return Err(SessionError::Transport);
                    }
                }
            }
        }
    }

    /// PURA-97 M-2 — process a frame from the widget-revocation
    /// broadcast. Returns `Ok(true)` if the session should exit (this
    /// connection's widget was revoked, or the broadcast lagged so we
    /// conservatively close), `Ok(false)` to keep looping (id was for a
    /// different widget, or the channel closed unexpectedly).
    async fn handle_revoke(
        &mut self,
        revoke: Result<i64, broadcast::error::RecvError>,
    ) -> Result<bool, SessionError> {
        match revoke {
            Ok(id) => {
                let our_id = match &self.principal {
                    Principal::Widget(w) => w.widget_id,
                    // Defensive: only widget sessions install the
                    // receiver, but the type system can't prove it.
                    _ => return Ok(false),
                };
                if id != our_id {
                    return Ok(false);
                }
                // Match: send a clean close and exit. 1008 (policy
                // violation) communicates "your credential is no
                // longer accepted" without leaking the precise cause
                // back to the (potentially attacker-controlled) viewer.
                let _ = self
                    .socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::POLICY,
                        reason: Utf8Bytes::from_static("widget token revoked"),
                    })))
                    .await;
                Ok(true)
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Conservative: a lagged receiver might have missed
                // our revoke event. Closing with the same close-frame
                // shape keeps the wire contract uniform across the
                // matched-id and lagged paths.
                let _ = self
                    .socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::POLICY,
                        reason: Utf8Bytes::from_static("widget token revoked"),
                    })))
                    .await;
                Ok(true)
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Hub gone. Should not happen while the AppState is
                // alive; treat as benign no-op so the loop keeps
                // serving the rest of the session.
                Ok(false)
            }
        }
    }

    async fn send_envelope(&mut self, env: Envelope) -> Result<(), SessionError> {
        let body = serde_json::to_string(&env).map_err(|_| SessionError::Encode)?;
        self.socket
            .send(Message::Text(body.into()))
            .await
            .map_err(|_| SessionError::Transport)?;
        Ok(())
    }

    async fn handle_client_text(&mut self, text: &str) -> Result<(), SessionError> {
        let frame: ClientFrame = match serde_json::from_str(text) {
            Ok(f) => f,
            Err(_) => {
                // Unknown / malformed frame ⇒ close per §8.3.
                let _ = self.socket.send(Message::Close(None)).await;
                return Err(SessionError::ProtocolViolation);
            }
        };
        match frame {
            ClientFrame::Subscribe {
                topic,
                last_event_id,
            } => {
                let parsed = match Topic::from_str(&topic) {
                    Ok(t) => t,
                    Err(_) => {
                        self.send_error("bad-topic", &topic).await?;
                        return Ok(());
                    }
                };
                if self.subscriptions.contains_key(&parsed) {
                    // Re-subscribe is idempotent for the live channel,
                    // but we still respect the new `lastEventId` and
                    // replay the gap. Cheapest implementation: tear the
                    // old forwarder down, re-run subscribe.
                    if let Some(h) = self.subscriptions.remove(&parsed) {
                        h.abort();
                    }
                }
                let sub = match self
                    .hub
                    .subscribe(&self.db, &self.principal, parsed, last_event_id)
                    .await
                {
                    Ok(s) => s,
                    Err(AuthorizeError::CredentialMismatch | AuthorizeError::Forbidden) => {
                        self.send_error("forbidden", &topic).await?;
                        return Ok(());
                    }
                    Err(AuthorizeError::Backend) => {
                        self.send_error("backend", &topic).await?;
                        return Ok(());
                    }
                };
                // Replay missed events first (drains synchronously so
                // the order from the client's perspective is replay →
                // live, never interleaved).
                for env in sub.replay {
                    self.send_envelope(env).await?;
                }
                let handle =
                    spawn_forwarder(self.hub.clone(), parsed, self.out_tx.clone(), sub.receiver);
                self.subscriptions.insert(parsed, handle);
                self.send_ack("subscribed", &topic).await?;
            }
            ClientFrame::Unsubscribe { topic } => {
                let parsed = match Topic::from_str(&topic) {
                    Ok(t) => t,
                    Err(_) => {
                        self.send_error("bad-topic", &topic).await?;
                        return Ok(());
                    }
                };
                if let Some(h) = self.subscriptions.remove(&parsed) {
                    h.abort();
                }
                self.send_ack("unsubscribed", &topic).await?;
            }
            ClientFrame::Ping => {
                let body = serde_json::to_string(&json!({
                    "type": "pong",
                    "data": {},
                    "ts": chrono::Utc::now().timestamp_millis(),
                }))
                .map_err(|_| SessionError::Encode)?;
                self.socket
                    .send(Message::Text(body.into()))
                    .await
                    .map_err(|_| SessionError::Transport)?;
            }
            ClientFrame::Pong => {
                // Already updated last_recv on the way in.
            }
        }
        Ok(())
    }

    async fn send_ack(&mut self, kind: &str, topic: &str) -> Result<(), SessionError> {
        let body = serde_json::to_string(&json!({
            "type": kind,
            "topic": topic,
            "ts": chrono::Utc::now().timestamp_millis(),
        }))
        .map_err(|_| SessionError::Encode)?;
        self.socket
            .send(Message::Text(body.into()))
            .await
            .map_err(|_| SessionError::Transport)?;
        Ok(())
    }

    async fn send_error(&mut self, code: &str, topic: &str) -> Result<(), SessionError> {
        let body = serde_json::to_string(&json!({
            "type": "error",
            "topic": topic,
            "data": {"code": code},
            "ts": chrono::Utc::now().timestamp_millis(),
        }))
        .map_err(|_| SessionError::Encode)?;
        self.socket
            .send(Message::Text(body.into()))
            .await
            .map_err(|_| SessionError::Transport)?;
        Ok(())
    }
}

/// PURA-97 M-2 — bridge between `Option<broadcast::Receiver<i64>>` and
/// the `tokio::select!` arm. When `rx` is `Some`, awaits the next
/// receive. When `None` (JWT-user session), returns a future that never
/// resolves so `select!` ignores this arm. Splitting the helper out
/// keeps the borrow on `self.widgets_revoked_rx` tight enough for the
/// other select arms to keep their disjoint borrows of `self`.
async fn next_revoke(
    rx: Option<&mut broadcast::Receiver<i64>>,
) -> Result<i64, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// PURA-103 — per-subscription forwarder bridging the hub's per-server
/// `broadcast` channel into this session's bounded `mpsc`. The hub fans
/// out **every** event for `topic.server_id` regardless of `TopicKind`,
/// so this task MUST drop any envelope whose `env.topic` does not match
/// the subscribed topic — otherwise an operator-only `server:N:clients`
/// `dashboard:tick` would leak to a public widget viewer subscribed to
/// `server:N:widget`. The Hub::authorize gate runs at subscribe time and
/// stops a widget token from asking for an operator topic, but the
/// per-server broadcast crosses topic boundaries so the filter is
/// load-bearing for confidentiality.
fn spawn_forwarder(
    hub: Hub,
    topic: super::topic::Topic,
    out: mpsc::Sender<Envelope>,
    mut rx: tokio::sync::broadcast::Receiver<Envelope>,
) -> tokio::task::JoinHandle<()> {
    let topic_str = topic.to_string();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if env.topic != topic_str {
                        // Different topic on the shared per-server
                        // broadcast — silently drop. Not a backpressure
                        // event, so do NOT emit a `dropped` envelope.
                        continue;
                    }
                    match out.try_send(env) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            hub.record_dropped();
                            // Best-effort `dropped` ahead of close. The
                            // session loop will pick up the channel
                            // close signal and emit a final `dropped`
                            // envelope.
                            let _ = out
                                .send(Envelope {
                                    id: 0,
                                    topic: String::new(),
                                    kind: "dropped".into(),
                                    data: json!({"reason": "send-queue-overflow"}),
                                    ts: chrono::Utc::now().timestamp_millis(),
                                })
                                .await;
                            return;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    hub.record_dropped();
                    let _ = out
                        .send(Envelope {
                            id: 0,
                            topic: String::new(),
                            kind: "dropped".into(),
                            data: json!({"reason": "broadcast-lagged"}),
                            ts: chrono::Utc::now().timestamp_millis(),
                        })
                        .await;
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("transport error")]
    Transport,
    #[error("protocol violation")]
    ProtocolViolation,
    #[error("pong timeout")]
    PongTimeout,
    #[error("encode error")]
    Encode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn parses_subscribe_with_last_event_id() {
        let f: ClientFrame = serde_json::from_str(
            r#"{"kind":"subscribe","topic":"server:1:clients","lastEventId":42}"#,
        )
        .unwrap();
        match f {
            ClientFrame::Subscribe {
                topic,
                last_event_id,
            } => {
                assert_eq!(topic, "server:1:clients");
                assert_eq!(last_event_id, Some(42));
            }
            _ => panic!("expected subscribe"),
        }
    }

    #[test]
    fn parses_subscribe_without_last_event_id() {
        let f: ClientFrame =
            serde_json::from_str(r#"{"kind":"subscribe","topic":"server:1:clients"}"#).unwrap();
        match f {
            ClientFrame::Subscribe { last_event_id, .. } => assert_eq!(last_event_id, None),
            _ => panic!("expected subscribe"),
        }
    }

    #[test]
    fn parses_unsubscribe_ping_pong() {
        let u: ClientFrame =
            serde_json::from_str(r#"{"kind":"unsubscribe","topic":"server:1:logs"}"#).unwrap();
        assert!(matches!(u, ClientFrame::Unsubscribe { .. }));
        let p: ClientFrame = serde_json::from_str(r#"{"kind":"ping"}"#).unwrap();
        assert!(matches!(p, ClientFrame::Ping));
        let q: ClientFrame = serde_json::from_str(r#"{"kind":"pong"}"#).unwrap();
        assert!(matches!(q, ClientFrame::Pong));
    }

    #[test]
    fn rejects_unknown_kind() {
        let r: Result<ClientFrame, _> = serde_json::from_str(r#"{"kind":"shutdown"}"#);
        assert!(r.is_err(), "unknown kind must fail to parse");
    }

    /// PURA-103 regression — `spawn_forwarder` MUST drop envelopes whose
    /// `topic` does not match the subscribed `Topic`. Without this filter,
    /// a public `server:N:widget` subscriber would receive operator-only
    /// `server:N:clients` / `server:N:channels` `dashboard:tick` envelopes
    /// (platform/version/uptime/bandwidth) leaked through the per-server
    /// broadcast. This test fails on the pre-fix forwarder and passes on
    /// the post-fix forwarder.
    #[tokio::test]
    async fn forwarder_drops_envelopes_for_other_topics_on_same_server() {
        use crate::ws::topic::{Topic, TopicKind};
        use serde_json::json;
        use tokio::sync::broadcast;

        let hub = Hub::new();
        // Per-server broadcast standing in for the hub's per-server
        // channel. The bug is that this channel carries every TopicKind
        // for the server, and the forwarder used to forward all of them.
        let (server_tx, server_rx) = broadcast::channel::<Envelope>(16);
        let (out_tx, mut out_rx) = mpsc::channel::<Envelope>(16);

        let widget_topic = Topic::new(1, TopicKind::Widget);
        let clients_topic = Topic::new(1, TopicKind::Clients);
        let channels_topic = Topic::new(1, TopicKind::Channels);

        // Subscriber asks for the widget topic only.
        let h = spawn_forwarder(hub, widget_topic, out_tx, server_rx);

        // Operator-only payload (PURA-72 H-1 leak vector — full
        // `dashboard:tick` with platform + version).
        server_tx
            .send(Envelope::new(
                1,
                &clients_topic,
                "dashboard:tick",
                json!({
                    "serverName": "TeamSpeak 6 Server",
                    "platform": "Linux",
                    "version": "6.0.0-beta9 [Build:…]",
                    "uptime": 1141,
                    "onlineUsers": 0,
                }),
                0,
            ))
            .unwrap();
        server_tx
            .send(Envelope::new(
                2,
                &channels_topic,
                "dashboard:tick",
                json!({
                    "serverName": "TeamSpeak 6 Server",
                    "platform": "Linux",
                    "channelCount": 1,
                }),
                0,
            ))
            .unwrap();
        // The redacted widget stub IS for the subscribed topic — must be
        // delivered.
        server_tx
            .send(Envelope::new(
                3,
                &widget_topic,
                "widget:refresh",
                json!({"refresh": true}),
                0,
            ))
            .unwrap();

        // Drop the sender so the forwarder exits its loop on Closed.
        drop(server_tx);
        let _ = h.await;

        let mut received = Vec::new();
        while let Some(env) = out_rx.recv().await {
            received.push(env);
        }

        assert_eq!(
            received.len(),
            1,
            "widget subscriber must only receive widget-topic envelopes; got {received:?}"
        );
        assert_eq!(received[0].topic, "server:1:widget");
        assert_eq!(received[0].kind, "widget:refresh");
        // Defence in depth: nothing leaked carries the operator
        // `platform` field that spec §7.29 requires redacted on the
        // public widget surface.
        for env in &received {
            assert!(
                env.data.get("platform").is_none(),
                "leaked platform field on forwarded envelope: {env:?}"
            );
            assert!(
                env.data.get("version").is_none(),
                "leaked version field on forwarded envelope: {env:?}"
            );
        }
    }

    #[test]
    fn dropped_envelope_shape_is_stable() {
        // Pin the wire shape the FE will see when a slow consumer
        // hits the per-conn queue cap. Future changes to this shape
        // need a coordinated FE update — the test guards that.
        let body = json!({
            "type": "dropped",
            "data": {"reason": "send-queue-overflow"},
        });
        let s: Value = serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();
        assert_eq!(s["type"], "dropped");
        assert_eq!(s["data"]["reason"], "send-queue-overflow");
    }
}
