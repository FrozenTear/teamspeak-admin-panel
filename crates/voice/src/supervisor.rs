//! Bot supervisor — PURA-118 WS-1.
//!
//! Spawns + tracks N bot actors. The supervisor doesn't own the
//! configuration of any individual bot; it just hands handles back to the
//! caller. WS-3+ may grow it (per-bot persistence, restart policy, etc).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::bot::run_bot;
use crate::command::BotCommand;
use crate::config::{BotConfig, BotId};
use crate::event::BotEvent;
use crate::store::{
    InMemoryMusicBotStore, LibraryEntry, LibraryEntryId, MusicBotStore, NewLibraryEntry,
    PlaylistName, StoreResult, Track,
};

/// Capacity of each bot's command queue. The dispatcher only ever has
/// one in-flight command, but a small buffer absorbs UI jitter without
/// blocking the supervisor.
const COMMAND_BUFFER: usize = 16;

/// Spawn a bot actor without a supervisor. The caller owns the returned
/// `BotHandle` directly. The supervisor uses this same constructor under
/// the hood so behaviour stays identical.
///
/// `store` is the `MusicBotStore` the actor uses for queue / playlist /
/// library state — see [`InMemoryMusicBotStore`] for a default; WS-5
/// will ship the SurrealDB-backed impl in `ts6-manager-server`.
///
/// `yt_cookie` is the live-updated cookie-file path (PURA-223). The bot
/// actor reads it at track-play time so UI uploads take effect without a
/// manager restart. Pass `Arc::new(RwLock::new(None))` to disable cookies.
pub fn spawn_bot(
    id: BotId,
    config: BotConfig,
    store: Arc<dyn MusicBotStore>,
    yt_cookie: Arc<RwLock<Option<PathBuf>>>,
) -> BotHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
    let (event_tx, _) = broadcast::channel(config.event_buffer.max(1));
    let event_tx_for_actor = event_tx.clone();
    let cfg_for_actor = config.clone();
    let store_for_actor = Arc::clone(&store);
    let join = tokio::spawn(async move {
        run_bot(
            id,
            cfg_for_actor,
            store_for_actor,
            cmd_rx,
            event_tx_for_actor,
            yt_cookie,
        )
        .await;
    });
    BotHandle {
        id,
        config,
        commands: cmd_tx,
        events: event_tx,
        join,
    }
}

/// Caller-facing handle for one bot. Holds the command sender, an event
/// receiver factory, and a join handle so callers can decide between
/// `await`-ing termination or detaching.
pub struct BotHandle {
    id: BotId,
    config: BotConfig,
    commands: mpsc::Sender<BotCommand>,
    events: broadcast::Sender<BotEvent>,
    join: JoinHandle<()>,
}

impl BotHandle {
    pub fn id(&self) -> BotId {
        self.id
    }

    pub fn config(&self) -> &BotConfig {
        &self.config
    }

    /// Dispatch a command. Returns `Err` only if the actor task has
    /// already exited (channel closed); the actor itself never rejects
    /// commands by closing the channel — it emits `BotEvent::Error`
    /// instead.
    pub async fn send(&self, cmd: BotCommand) -> Result<(), SendError> {
        self.commands
            .send(cmd)
            .await
            .map_err(|_| SendError::ActorGone)
    }

