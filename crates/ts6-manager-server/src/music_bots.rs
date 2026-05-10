//! Music-bot service hook (PURA-123 WS-5).
//!
//! Wraps the WS-1 [`music_bot::BotSupervisor`] together with an in-memory
//! request log and the runtime artefacts the REST surface needs (default
//! identity directory, channel tracking, last-known state, last
//! now-playing). The route layer in [`crate::routes::music_bots`] is the
//! only consumer — kept separate from `app_state` so `AppState` stays a
//! pure DTO.
//!
//! WS-5 ships an in-memory, in-process implementation. The SurrealDB-
//! backed persistence ticket is flagged for follow-up under PURA-117 §
//! "Out of scope" — the trait surface here is shaped so that swap is
//! local to this file (the route layer never reaches past
//! [`MusicBotService`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use music_bot::{BotEvent, BotId, BotState, BotSupervisor, ChannelId, Track};
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use ts6_manager_shared::music_bots as wire;

/// Service layer wired into [`crate::app_state::AppState::music_bots`].
/// One instance per process; cheap to clone (every field is `Arc`-shared).
#[derive(Clone)]
pub struct MusicBotService {
    pub supervisor: Arc<BotSupervisor>,
    pub identity_dir: Arc<PathBuf>,
    pub liveness: Arc<LivenessTracker>,
    pub requests: Arc<RequestLog>,
}

impl MusicBotService {
    /// Build a service with the in-memory supervisor + an empty request
    /// log. `identity_dir` is where bots without an explicit
    /// `identityPath` receive their on-disk identity file (matches
    /// `ts6_voice_fixture::load_or_create_identity`).
    pub fn new(identity_dir: PathBuf) -> Self {
        let supervisor = Arc::new(BotSupervisor::new());
        let liveness = Arc::new(LivenessTracker::default());
        let requests = Arc::new(RequestLog::default());
        Self {
            supervisor,
            identity_dir: Arc::new(identity_dir),
            liveness,
            requests,
        }
    }

    /// Test helper — fresh in-memory supervisor + a per-process temp
    /// identity directory. Used by every existing test fixture that
    /// constructs an `AppState` literal.
    #[cfg(test)]
    pub fn default_for_tests() -> Self {
        Self::new(std::env::temp_dir().join("ts6-test-music-bots"))
    }

    /// Spawn a tokio task that subscribes to a freshly-created bot's
    /// `BotEvent` broadcast and updates the [`LivenessTracker`] +
    /// [`RequestLog`] accordingly. Cheap — the actor's broadcast capacity
    /// is small (default 64 in `BotConfig::event_buffer`); the worker is
    /// purely a state mirror.
    pub async fn watch(&self, bot: BotId) {
        let Some(rx) = self.supervisor.subscribe(bot).await else {
            return;
        };
        let liveness = Arc::clone(&self.liveness);
        let requests = Arc::clone(&self.requests);
        tokio::spawn(watcher_loop(bot, rx, liveness, requests));
    }
}

/// Per-bot live state the route layer reads back as `MusicBotSummary` /
/// `MusicBotDetail`. The bot actor itself is the source of truth for
/// state transitions; we mirror them here so the GET handlers don't have
/// to round-trip through the broadcast channel for a snapshot read.
#[derive(Default)]
pub struct LivenessTracker {
    inner: RwLock<HashMap<BotId, BotLiveness>>,
}

#[derive(Debug, Clone)]
pub struct BotLiveness {
    pub state: BotState,
    pub now_playing: Option<Track>,
    pub channel_id: Option<ChannelId>,
}

impl Default for BotLiveness {
    fn default() -> Self {
        Self {
            state: BotState::Disconnected,
            now_playing: None,
            channel_id: None,
        }
    }
}

impl LivenessTracker {
    pub async fn snapshot(&self, bot: BotId) -> BotLiveness {
        self.inner
            .read()
            .await
            .get(&bot)
            .cloned()
            .unwrap_or_default()
    }

    async fn record_event(&self, bot: BotId, ev: &BotEvent) {
        let mut inner = self.inner.write().await;
        let entry = inner.entry(bot).or_default();
        match ev {
            BotEvent::StateChanged { to, .. } => {
                entry.state = *to;
                if matches!(to, BotState::Disconnected | BotState::Disconnecting) {
                    entry.channel_id = None;
                    entry.now_playing = None;
                }
            }
            BotEvent::Connected {
                default_channel, ..
            } => {
                entry.channel_id = Some(*default_channel);
            }
            BotEvent::Disconnected { .. } => {
                entry.channel_id = None;
                entry.now_playing = None;
            }
            BotEvent::JoinedChannel { channel_id } => {
                entry.channel_id = Some(*channel_id);
            }
            BotEvent::LeftChannel => {
                entry.channel_id = None;
            }
            BotEvent::NowPlaying(track) => {
                entry.now_playing = Some(track.clone());
            }
            BotEvent::QueueEmpty => {
                entry.now_playing = None;
            }
            BotEvent::QueueChanged { current, .. } => {
                entry.now_playing = current.clone();
            }
            // The remaining variants don't contribute to the snapshot.
            BotEvent::PlaylistChanged(_)
            | BotEvent::LibraryChanged
            | BotEvent::Error(_) => {}
        }
    }
}

/// In-memory append-only request log. WS-5 ships this as a bounded
/// `Vec` guarded by a `RwLock`; the SurrealDB-backed swap lives in a
/// follow-up under PURA-117. Newest-first reads, oldest-first inserts.
#[derive(Default)]
pub struct RequestLog {
    next_id: AtomicU64,
    inner: RwLock<Vec<wire::MusicRequest>>,
}

/// Bound on the in-memory log so a chatty bridge doesn't grow it without
/// limit. Drops oldest entries first when full.
const REQUEST_LOG_CAP: usize = 1_000;

impl RequestLog {
    pub async fn record(&self, mut req: wire::MusicRequest) -> wire::MusicRequest {
        req.id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut inner = self.inner.write().await;
        if inner.len() >= REQUEST_LOG_CAP {
            inner.remove(0);
        }
        inner.push(req.clone());
        req
    }

    pub async fn list(&self, filter: RequestFilter) -> Vec<wire::MusicRequest> {
        let inner = self.inner.read().await;
        let mut out: Vec<wire::MusicRequest> = inner
            .iter()
            .filter(|r| filter.bot.map_or(true, |b| r.bot == b))
            .filter(|r| {
                filter
                    .requested_by
                    .as_deref()
                    .map_or(true, |q| r.requested_by.as_deref() == Some(q))
            })
            .filter(|r| filter.since.map_or(true, |t| r.requested_at >= t))
            .filter(|r| filter.until.map_or(true, |t| r.requested_at <= t))
            .cloned()
            .collect();
        // Newest-first for the API view.
        out.reverse();
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        out
    }
}

/// Filter args the route layer constructs from query params before
/// calling [`RequestLog::list`].
#[derive(Debug, Default, Clone)]
pub struct RequestFilter {
    pub bot: Option<wire::BotId>,
    pub requested_by: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

async fn watcher_loop(
    bot: BotId,
    mut rx: broadcast::Receiver<BotEvent>,
    liveness: Arc<LivenessTracker>,
    _requests: Arc<RequestLog>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                liveness.record_event(bot, &ev).await;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Mirror is best-effort; if the actor outpaces us we
                // resync on the next event. The actor's state machine
                // is the source of truth.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
