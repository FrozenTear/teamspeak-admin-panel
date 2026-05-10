//! Per-bot persistence surface — PURA-121 WS-3.
//!
//! This module owns the **data model** (queue, playlists, library) and
//! the **`MusicBotStore` trait** that the bot actor + supervisor + WS-5
//! REST handlers all call into. WS-3 ships an in-memory impl
//! (`InMemoryMusicBotStore`) so the music-bot crate stays test-cheap and
//! embeddable without dragging a DB engine in. The SurrealDB-backed impl
//! lives in `ts6-manager-server` (WS-5) once the supervisor is wired into
//! the axum stack.
//!
//! Concurrency: all in-memory state is guarded by a single
//! `tokio::sync::RwLock` — readers (peek / list / current) don't block
//! each other; the per-op critical sections are short enough that a more
//! granular lock layout would add complexity without throughput. The
//! Surreal impl in WS-5 will swap this for the existing repo-pattern's
//! pooled queries.
//!
//! Snapshot/load: `InMemoryMusicBotStore` exposes `snapshot_to_json`
//! / `load_from_json` so the WS-3 acceptance test can prove the data
//! model survives a restart without spinning up Surreal. The same
//! serialised shape is what the SurrealDB impl will round-trip rows
//! through, so the snapshot doubles as the migration contract.
//!
//! Cleanroom rule applies: this surface is derived from the `BotEvent` /
//! `BotCommand` API in `docs/voice/music-bot-lifecycle.md` and the
//! issue spec only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::command::AudioSource;
use crate::config::BotId;

/// Identifier minted by the store for every queued / playlisted track.
/// Stable across snapshot/load — the in-memory impl persists the
/// monotonic counter alongside the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u64);

impl std::fmt::Display for TrackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "trk-{:08x}", self.0)
    }
}

/// Identifier minted by the store for every library entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LibraryEntryId(pub u64);

impl std::fmt::Display for LibraryEntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lib-{:08x}", self.0)
    }
}

/// Playlist key. Wraps a `String` to keep the call sites self-documenting
/// and to leave room for normalisation rules later (case-fold, trim).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistName(pub String);

impl std::fmt::Display for PlaylistName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S: Into<String>> From<S> for PlaylistName {
    fn from(s: S) -> Self {
        PlaylistName(s.into())
    }
}

/// One queued (or playlisted) track. `id` is store-assigned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub source: AudioSource,
    pub title: String,
    pub duration_secs: Option<u64>,
    pub requested_by: Option<String>,
}

/// New-track payload used by `enqueue` / `playlist_add_track`. The store
/// stamps a fresh `TrackId` on insert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTrack {
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub requested_by: Option<String>,
}

impl NewTrack {
    /// Convenience — tests + WS-5 REST handlers reach for this often.
    pub fn url(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            source: AudioSource::Url(url.into()),
            title: title.into(),
            duration_secs: None,
            requested_by: None,
        }
    }
}

