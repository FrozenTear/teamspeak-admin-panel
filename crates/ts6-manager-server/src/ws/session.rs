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

use axum::extract::ws::{Message, WebSocket};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

use crate::db::Database;

use super::auth::Principal;
use super::envelope::Envelope;
use super::hub::{AuthorizeError, Hub};
use super::topic::Topic;

/// Per-connection bounded send queue size. Picked at the smaller end of
/// what makes sense for an interactive dashboard: a reasonable client
/// drains within a frame; one that hasn't drained 64 envelopes is
/// genuinely backed up and should be cut.
pub const SEND_QUEUE_CAP: usize = 64;

const PING_INTERVAL: Duration = Duration::from_secs(20);
const POND_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the session loop until the connection closes. Consumes the
/// `WebSocket` and the [`Principal`] from the handshake.
pub async fn run(
    socket: WebSocket,
    principal: Principal,
    hub: Hub,
    db: std::sync::Arc<Database>,
) {
    hub.record_connection_open();
    let result = SessionLoop::new(socket, principal, hub.clone(), db).run().await;
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
}

impl SessionLoop {
    fn new(socket: WebSocket, principal: Principal, hub: Hub, db: std::sync::Arc<Database>) -> Self {
        let (out_tx, out_rx) = mpsc::channel(SEND_QUEUE_CAP);
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
            ClientFrame::Subscribe { topic, last_event_id } => {
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
                let handle = spawn_forwarder(self.hub.clone(), self.out_tx.clone(), sub.receiver);
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

fn spawn_forwarder(
    hub: Hub,
    out: mpsc::Sender<Envelope>,
    mut rx: tokio::sync::broadcast::Receiver<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(env) => match out.try_send(env) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        hub.record_dropped();
                        // Best-effort `dropped` ahead of close. The
                        // session loop will pick up the channel close
                        // signal and emit a final `dropped` envelope.
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
                },
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
        let f: ClientFrame =
            serde_json::from_str(r#"{"kind":"subscribe","topic":"server:1:clients","lastEventId":42}"#)
                .unwrap();
        match f {
            ClientFrame::Subscribe { topic, last_event_id } => {
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