    /// Try to dispatch without awaiting capacity.
    pub fn try_send(&self, cmd: BotCommand) -> Result<(), SendError> {
        self.commands.try_send(cmd).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => SendError::Full,
            mpsc::error::TrySendError::Closed(_) => SendError::ActorGone,
        })
    }

    /// Subscribe to this bot's event stream. Receivers are independent —
    /// each call returns a fresh receiver positioned at the next event.
    pub fn subscribe(&self) -> broadcast::Receiver<BotEvent> {
        self.events.subscribe()
    }

    /// Convenience: send `Shutdown` and await the actor task.
    pub async fn shutdown(self) -> Result<(), SendError> {
        // Best-effort send; the actor may already be exiting.
        let _ = self.commands.send(BotCommand::Shutdown).await;
        // Drop the sender so the actor's `rx.recv()` resolves to `None`
        // after the Shutdown is processed (matters when `auto_connect`
        // was off and we never went online).
        drop(self.commands);
        self.join.await.map_err(|err| {
            warn!(?err, "bot actor panicked on shutdown");
            SendError::ActorGone
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("bot actor has exited")]
    ActorGone,
    #[error("bot command queue is full")]
    Full,
}

/// The supervisor holds the canonical map of live bots so the rest of
/// the app (REST, FE, chat-bridge) can look up by `BotId` without
/// re-plumbing each handle.
pub struct BotSupervisor {
    next_id: AtomicU64,
    bots: Arc<Mutex<HashMap<BotId, BotHandle>>>,
    store: Arc<dyn MusicBotStore>,
}

impl Default for BotSupervisor {
    /// Default supervisor uses an in-memory store. Production wiring
    /// (`ts6-manager-server` in WS-5) will pass a SurrealDB-backed
    /// store via [`BotSupervisor::with_store`].
    fn default() -> Self {
        Self::with_store(Arc::new(InMemoryMusicBotStore::new()))
    }
}

impl BotSupervisor {
    /// Convenience — `BotSupervisor::default()` plus a fresh in-memory
    /// store. Used by tests + the WS-1 prototype where persistence
    /// across process restarts isn't a requirement.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a supervisor backed by the supplied store. WS-5 wires the
    /// SurrealDB impl in here.
    pub fn with_store(store: Arc<dyn MusicBotStore>) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            bots: Arc::new(Mutex::new(HashMap::new())),
            store,
        }
    }

    /// Borrow the store. Tests + WS-5 REST handlers reach for this when
    /// they need to mutate playlist/library state outside the bot's
    /// command dispatch.
    pub fn store(&self) -> &Arc<dyn MusicBotStore> {
        &self.store
    }

    /// Spawn a fresh bot actor. The returned id is also looked up via
    /// `get` / `list`; the handle is what the caller drives directly.
    /// We intentionally hand the same handle to both the supervisor map
    /// and the caller — the supervisor map keeps the actor alive past
    /// the caller dropping its copy, which is what callers want when
    /// the bot is owned by the long-lived service rather than a
    /// request handler.
    ///
    /// `yt_cookie` is the live-updated cookie-file path (PURA-223).
    /// The caller typically passes `AppState::yt_cookie.clone()`.
    pub async fn spawn(&self, config: BotConfig, yt_cookie: Arc<RwLock<Option<PathBuf>>>) -> BotId {
        let id = BotId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let name = config.name.clone();
        let handle = spawn_bot(id, config, Arc::clone(&self.store), yt_cookie);
        self.bots.lock().await.insert(id, handle);
        info!(%id, %name, "spawned bot");
        id
    }

    /// Spawn a bot under a caller-chosen `id` instead of a freshly-minted
    /// one. Used by the manager's boot-time rehydration path (PURA-357):
    /// persisted bots are re-spawned under their original ids so REST /
    /// FE references survive a process restart or image upgrade.
    ///
    /// The monotonic id counter is bumped past `id` so a subsequent
    /// [`spawn`](Self::spawn) cannot mint a colliding id, regardless of
    /// the order rehydrated ids arrive in.
    pub async fn spawn_with_id(
        &self,
        id: BotId,
        config: BotConfig,
        yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    ) -> BotId {
        let name = config.name.clone();
        let handle = spawn_bot(id, config, Arc::clone(&self.store), yt_cookie);
        self.bots.lock().await.insert(id, handle);
        // Keep the minted-id counter ahead of every rehydrated id.
        self.next_id.fetch_max(id.0 + 1, Ordering::Relaxed);
        info!(%id, %name, "rehydrated bot");
        id
    }

    /// Send a command to a tracked bot.
    pub async fn send(&self, id: BotId, cmd: BotCommand) -> Result<(), SendError> {
        let bots = self.bots.lock().await;
        let handle = bots.get(&id).ok_or(SendError::ActorGone)?;
        handle.send(cmd).await
    }

    /// Subscribe to a tracked bot's events.
    pub async fn subscribe(&self, id: BotId) -> Option<broadcast::Receiver<BotEvent>> {
        self.bots.lock().await.get(&id).map(|h| h.subscribe())
    }

    /// Snapshot of currently tracked bot IDs + their configs. Cheap so
    /// REST handlers can call it on every request.
    pub async fn list(&self) -> Vec<BotInfo> {
        self.bots
            .lock()
            .await
            .values()
            .map(|h| BotInfo {
                id: h.id,
                name: h.config.name.clone(),
                server_addr: h.config.server_addr.clone(),
            })
            .collect()
    }

    /// Tear a single bot down. Returns once the actor task has exited.
    pub async fn shutdown_bot(&self, id: BotId) -> Result<(), SendError> {
        let handle = self.bots.lock().await.remove(&id);
        match handle {
            Some(h) => h.shutdown().await,
            None => Err(SendError::ActorGone),
        }
    }

    // -----------------------------------------------------------------
    // PURA-121 WS-3 — playlist + library CRUD.
    //
    // These don't go through `BotCommand` because they aren't lifecycle
    // ops — REST + chat surfaces shouldn't have to round-trip through
    // the actor's mpsc to add a playlist entry. Each method also emits
    // a `BotEvent` on the bot's broadcast channel so subscribers can
    // refetch without polling.
    // -----------------------------------------------------------------

    pub async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store.playlist_create(bot, name.clone()).await?;
        self.notify(bot, BotEvent::PlaylistChanged(name)).await;
        Ok(())
    }

    pub async fn playlist_rename(
        &self,
        bot: BotId,
        old: PlaylistName,
        new: PlaylistName,
    ) -> StoreResult<()> {
        self.store.playlist_rename(bot, old, new.clone()).await?;
        self.notify(bot, BotEvent::PlaylistChanged(new)).await;
        Ok(())
    }

    pub async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store.playlist_delete(bot, name.clone()).await?;
        self.notify(bot, BotEvent::PlaylistChanged(name)).await;
        Ok(())
    }

    pub async fn playlist_add_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        track: crate::store::NewTrack,
    ) -> StoreResult<Track> {
        let stored = self.store.playlist_add_track(bot, name, track).await?;
        self.notify(bot, BotEvent::PlaylistChanged(name.clone()))
            .await;
        Ok(stored)
    }

    pub async fn playlist_remove_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        id: crate::store::TrackId,
    ) -> StoreResult<bool> {
        let changed = self.store.playlist_remove_track(bot, name, id).await?;
        if changed {
            self.notify(bot, BotEvent::PlaylistChanged(name.clone()))
                .await;
        }
        Ok(changed)
    }

    pub async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>> {
        self.store.playlist_list(bot).await
    }

    pub async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>> {
        self.store.playlist_list_tracks(bot, name).await
    }

    pub async fn library_add(
        &self,
        bot: BotId,
        entry: NewLibraryEntry,
    ) -> StoreResult<LibraryEntry> {
        let stored = self.store.library_add(bot, entry).await?;
        self.notify(bot, BotEvent::LibraryChanged).await;
        Ok(stored)
    }

    pub async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool> {
        let removed = self.store.library_remove(bot, id).await?;
        if removed {
            self.notify(bot, BotEvent::LibraryChanged).await;
        }
        Ok(removed)
    }

    pub async fn library_lookup(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> StoreResult<Option<LibraryEntry>> {
        self.store.library_lookup(bot, id).await
    }

    pub async fn library_list(
        &self,
        bot: BotId,
        tag: Option<&str>,
    ) -> StoreResult<Vec<LibraryEntry>> {
        self.store.library_list(bot, tag).await
    }

    /// Best-effort emit on a tracked bot's broadcast channel. If the bot
    /// isn't tracked (already shut down) we drop the event silently —
    /// every WS-3 caller is OK with that since the persisted state is
    /// authoritative for late-coming subscribers.
    async fn notify(&self, bot: BotId, event: BotEvent) {
        if let Some(handle) = self.bots.lock().await.get(&bot) {
            let _ = handle.events.send(event);
        }
    }

    /// Tear every tracked bot down. Used on graceful service shutdown.
    pub async fn shutdown_all(&self) {
        let handles: Vec<BotHandle> = {
            let mut map = self.bots.lock().await;
            map.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            let id = handle.id();
            if let Err(err) = handle.shutdown().await {
                warn!(%id, ?err, "bot shutdown failed");
            }
        }
    }
}