/// One library entry — a saved source + tags. `id` is store-assigned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: LibraryEntryId,
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLibraryEntry {
    pub source: AudioSource,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("playlist '{0}' not found")]
    PlaylistNotFound(PlaylistName),
    #[error("playlist '{0}' already exists")]
    PlaylistExists(PlaylistName),
    #[error("track {0} not found")]
    TrackNotFound(TrackId),
    #[error("library entry {0} not found")]
    LibraryEntryNotFound(LibraryEntryId),
    #[error("reorder argument did not match queue: {reason}")]
    ReorderMismatch { reason: String },
    #[error("snapshot serialise/deserialise: {0}")]
    Snapshot(String),
    #[error("backend: {0}")]
    Backend(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Persistence boundary for one music-bot's queue / playlists / library.
/// All operations are scoped by `BotId` so a single store instance can
/// host every bot the supervisor spawns.
///
/// Implementations MUST be `Send + Sync`; the bot actor and the
/// supervisor share an `Arc<dyn MusicBotStore>`.
#[async_trait]
pub trait MusicBotStore: Send + Sync {
    // ---- Queue --------------------------------------------------------

    /// Append a track to the end of the bot's queue. Returns the
    /// stored `Track` with its assigned `id`.
    async fn queue_enqueue(&self, bot: BotId, track: NewTrack) -> StoreResult<Track>;

    /// Pop the head of the queue. The "now-playing" track is always the
    /// head — callers `dequeue_head` on natural end-of-stream / `SkipNext`
    /// to advance.
    async fn queue_dequeue_head(&self, bot: BotId) -> StoreResult<Option<Track>>;

    /// Snapshot of the upcoming queue, head-first. Read-only — the
    /// returned `Vec` is owned and decoupled from the store's lock.
    async fn queue_peek(&self, bot: BotId) -> StoreResult<Vec<Track>>;

    /// Drop every track from the queue. No-op if already empty.
    async fn queue_clear(&self, bot: BotId) -> StoreResult<()>;

    /// Reorder the queue to match `order`. Argument MUST be a permutation
    /// of the current queue's `TrackId`s (same length, same set) — the
    /// store rejects anything else with `ReorderMismatch`.
    async fn queue_reorder(&self, bot: BotId, order: Vec<TrackId>) -> StoreResult<()>;

    /// Remove a single track. Returns `Ok(true)` if the id was present,
    /// `Ok(false)` otherwise.
    async fn queue_remove(&self, bot: BotId, id: TrackId) -> StoreResult<bool>;

    /// Convenience: the head of the queue, if any. Equivalent to
    /// `queue_peek(...)?.first().cloned()`.
    async fn queue_current(&self, bot: BotId) -> StoreResult<Option<Track>>;

    // ---- Playlists ----------------------------------------------------

    async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()>;
    async fn playlist_rename(
        &self,
        bot: BotId,
        old: PlaylistName,
        new: PlaylistName,
    ) -> StoreResult<()>;
    async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()>;
    async fn playlist_add_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        track: NewTrack,
    ) -> StoreResult<Track>;
    async fn playlist_remove_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        id: TrackId,
    ) -> StoreResult<bool>;
    async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>>;
    async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>>;

    /// Append every track in `name` to the bot's queue, preserving order.
    /// Returns the freshly-stamped `Track` clones so callers don't have
    /// to round-trip through `queue_peek` to learn the new ids.
    async fn enqueue_playlist(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>>;

    // ---- Library ------------------------------------------------------

    async fn library_add(
        &self,
        bot: BotId,
        entry: NewLibraryEntry,
    ) -> StoreResult<LibraryEntry>;
    async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool>;
    async fn library_lookup(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> StoreResult<Option<LibraryEntry>>;

    /// List library entries, optionally filtered by tag (exact match).
    async fn library_list(
        &self,
        bot: BotId,
        tag: Option<&str>,
    ) -> StoreResult<Vec<LibraryEntry>>;
}

// =====================================================================
// In-memory implementation
// =====================================================================

/// Per-bot state kept by `InMemoryMusicBotStore`. Stays serde-friendly so
/// `snapshot_to_json` / `load_from_json` round-trips the whole shape
/// without a hand-written wire format. The Surreal impl in WS-5 will
/// project this struct onto its tables — the field names ARE the
/// migration contract.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PerBotState {
    queue: Vec<Track>,
    playlists: HashMap<PlaylistName, Vec<Track>>,
    library: Vec<LibraryEntry>,
}

/// Snapshot envelope. The version field gives the WS-5 SurrealDB impl a
/// hard pin to refuse loading a future snapshot it doesn't understand.
/// Wire-format only — callers go through `snapshot_to_json` /
/// `load_from_json`. The field shape IS the migration contract; see
/// `docs/voice/music-bot-state.md`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StoreSnapshot {
    /// Schema version. Bumped any time `PerBotState` shape changes.
    version: u32,
    /// Next monotonic counter for `TrackId` / `LibraryEntryId`. Stored
    /// once, shared across all bots, so ids stay stable across restarts.
    next_track_id: u64,
    next_library_id: u64,
    bots: HashMap<BotId, PerBotState>,
}

