//! Bot supervisor — PURA-118 WS-1.
//!
//! Spawns + tracks N bot actors. The supervisor doesn't own the
//! configuration of any individual bot; it just hands handles back to the
//! caller. WS-3+ may grow it (per-bot persistence, restart policy, etc).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::bot::run_bot;
use crate::command::BotCommand;
use crate::config::{BotConfig, BotId};
use crate::event::BotEvent;

/// Capacity of each bot's command queue. The dispatcher only ever has
/// one in-flight command, but a small buffer absorbs UI jitter without
/// blocking the supervisor.
const COMMAND_BUFFER: usize = 16;

/// Spawn a bot actor without a supervisor. The caller owns the returned
/// `BotHandle` directly. The supervisor uses this same constructor under
/// the hood so behaviour stays identical.
pub fn spawn_bot(id: BotId, config: BotConfig) -> BotHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
    let (event_tx, _) = broadcast::channel(config.event_buffer.max(1));
    let event_tx_for_actor = event_tx.clone();
    let cfg_for_actor = config.clone();
    let join = tokio::spawn(async move {
        run_bot(id, cfg_for_actor, cmd_rx, event_tx_for_actor).await;
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
}

impl Default for BotSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl BotSupervisor {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            bots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn a fresh bot actor. The returned id is also looked up via
    /// `get` / `list`; the handle is what the caller drives directly.
    /// We intentionally hand the same handle to both the supervisor map
    /// and the caller — the supervisor map keeps the actor alive past
    /// the caller dropping its copy, which is what callers want when
    /// the bot is owned by the long-lived service rather than a
    /// request handler.
    pub async fn spawn(&self, config: BotConfig) -> BotId {
        let id = BotId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let name = config.name.clone();
        let handle = spawn_bot(id, config);
        self.bots.lock().await.insert(id, handle);
        info!(%id, %name, "spawned bot");
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