/// Lightweight read-only view used by `BotSupervisor::list`.
#[derive(Debug, Clone)]
pub struct BotInfo {
    pub id: BotId,
    pub name: String,
    pub server_addr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_config(name: &str) -> BotConfig {
        // `auto_connect = false` keeps the spawned actor in `Disconnected`
        // so the test never touches the network.
        BotConfig::new(name, std::env::temp_dir().join(format!("{name}.identity")))
            .with_auto_connect(false)
    }

    /// PURA-357 — a rehydrated bot keeps its persisted id, and a bot
    /// spawned afterwards must not collide with it even though the
    /// rehydrated id is higher than the supervisor's fresh counter.
    #[tokio::test]
    async fn spawn_with_id_preserves_id_and_advances_the_counter() {
        let sup = BotSupervisor::new();
        let cookie = Arc::new(RwLock::new(None));

        let rehydrated = sup
            .spawn_with_id(BotId(5), idle_config("rehydrated"), Arc::clone(&cookie))
            .await;
        assert_eq!(rehydrated, BotId(5), "rehydration must keep the stored id");

        // A fresh spawn must land past every rehydrated id, not at 1.
        let fresh = sup.spawn(idle_config("fresh"), Arc::clone(&cookie)).await;
        assert_eq!(fresh, BotId(6), "fresh spawn must not collide with id 5");

        let ids: Vec<u64> = {
            let mut v: Vec<u64> = sup.list().await.into_iter().map(|i| i.id.0).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(ids, vec![5, 6]);

        sup.shutdown_all().await;
    }
}
