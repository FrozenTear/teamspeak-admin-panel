//! WS hub shared state — PURA-70.
//!
//! Owns per-server fan-out broadcast channels, the per-server reconnect
//! ring buffer, and the metrics counters. Cloned cheaply into every
//! request handler via [`crate::app_state::AppState`].
//!
//! Each topic resolves at runtime to a per-server `tokio::sync::broadcast`
//! sender. Sessions hold a `broadcast::Receiver` per active subscription
//! and select across them in their main loop. The broadcast channel is
//! intentionally **not** the per-connection bounded queue — that lives
//! inside [`super::session`] and is the slow-consumer-detection point
//! (see the §Backpressure header on [`super::session`]).
//!
//! Metrics are plain `AtomicU64`s. PURA-82 wires
//! [`Metrics::snapshot`] into the admin-JWT-gated `/metrics` route in
//! [`crate::routes::metrics`]; this module only collects.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, broadcast};

use crate::db::Database;
use crate::repos::server_user_grants;

use super::auth::Principal;
use super::envelope::{Envelope, RING_CAPACITY, RingBuffer};
use super::topic::{AuthRequirement, Topic, TopicKind};

/// Per-server broadcast channel capacity. Slow consumers see
/// `RecvError::Lagged` from `broadcast::Receiver::recv()` and the session
/// translates that to a `dropped` envelope plus close.
const BROADCAST_CAPACITY: usize = 128;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<HubInner>,
}

struct HubInner {
    next_event_id: AtomicU64,
    /// Per-server channel + ring buffer. Created lazily on first publish
    /// or first subscribe for a given server id.
    servers: Mutex<HashMap<i64, ServerSlot>>,
    metrics: Metrics,
}

struct ServerSlot {
    sender: broadcast::Sender<Envelope>,
    ring: RingBuffer,
}

#[derive(Default)]
pub struct Metrics {
    pub connections: AtomicU64,
    pub subscribe_ok: AtomicU64,
    pub subscribe_denied: AtomicU64,
    pub events_published: AtomicU64,
    pub events_dropped: AtomicU64,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            connections: self.connections.load(Ordering::Relaxed),
            subscribe_ok: self.subscribe_ok.load(Ordering::Relaxed),
            subscribe_denied: self.subscribe_denied.load(Ordering::Relaxed),
            events_published: self.events_published.load(Ordering::Relaxed),
            events_dropped: self.events_dropped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub connections: u64,
    pub subscribe_ok: u64,
    pub subscribe_denied: u64,
    pub events_published: u64,
    pub events_dropped: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorizeError {
    /// Principal credential type does not match the topic's
    /// [`AuthRequirement`] — e.g. a JWT user trying to subscribe to
    /// `server:N:widget`, or a widget token trying to subscribe to
    /// `server:N:logs`.
    #[error("credential type does not authorise this topic")]
    CredentialMismatch,
    /// Per-topic ACL rejected this principal (no grant, role too low,
    /// widget pointing at a different server, etc.).
    #[error("forbidden")]
    Forbidden,
    /// Backend (DB) failure while running the ACL.
    #[error("backend error")]
    Backend,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HubInner {
                next_event_id: AtomicU64::new(1),
                servers: Mutex::new(HashMap::new()),
                metrics: Metrics::default(),
            }),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    /// Subscribe to `topic` after running the per-topic ACL against
    /// `principal`. Returns the broadcast receiver plus any replay
    /// envelopes (`last_event_id` is `None` on a fresh connection).
    pub async fn subscribe(
        &self,
        db: &Database,
        principal: &Principal,
        topic: Topic,
        last_event_id: Option<u64>,
    ) -> Result<Subscription, AuthorizeError> {
        self.authorize(db, principal, &topic).await?;
        let mut servers = self.inner.servers.lock().await;
        let slot = servers.entry(topic.server_id).or_insert_with(|| ServerSlot {
            sender: broadcast::channel(BROADCAST_CAPACITY).0,
            ring: RingBuffer::new(RING_CAPACITY),
        });
        let receiver = slot.sender.subscribe();
        let replay = match last_event_id {
            Some(id) => slot.ring.replay_for(&topic.to_string(), id),
            None => Vec::new(),
        };
        self.inner.metrics.subscribe_ok.fetch_add(1, Ordering::Relaxed);
        Ok(Subscription { receiver, replay })
    }

