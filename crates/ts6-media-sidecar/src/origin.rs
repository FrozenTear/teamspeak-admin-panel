//! Broadcast registry — the API surface WS-2 plugs into.
//!
//! Wraps a single [`moq_lite::OriginProducer`] and remembers which
//! namespaces we've published. The producer side (FFmpeg → IVF/Ogg →
//! moq-lite frames) lands in WS-2; this scaffold only exposes the
//! `register_broadcast` / `unregister_broadcast` shape.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use moq_lite::{Broadcast, BroadcastProducer, Origin, OriginConsumer, OriginProducer};
use tokio::sync::RwLock;

/// In-memory registry of named broadcasts published into the sidecar's
/// single MoQ origin. Cheap-clonable handles via `Arc<SidecarOrigin>`.
pub struct SidecarOrigin {
    producer: OriginProducer,
    broadcasts: RwLock<HashMap<String, BroadcastProducer>>,
}

impl SidecarOrigin {
    /// Build an empty registry behind a fresh randomly-id'd origin.
    pub fn new() -> Self {
        Self {
            producer: Origin::random().produce(),
            broadcasts: RwLock::new(HashMap::new()),
        }
    }

    /// Hand a consumer to `moq-native::Server::with_publish` so any session
    /// the relay accepts can subscribe to the broadcasts we register here.
    pub fn consumer(&self) -> OriginConsumer {
        self.producer.consume()
    }

    /// Number of broadcasts currently registered.
    pub async fn len(&self) -> usize {
        self.broadcasts.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.broadcasts.read().await.is_empty()
    }

    /// Snapshot of registered broadcast names. Order is unspecified.
    pub async fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.broadcasts.read().await.keys().cloned().collect();
        names.sort();
        names
    }

    /// Register a broadcast under `name`. Returns the [`BroadcastProducer`]
    /// the caller writes media frames into. Errors if `name` already
    /// exists or the publish is refused by the underlying origin (e.g.
    /// scope/path violation).
    ///
    /// WS-2 owns what gets written to the returned producer; this scaffold
    /// only validates the registration path.
    pub async fn register_broadcast(&self, name: &str) -> Result<BroadcastProducer> {
        let mut broadcasts = self.broadcasts.write().await;
        if broadcasts.contains_key(name) {
            return Err(anyhow!("broadcast '{name}' already registered"));
        }

        let producer = Broadcast::new().produce();
        let published = self.producer.publish_broadcast(name, producer.consume());
        if !published {
            return Err(anyhow!("origin refused to publish '{name}'"));
        }

        broadcasts.insert(name.to_string(), producer.clone());
        Ok(producer)
    }

    /// Drop the registered broadcast. The MoQ origin auto-unannounces
    /// once the [`BroadcastProducer`] is dropped, so this just clears the
    /// registry entry. Returns Err if `name` was not registered.
    pub async fn unregister_broadcast(&self, name: &str) -> Result<()> {
        let mut broadcasts = self.broadcasts.write().await;
        broadcasts
            .remove(name)
            .map(|_| ())
            .with_context(|| format!("broadcast '{name}' not registered"))
    }
}

impl Default for SidecarOrigin {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_unregister_round_trips() {
        let origin = SidecarOrigin::new();
        assert_eq!(origin.len().await, 0);

        let _producer = origin.register_broadcast("camera-1").await.unwrap();
        assert_eq!(origin.len().await, 1);
        assert_eq!(origin.names().await, vec!["camera-1".to_string()]);

        origin.unregister_broadcast("camera-1").await.unwrap();
        assert_eq!(origin.len().await, 0);
    }

    #[tokio::test]
    async fn register_rejects_duplicates() {
        let origin = SidecarOrigin::new();
        let _producer = origin.register_broadcast("camera-1").await.unwrap();
        let err = origin
            .register_broadcast("camera-1")
            .await
            .err()
            .expect("duplicate must error");
        assert!(err.to_string().contains("already registered"));
    }

    #[tokio::test]
    async fn unregister_unknown_errors() {
        let origin = SidecarOrigin::new();
        let err = origin
            .unregister_broadcast("ghost")
            .await
            .expect_err("unknown must error");
        assert!(err.to_string().contains("not registered"));
    }
}