/// Schema version pinned to whatever shape `PerBotState` is in today.
/// Bump alongside any field rename / type change.
pub const SNAPSHOT_VERSION: u32 = 1;

/// In-memory `MusicBotStore`. Default-constructible, cheap to clone (it
/// is itself just an `Arc`-wrapped lock once you wrap it for the
/// supervisor). `snapshot_to_json` / `load_from_json` give the WS-3
/// integration test a real restart-restore proof.
#[derive(Debug, Default)]
pub struct InMemoryMusicBotStore {
    inner: RwLock<PerStoreState>,
    next_track_id: AtomicU64,
    next_library_id: AtomicU64,
}

#[derive(Debug, Default)]
struct PerStoreState {
    bots: HashMap<BotId, PerBotState>,
}

impl InMemoryMusicBotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Boxed helper — saves callers the `Arc<dyn _>` ceremony.
    pub fn shared() -> Arc<dyn MusicBotStore> {
        Arc::new(Self::new())
    }

    fn next_track_id(&self) -> TrackId {
        // `fetch_add(1)` on Relaxed is fine — ids only need uniqueness
        // within this store instance, not a happens-before with anything
        // else. Snapshot/load reseeds the counter from the persisted
        // `next_track_id` to avoid post-restart collisions.
        TrackId(self.next_track_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn next_library_id(&self) -> LibraryEntryId {
        LibraryEntryId(self.next_library_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Serialise the entire store into a JSON byte string. Used by the
    /// WS-3 acceptance test to prove the data model is restart-safe; the
    /// WS-5 SurrealDB impl will project the same shape onto its tables.
    pub async fn snapshot_to_json(&self) -> StoreResult<Vec<u8>> {
        let inner = self.inner.read().await;
        let snapshot = StoreSnapshot {
            version: SNAPSHOT_VERSION,
            next_track_id: self.next_track_id.load(Ordering::Relaxed),
            next_library_id: self.next_library_id.load(Ordering::Relaxed),
            bots: inner.bots.clone(),
        };
        serde_json::to_vec_pretty(&snapshot)
            .map_err(|err| StoreError::Snapshot(err.to_string()))
    }

    /// Replace this store's state with the contents of a snapshot. The
    /// monotonic id counters are reseeded so freshly-issued ids never
    /// collide with persisted ones.
    pub async fn load_from_json(&self, bytes: &[u8]) -> StoreResult<()> {
        let snapshot: StoreSnapshot = serde_json::from_slice(bytes)
            .map_err(|err| StoreError::Snapshot(err.to_string()))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(StoreError::Snapshot(format!(
                "unsupported snapshot version {}: this build expects {}",
                snapshot.version, SNAPSHOT_VERSION
            )));
        }
        let mut inner = self.inner.write().await;
        inner.bots = snapshot.bots;
        self.next_track_id
            .store(snapshot.next_track_id, Ordering::Relaxed);
        self.next_library_id
            .store(snapshot.next_library_id, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait]
impl MusicBotStore for InMemoryMusicBotStore {
    // ---- Queue --------------------------------------------------------

    async fn queue_enqueue(&self, bot: BotId, track: NewTrack) -> StoreResult<Track> {
        let id = self.next_track_id();
        let track = Track {
            id,
            source: track.source,
            title: track.title,
            duration_secs: track.duration_secs,
            requested_by: track.requested_by,
        };
        let mut inner = self.inner.write().await;
        inner.bots.entry(bot).or_default().queue.push(track.clone());
        Ok(track)
    }

    async fn queue_dequeue_head(&self, bot: BotId) -> StoreResult<Option<Track>> {
        let mut inner = self.inner.write().await;
        let queue = &mut inner.bots.entry(bot).or_default().queue;
        if queue.is_empty() {
            Ok(None)
        } else {
            Ok(Some(queue.remove(0)))
        }
    }

    async fn queue_peek(&self, bot: BotId) -> StoreResult<Vec<Track>> {
        let inner = self.inner.read().await;
        Ok(inner
            .bots
            .get(&bot)
            .map(|s| s.queue.clone())
            .unwrap_or_default())
    }

    async fn queue_clear(&self, bot: BotId) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        if let Some(state) = inner.bots.get_mut(&bot) {
            state.queue.clear();
        }
        Ok(())
    }

    async fn queue_reorder(&self, bot: BotId, order: Vec<TrackId>) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let queue = &mut inner.bots.entry(bot).or_default().queue;
        if order.len() != queue.len() {
            return Err(StoreError::ReorderMismatch {
                reason: format!(
                    "argument has {} ids, queue has {}",
                    order.len(),
                    queue.len()
                ),
            });
        }
        let existing_ids: std::collections::HashSet<TrackId> =
            queue.iter().map(|t| t.id).collect();
        for id in &order {
            if !existing_ids.contains(id) {
                return Err(StoreError::ReorderMismatch {
                    reason: format!("id {id} not in current queue"),
                });
            }
        }
        // Reorder by removing each id from `queue` in `order`. O(n²) but
        // music-bot queues are small (UI typical: <50 tracks); not worth
        // a more involved structure.
        let mut by_id: HashMap<TrackId, Track> =
            queue.drain(..).map(|t| (t.id, t)).collect();
        for id in order {
            // `existing_ids` check above guarantees the id is here, so
            // the `unwrap` is sound. Defensive `expect` for the
            // catastrophic case where a future patch breaks the
            // invariant — don't want a silent reorder.
            let track = by_id
                .remove(&id)
                .expect("id present in existing_ids must be in by_id");
            queue.push(track);
        }
        Ok(())
    }

    async fn queue_remove(&self, bot: BotId, id: TrackId) -> StoreResult<bool> {
        let mut inner = self.inner.write().await;
        let queue = &mut inner.bots.entry(bot).or_default().queue;
        let before = queue.len();
        queue.retain(|t| t.id != id);
        Ok(queue.len() != before)
    }

    async fn queue_current(&self, bot: BotId) -> StoreResult<Option<Track>> {
        let inner = self.inner.read().await;
        Ok(inner
            .bots
            .get(&bot)
            .and_then(|s| s.queue.first().cloned()))
    }

    // ---- Playlists ----------------------------------------------------

    async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let playlists = &mut inner.bots.entry(bot).or_default().playlists;
        if playlists.contains_key(&name) {
            return Err(StoreError::PlaylistExists(name));
        }
        playlists.insert(name, Vec::new());
        Ok(())
    }

    async fn playlist_rename(
        &self,
        bot: BotId,
        old: PlaylistName,
        new: PlaylistName,
    ) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let playlists = &mut inner.bots.entry(bot).or_default().playlists;
        if !playlists.contains_key(&old) {
            return Err(StoreError::PlaylistNotFound(old));
        }
        if playlists.contains_key(&new) {
            return Err(StoreError::PlaylistExists(new));
        }
        let tracks = playlists.remove(&old).unwrap();
        playlists.insert(new, tracks);
        Ok(())
    }

    async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let playlists = &mut inner.bots.entry(bot).or_default().playlists;
        playlists
            .remove(&name)
            .map(|_| ())
            .ok_or(StoreError::PlaylistNotFound(name))
    }

    async fn playlist_add_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        track: NewTrack,
    ) -> StoreResult<Track> {
        let id = self.next_track_id();
        let stored = Track {
            id,
            source: track.source,
            title: track.title,
            duration_secs: track.duration_secs,
            requested_by: track.requested_by,
        };
        let mut inner = self.inner.write().await;
        let playlists = &mut inner.bots.entry(bot).or_default().playlists;
        let playlist = playlists
            .get_mut(name)
            .ok_or_else(|| StoreError::PlaylistNotFound(name.clone()))?;
        playlist.push(stored.clone());
        Ok(stored)
    }

    async fn playlist_remove_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        id: TrackId,
    ) -> StoreResult<bool> {
        let mut inner = self.inner.write().await;
        let playlists = &mut inner.bots.entry(bot).or_default().playlists;
        let playlist = playlists
            .get_mut(name)
            .ok_or_else(|| StoreError::PlaylistNotFound(name.clone()))?;
        let before = playlist.len();
        playlist.retain(|t| t.id != id);
        Ok(playlist.len() != before)
    }

    async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>> {
        let inner = self.inner.read().await;
        let playlist = inner
            .bots
            .get(&bot)
            .and_then(|s| s.playlists.get(name))
            .ok_or_else(|| StoreError::PlaylistNotFound(name.clone()))?;
        Ok(playlist.clone())
    }

    async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>> {
        let inner = self.inner.read().await;
        let mut names: Vec<PlaylistName> = inner
            .bots
            .get(&bot)
            .map(|s| s.playlists.keys().cloned().collect())
            .unwrap_or_default();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(names)
    }

    async fn enqueue_playlist(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>> {
        // Read playlist source under a read lock first, then re-stamp
        // ids and append under a write lock. The two-phase shape is
        // intentional — playlist tracks have stable ids inside the
        // playlist, but every queue copy gets a fresh id so a removal
        // in the queue doesn't touch the playlist source.
        let source = {
            let inner = self.inner.read().await;
            inner
                .bots
                .get(&bot)
                .and_then(|s| s.playlists.get(name))
                .ok_or_else(|| StoreError::PlaylistNotFound(name.clone()))?
                .clone()
        };
        let stamped: Vec<Track> = source
            .into_iter()
            .map(|t| Track {
                id: self.next_track_id(),
                ..t
            })
            .collect();
        let mut inner = self.inner.write().await;
        let queue = &mut inner.bots.entry(bot).or_default().queue;
        queue.extend(stamped.iter().cloned());
        Ok(stamped)
    }

    // ---- Library ------------------------------------------------------

    async fn library_add(
        &self,
        bot: BotId,
        entry: NewLibraryEntry,
    ) -> StoreResult<LibraryEntry> {
        let id = self.next_library_id();
        let stored = LibraryEntry {
            id,
            source: entry.source,
            title: entry.title,
            tags: entry.tags,
        };
        let mut inner = self.inner.write().await;
        inner
            .bots
            .entry(bot)
            .or_default()
            .library
            .push(stored.clone());
        Ok(stored)
    }

    async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool> {
        let mut inner = self.inner.write().await;
        let library = &mut inner.bots.entry(bot).or_default().library;
        let before = library.len();
        library.retain(|e| e.id != id);
        Ok(library.len() != before)
    }

    async fn library_lookup(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> StoreResult<Option<LibraryEntry>> {
        let inner = self.inner.read().await;
        Ok(inner
            .bots
            .get(&bot)
            .and_then(|s| s.library.iter().find(|e| e.id == id).cloned()))
    }

    async fn library_list(
        &self,
        bot: BotId,
        tag: Option<&str>,
    ) -> StoreResult<Vec<LibraryEntry>> {
        let inner = self.inner.read().await;
        let entries = inner
            .bots
            .get(&bot)
            .map(|s| s.library.clone())
            .unwrap_or_default();
        Ok(match tag {
            Some(t) => entries
                .into_iter()
                .filter(|e| e.tags.iter().any(|x| x == t))
                .collect(),
            None => entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> BotId {
        BotId(1)
    }

    fn new_track(title: &str) -> NewTrack {
        NewTrack::url(title, format!("https://example.com/{title}.mp3"))
    }

    #[tokio::test]
    async fn queue_enqueue_dequeue_roundtrip() {
        let store = InMemoryMusicBotStore::new();
        let t1 = store.queue_enqueue(bot(), new_track("a")).await.unwrap();
        let t2 = store.queue_enqueue(bot(), new_track("b")).await.unwrap();
        let t3 = store.queue_enqueue(bot(), new_track("c")).await.unwrap();
        // Three distinct ids, queue ordered by insertion.
        assert_ne!(t1.id, t2.id);
        assert_ne!(t2.id, t3.id);
        let peeked = store.queue_peek(bot()).await.unwrap();
        assert_eq!(peeked.iter().map(|t| t.id).collect::<Vec<_>>(), vec![t1.id, t2.id, t3.id]);

        // Current = head.
        let current = store.queue_current(bot()).await.unwrap();
        assert_eq!(current.unwrap().id, t1.id);

        // Dequeue advances the head.
        let popped = store.queue_dequeue_head(bot()).await.unwrap();
        assert_eq!(popped.unwrap().id, t1.id);
        let current = store.queue_current(bot()).await.unwrap();
        assert_eq!(current.unwrap().id, t2.id);

        // Drains to empty.
        store.queue_dequeue_head(bot()).await.unwrap();
        store.queue_dequeue_head(bot()).await.unwrap();
        assert!(store.queue_current(bot()).await.unwrap().is_none());
        // Past-the-end dequeue returns None, not an error.
        assert!(store.queue_dequeue_head(bot()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn queue_remove_clear_reorder() {
        let store = InMemoryMusicBotStore::new();
        let t1 = store.queue_enqueue(bot(), new_track("a")).await.unwrap();
        let t2 = store.queue_enqueue(bot(), new_track("b")).await.unwrap();
        let t3 = store.queue_enqueue(bot(), new_track("c")).await.unwrap();

        // remove(unknown) is Ok(false).
        assert!(!store.queue_remove(bot(), TrackId(99_999)).await.unwrap());
        // remove(known) returns true and shrinks the queue.
        assert!(store.queue_remove(bot(), t2.id).await.unwrap());
        assert_eq!(
            store
                .queue_peek(bot())
                .await
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![t1.id, t3.id]
        );

        // reorder swaps the two remaining tracks.
        store.queue_reorder(bot(), vec![t3.id, t1.id]).await.unwrap();
        assert_eq!(
            store
                .queue_peek(bot())
                .await
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![t3.id, t1.id]
        );

        // reorder rejects mismatched lengths.
        let err = store
            .queue_reorder(bot(), vec![t3.id])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ReorderMismatch { .. }));

        // reorder rejects unknown ids.
        let err = store
            .queue_reorder(bot(), vec![t3.id, TrackId(999)])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ReorderMismatch { .. }));

        // clear empties.
        store.queue_clear(bot()).await.unwrap();
        assert!(store.queue_peek(bot()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn playlist_crud_and_enqueue() {
        let store = InMemoryMusicBotStore::new();
        let pl: PlaylistName = "lo-fi-radio".into();
        store.playlist_create(bot(), pl.clone()).await.unwrap();
        // Duplicate create is rejected.
        let err = store
            .playlist_create(bot(), pl.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PlaylistExists(_)));

        let pt1 = store
            .playlist_add_track(bot(), &pl, new_track("a"))
            .await
            .unwrap();
        let pt2 = store
            .playlist_add_track(bot(), &pl, new_track("b"))
            .await
            .unwrap();
        let listed = store.playlist_list_tracks(bot(), &pl).await.unwrap();
        assert_eq!(listed.iter().map(|t| t.id).collect::<Vec<_>>(), vec![pt1.id, pt2.id]);

        // remove returns true/false; absent id is false, not an error.
        assert!(store
            .playlist_remove_track(bot(), &pl, pt1.id)
            .await
            .unwrap());
        assert!(!store
            .playlist_remove_track(bot(), &pl, TrackId(9_999))
            .await
            .unwrap());

        // rename succeeds; rename-onto-existing rejects.
        let pl2: PlaylistName = "renamed".into();
        store.playlist_rename(bot(), pl.clone(), pl2.clone()).await.unwrap();
        // Original name no longer exists → list_tracks errors.
        let err = store
            .playlist_list_tracks(bot(), &pl)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PlaylistNotFound(_)));

        // enqueue_playlist appends every playlist track with fresh ids.
        let queued = store.enqueue_playlist(bot(), &pl2).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_ne!(queued[0].id, pt2.id, "enqueue_playlist must mint fresh ids");
        assert_eq!(queued[0].title, "b");

        // playlist_list returns sorted names.
        store.playlist_create(bot(), "alpha".into()).await.unwrap();
        let names = store.playlist_list(bot()).await.unwrap();
        assert_eq!(
            names,
            vec![PlaylistName("alpha".into()), PlaylistName("renamed".into())]
        );

        // delete is idempotent in the not-found sense — second call errors.
        store.playlist_delete(bot(), pl2.clone()).await.unwrap();
        let err = store.playlist_delete(bot(), pl2).await.unwrap_err();
        assert!(matches!(err, StoreError::PlaylistNotFound(_)));
    }

    #[tokio::test]
    async fn library_lookup_and_tag_filter() {
        let store = InMemoryMusicBotStore::new();
        let e1 = store
            .library_add(
                bot(),
                NewLibraryEntry {
                    source: AudioSource::Url("https://r.example/lofi.mp3".into()),
                    title: "lofi-1".into(),
                    tags: vec!["chill".into(), "instrumental".into()],
                },
            )
            .await
            .unwrap();
        let e2 = store
            .library_add(
                bot(),
                NewLibraryEntry {
                    source: AudioSource::Url("https://r.example/punk.mp3".into()),
                    title: "punk-1".into(),
                    tags: vec!["loud".into()],
                },
            )
            .await
            .unwrap();

        // lookup hits.
        assert_eq!(
            store.library_lookup(bot(), e1.id).await.unwrap().unwrap().id,
            e1.id
        );
        // unknown id is None, not an error.
        assert!(store
            .library_lookup(bot(), LibraryEntryId(99_999))
            .await
            .unwrap()
            .is_none());

        // tag filter narrows.
        let chill = store
            .library_list(bot(), Some("chill"))
            .await
            .unwrap();
        assert_eq!(chill.len(), 1);
        assert_eq!(chill[0].id, e1.id);

        // No-tag list returns everything.
        let all = store.library_list(bot(), None).await.unwrap();
        assert_eq!(all.len(), 2);

        // remove returns true/false.
        assert!(store.library_remove(bot(), e2.id).await.unwrap());
        assert!(!store.library_remove(bot(), e2.id).await.unwrap());
    }

    #[tokio::test]
    async fn snapshot_load_round_trips_with_id_continuity() {
        let store = InMemoryMusicBotStore::new();
        let t1 = store.queue_enqueue(bot(), new_track("a")).await.unwrap();
        let t2 = store.queue_enqueue(bot(), new_track("b")).await.unwrap();
        let bytes = store.snapshot_to_json().await.unwrap();
        let next = InMemoryMusicBotStore::new();
        next.load_from_json(&bytes).await.unwrap();
        let peeked = next.queue_peek(bot()).await.unwrap();
        assert_eq!(
            peeked.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![t1.id, t2.id]
        );
        // Reseeded counter — newly issued ids are higher than persisted ones.
        let t3 = next.queue_enqueue(bot(), new_track("c")).await.unwrap();
        assert!(t3.id > t2.id);
    }

    #[tokio::test]
    async fn snapshot_rejects_unsupported_version() {
        let store = InMemoryMusicBotStore::new();
        // Hand-craft a snapshot with a future version.
        let envelope = serde_json::json!({
            "version": SNAPSHOT_VERSION + 1,
            "next_track_id": 0,
            "next_library_id": 0,
            "bots": {},
        });
        let err = store
            .load_from_json(envelope.to_string().as_bytes())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Snapshot(_)));
    }

    #[tokio::test]
    async fn per_bot_isolation() {
        let store = InMemoryMusicBotStore::new();
        store.queue_enqueue(BotId(1), new_track("a")).await.unwrap();
        store.queue_enqueue(BotId(2), new_track("b")).await.unwrap();
        let q1 = store.queue_peek(BotId(1)).await.unwrap();
        let q2 = store.queue_peek(BotId(2)).await.unwrap();
        assert_eq!(q1.len(), 1);
        assert_eq!(q2.len(), 1);
        assert_ne!(q1[0].title, q2[0].title);
    }
}