    /// Authorise `principal` against `topic`. Pure ACL — no channel
    /// allocation. Exposed independently for unit tests.
    pub async fn authorize(
        &self,
        db: &Database,
        principal: &Principal,
        topic: &Topic,
    ) -> Result<(), AuthorizeError> {
        match (principal, topic.auth_requirement()) {
            (Principal::User(_), AuthRequirement::WidgetToken)
            | (Principal::Widget(_), AuthRequirement::JwtUser) => {
                self.inner
                    .metrics
                    .subscribe_denied
                    .fetch_add(1, Ordering::Relaxed);
                Err(AuthorizeError::CredentialMismatch)
            }
            (Principal::User(u), AuthRequirement::JwtUser) => match topic.kind {
                TopicKind::Logs => {
                    if u.is_admin {
                        Ok(())
                    } else {
                        self.deny()
                    }
                }
                TopicKind::Clients | TopicKind::Channels => {
                    if u.is_admin {
                        Ok(())
                    } else {
                        let granted = server_user_grants::exists(db, u.user_id, topic.server_id)
                            .await
                            .map_err(|_| AuthorizeError::Backend)?;
                        if granted { Ok(()) } else { self.deny() }
                    }
                }
                // Unreachable: covered by the credential-mismatch arm
                // above (Widget kind requires WidgetToken auth), kept
                // explicit so a future TopicKind variant forces this
                // match to be revisited.
                TopicKind::Widget => self.deny(),
            },
            (Principal::Widget(w), AuthRequirement::WidgetToken) => {
                if w.server_config_id == topic.server_id {
                    Ok(())
                } else {
                    self.deny()
                }
            }
        }
    }

    fn deny(&self) -> Result<(), AuthorizeError> {
        self.inner
            .metrics
            .subscribe_denied
            .fetch_add(1, Ordering::Relaxed);
        Err(AuthorizeError::Forbidden)
    }

    /// Publish `data` on `topic`. Stamps a hub-global event id, pushes
    /// into the ring buffer, and fans out via the per-server broadcast.
    /// No-op (returns the stamped envelope but doesn't fan out) if no
    /// subscribers exist — broadcast::Sender::send returns Err in that
    /// case, which is fine.
    pub async fn publish(&self, topic: Topic, kind: impl Into<String>, data: serde_json::Value) -> Envelope {
        let id = self.inner.next_event_id.fetch_add(1, Ordering::Relaxed);
        let ts = chrono::Utc::now().timestamp_millis();
        let env = Envelope::new(id, &topic, kind, data, ts);
        let mut servers = self.inner.servers.lock().await;
        let slot = servers.entry(topic.server_id).or_insert_with(|| ServerSlot {
            sender: broadcast::channel(BROADCAST_CAPACITY).0,
            ring: RingBuffer::new(RING_CAPACITY),
        });
        slot.ring.push(env.clone());
        // Best-effort fan-out. `send` errors when there are zero active
        // receivers — that's fine, the ring buffer captures the event
        // for any reconnect that lands within the window.
        let _ = slot.sender.send(env.clone());
        self.inner
            .metrics
            .events_published
            .fetch_add(1, Ordering::Relaxed);
        env
    }

