//! Token-keyed 45 s widget data cache (spec §7.29).
//!
//! Public widget data is cached for **exactly 45 seconds** keyed by widget
//! token. The cache is invalidated on `PATCH /widgets/:id`,
//! `DELETE /widgets/:id`, and `POST /widgets/:id/regenerate-token`. The
//! response carries `Cache-Control: public, max-age=45` for downstream CDNs.
//!
//! Implementation choices:
//!
//! - Single in-memory `HashMap<String, Entry>` behind a `tokio::sync::Mutex`.
//!   Concurrency model: cache reads are O(1) and rare contention is fine
//!   for a public-widget surface.
//! - Token strings are owned `String`s (not `&'static`), since they are
//!   read from DB rows.
//! - On a cache miss the **caller** drives the upstream fetch and writes
//!   back via [`WidgetCache::insert`]. The cache itself never blocks on
//!   network I/O — keeps the lock-hold time tiny.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use ts6_manager_shared::widgets::WidgetData;

/// Spec §7.29: 45 s public-widget data TTL.
pub const CACHE_TTL: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub struct WidgetCache {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Clone)]
struct Entry {
    data: WidgetData,
    expires_at: Instant,
}

impl WidgetCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return the cached `WidgetData` if the entry exists and has not
    /// expired. Expired entries are evicted on lookup.
    pub async fn get(&self, token: &str) -> Option<WidgetData> {
        let mut guard = self.inner.lock().await;
        match guard.get(token) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.data.clone()),
            Some(_) => {
                guard.remove(token);
                None
            }
            None => None,
        }
    }

    /// Store `data` under `token` for [`CACHE_TTL`] from now.
    pub async fn insert(&self, token: String, data: WidgetData) {
        let mut guard = self.inner.lock().await;
        guard.insert(
            token,
            Entry {
                data,
                expires_at: Instant::now() + CACHE_TTL,
            },
        );
    }

    /// Drop the cached entry for `token`. Idempotent.
    pub async fn invalidate(&self, token: &str) {
        self.inner.lock().await.remove(token);
    }

    /// Test/debug helper — number of live (post-eviction) entries.
    #[cfg(test)]
    pub async fn len(&self) -> usize {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|_, e| e.expires_at > now);
        guard.len()
    }
}

impl Default for WidgetCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WidgetCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WidgetCache").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts6_manager_shared::widgets::{WidgetData, WidgetServer};

    fn fixture() -> WidgetData {
        WidgetData {
            name: "Test".into(),
            theme: "dark".into(),
            server_config_id: 1,
            show_channel_tree: true,
            show_clients: true,
            hide_empty_channels: false,
            max_channel_depth: 5,
            server: WidgetServer {
                name: "TS".into(),
                clients_online: 0,
                max_clients: 32,
                uptime_seconds: 0,
                platform: "TeamSpeak".into(),
                version: String::new(),
            },
            channels: Vec::new(),
        }
    }

    #[tokio::test]
    async fn miss_then_hit() {
        let cache = WidgetCache::new();
        assert!(cache.get("tok").await.is_none());
        cache.insert("tok".into(), fixture()).await;
        let got = cache.get("tok").await.expect("hit");
        assert_eq!(got.server.name, "TS");
    }

    #[tokio::test]
    async fn invalidate_drops_entry() {
        let cache = WidgetCache::new();
        cache.insert("tok".into(), fixture()).await;
        cache.invalidate("tok").await;
        assert!(cache.get("tok").await.is_none());
    }

    #[tokio::test]
    async fn expired_entry_is_evicted_on_lookup() {
        let cache = WidgetCache::new();
        // Bypass the public API to seed an already-expired entry without
        // sleeping for the full TTL in the test.
        {
            let mut guard = cache.inner.lock().await;
            guard.insert(
                "tok".into(),
                Entry {
                    data: fixture(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert!(cache.get("tok").await.is_none());
        assert_eq!(cache.len().await, 0);
    }
}