    pub fn record_dropped(&self) {
        self.inner.metrics.events_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_connection_open(&self) {
        self.inner.metrics.connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_connection_close(&self) {
        // Saturating decrement — we never expose this counter as "current
        // connections", only as "lifetime opens" so a strict decrement
        // is unnecessary. Subtract anyway for an authoritative live
        // count if we wire it into /metrics later.
        self.inner
            .metrics
            .connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Subscription {
    pub receiver: broadcast::Receiver<Envelope>,
    pub replay: Vec<Envelope>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{server_user_grants, users};
    use crate::ws::auth::{UserPrincipal, WidgetPrincipal};
    use serde_json::json;

    async fn fresh_db() -> Arc<Database> {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        db
    }

    async fn seed_user(db: &Database, role: &str) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
            db,
            users::NewUser {
                username: "alice".into(),
                passwordHash: hash,
                displayName: "Alice".into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn user_principal(id: i64, role: &str) -> Principal {
        Principal::User(UserPrincipal {
            user_id: id,
            username: "alice".into(),
            role: role.into(),
            is_admin: role == "admin",
            is_at_least_moderator: role == "admin" || role == "moderator",
        })
    }

    fn widget_principal(server_config_id: i64) -> Principal {
        Principal::Widget(WidgetPrincipal {
            widget_id: 1,
            server_config_id,
            virtual_server_id: 1,
        })
    }

    #[tokio::test]
    async fn admin_subscribes_to_any_server_topic() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        for kind in [TopicKind::Clients, TopicKind::Channels, TopicKind::Logs] {
            hub.authorize(&db, &admin, &Topic::new(99, kind))
                .await
                .expect("admin must be allowed");
        }
    }

    #[tokio::test]
    async fn viewer_without_grant_denied_clients() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let uid = seed_user(&db, "viewer").await;
        let p = user_principal(uid, "viewer");
        let err = hub
            .authorize(&db, &p, &Topic::new(7, TopicKind::Clients))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthorizeError::Forbidden));
    }

    #[tokio::test]
    async fn viewer_with_grant_allowed_clients_but_not_logs() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let uid = seed_user(&db, "viewer").await;
        server_user_grants::insert(&db, uid, 7).await.unwrap();
        let p = user_principal(uid, "viewer");
        hub.authorize(&db, &p, &Topic::new(7, TopicKind::Clients))
            .await
            .expect("granted viewer can subscribe to clients");
        hub.authorize(&db, &p, &Topic::new(7, TopicKind::Channels))
            .await
            .expect("granted viewer can subscribe to channels");
        let err = hub
            .authorize(&db, &p, &Topic::new(7, TopicKind::Logs))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthorizeError::Forbidden),
            "logs is admin-only"
        );
    }

    #[tokio::test]
    async fn widget_token_only_for_matching_server() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let p = widget_principal(5);
        hub.authorize(&db, &p, &Topic::new(5, TopicKind::Widget))
            .await
            .expect("widget can subscribe to its own server");
        let err = hub
            .authorize(&db, &p, &Topic::new(6, TopicKind::Widget))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthorizeError::Forbidden));
    }

    #[tokio::test]
    async fn jwt_user_cannot_subscribe_to_widget_topic() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        let err = hub
            .authorize(&db, &admin, &Topic::new(1, TopicKind::Widget))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthorizeError::CredentialMismatch));
    }

    #[tokio::test]
    async fn widget_cannot_subscribe_to_jwt_topics() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let p = widget_principal(5);
        for kind in [TopicKind::Clients, TopicKind::Channels, TopicKind::Logs] {
            let err = hub
                .authorize(&db, &p, &Topic::new(5, kind))
                .await
                .unwrap_err();
            assert!(matches!(err, AuthorizeError::CredentialMismatch));
        }
    }

    #[tokio::test]
    async fn publish_lands_in_ring_and_replays_on_subscribe() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        let topic = Topic::new(1, TopicKind::Clients);
        let e1 = hub.publish(topic, "ts:client:connected", json!({"clid": 1})).await;
        let e2 = hub.publish(topic, "ts:client:connected", json!({"clid": 2})).await;
        assert_eq!(e2.id, e1.id + 1, "ids are monotonic");

        let sub = hub
            .subscribe(&db, &admin, topic, Some(0))
            .await
            .unwrap();
        let ids: Vec<u64> = sub.replay.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![e1.id, e2.id]);
    }

    #[tokio::test]
    async fn subscribe_fresh_returns_empty_replay() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        let topic = Topic::new(1, TopicKind::Clients);
        hub.publish(topic, "x", json!({})).await;
        let sub = hub.subscribe(&db, &admin, topic, None).await.unwrap();
        assert!(sub.replay.is_empty(), "no last_event_id ⇒ no replay");
    }

    #[tokio::test]
    async fn published_event_reaches_live_subscriber() {
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        let topic = Topic::new(1, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin, topic, None).await.unwrap();
        let e = hub.publish(topic, "ts:client:connected", json!({"clid": 5})).await;
        let received = sub.receiver.recv().await.expect("must receive");
        assert_eq!(received.id, e.id);
        assert_eq!(received.kind, "ts:client:connected");
    }

    #[tokio::test]
    async fn slow_subscriber_sees_lagged_when_overflowing_broadcast() {
        // The broadcast channel has BROADCAST_CAPACITY = 128. Publishing
        // 200 events without draining the receiver MUST eventually emit
        // `RecvError::Lagged` on the next `recv` call. The session then
        // translates that into a `dropped` envelope + close.
        let db = fresh_db().await;
        let hub = Hub::new();
        let admin = user_principal(1, "admin");
        let topic = Topic::new(1, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin, topic, None).await.unwrap();
        for _ in 0..(BROADCAST_CAPACITY as u64 + 50) {
            hub.publish(topic, "x", json!({})).await;
        }
        let mut saw_lagged = false;
        for _ in 0..(BROADCAST_CAPACITY as u64 + 50) {
            match sub.receiver.try_recv() {
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    saw_lagged = true;
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                _ => continue,
            }
        }
        assert!(saw_lagged, "slow consumer must see Lagged");
    }
}
