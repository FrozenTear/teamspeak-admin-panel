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
    /// PURA-347 — elapsed playback position of `now_playing`, in whole
    /// seconds. Mirrored from `BotEvent::Progress` (the truthful play
    /// clock); reset to `Some(0)` on a fresh `NowPlaying` and cleared to
    /// `None` whenever playback stops. `None` while no track is playing
    /// or before the first one-second tick.
    pub now_playing_elapsed_secs: Option<u64>,
    pub channel_id: Option<ChannelId>,
    /// PURA-261 — cause of the most recent playback failure, if the bot
    /// is not currently playing a track. Set when an audio pipeline ends
    /// without producing audio (bad URL, bot-gated video, codec error);
    /// cleared when a new track starts or the bot reconnects. The route
    /// layer surfaces this so operators see *why* playback stopped
    /// instead of a silently-stuck `Playing` state.
    pub last_error: Option<String>,
}

impl Default for BotLiveness {
    fn default() -> Self {
        Self {
            state: BotState::Disconnected,
            now_playing: None,
            now_playing_elapsed_secs: None,
            channel_id: None,
            last_error: None,
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
                    entry.now_playing_elapsed_secs = None;
                    entry.last_error = None;
                }
            }
            BotEvent::Connected {
                default_channel, ..
            } => {
                entry.channel_id = Some(*default_channel);
                entry.last_error = None;
            }
            BotEvent::Disconnected { .. } => {
                entry.channel_id = None;
                entry.now_playing = None;
                entry.now_playing_elapsed_secs = None;
                entry.last_error = None;
            }
            BotEvent::JoinedChannel { channel_id } => {
                entry.channel_id = Some(*channel_id);
            }
            BotEvent::LeftChannel => {
                entry.channel_id = None;
            }
            BotEvent::NowPlaying(track) => {
                entry.now_playing = Some(track.clone());
                // PURA-347 — a fresh track restarts the play clock at 0.
                // `BotEvent::Progress` ticks bump it once per second.
                entry.now_playing_elapsed_secs = Some(0);
                // A fresh track supersedes any prior failure.
                entry.last_error = None;
            }
            // PURA-347 — playback-progress tick. Only meaningful while a
            // track is playing; the surrounding `NowPlaying` /
            // `AudioFinished` events bracket it, so a stray tick after a
            // finish is harmless (the next finish clears it again).
            BotEvent::Progress { elapsed_secs } => {
                if entry.now_playing.is_some() {
                    entry.now_playing_elapsed_secs = Some(*elapsed_secs);
                }
            }
            BotEvent::QueueEmpty => {
                entry.now_playing = None;
                entry.now_playing_elapsed_secs = None;
            }
            BotEvent::QueueChanged { current, .. } => {
                entry.now_playing = current.clone();
                // Drop a stale elapsed when the queue head clears; a new
                // head's clock resets when its `NowPlaying` fires.
                if current.is_none() {
                    entry.now_playing_elapsed_secs = None;
                }
            }
            // PURA-261 — an audio pipeline ended. Clear `now_playing`
            // so the route layer stops synthesising `Playing`; any
            // auto-advance `NowPlaying` for the next track is emitted
            // *after* this event and re-sets it. A `failed: ` reason
            // prefix means the pipeline produced no audio — surface the
            // cause as `last_error`; a clean finish clears it.
            BotEvent::AudioFinished { reason } => {
                entry.now_playing = None;
                entry.now_playing_elapsed_secs = None;
                entry.last_error = reason
                    .strip_prefix("failed: ")
                    .map(|cause| cause.to_string());
            }
            // The remaining variants don't contribute to the snapshot.
            // THE-927 — `Resolving` / `FirstFrameOnWire` are SSE-only
            // animation hints, not part of the persistent liveness state.
            BotEvent::PlaylistChanged(_)
            | BotEvent::LibraryChanged
            | BotEvent::Resolving { .. }
            | BotEvent::FirstFrameOnWire
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
            .filter(|r| filter.bot.is_none_or(|b| r.bot == b))
            .filter(|r| {
                filter
                    .requested_by
                    .as_deref()
                    .is_none_or(|q| r.requested_by.as_deref() == Some(q))
            })
            .filter(|r| filter.since.is_none_or(|t| r.requested_at >= t))
            .filter(|r| filter.until.is_none_or(|t| r.requested_at <= t))
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

#[cfg(test)]
mod tests {
    use super::*;
    use music_bot::{AudioSource, TrackId};

    fn track(title: &str) -> Track {
        Track {
            id: TrackId(0),
            source: AudioSource::Url("https://example.com/x".into()),
            title: title.into(),
            duration_secs: None,
            requested_by: None,
        }
    }

    /// PURA-261 — a pipeline that produced no audio must drop the bot
    /// out of the synthesised `Playing` state and surface the cause.
    #[tokio::test]
    async fn failed_audio_finish_clears_now_playing_and_records_last_error() {
        let tracker = LivenessTracker::default();
        let bot = BotId(1);

        tracker
            .record_event(bot, &BotEvent::NowPlaying(track("bad video")))
            .await;
        assert!(tracker.snapshot(bot).await.now_playing.is_some());

        tracker
            .record_event(
                bot,
                &BotEvent::AudioFinished {
                    reason: "failed: audio pipeline produced 0 frames — check yt-dlp/ffmpeg logs"
                        .into(),
                },
            )
            .await;

        let snap = tracker.snapshot(bot).await;
        assert!(
            snap.now_playing.is_none(),
            "a failed track must not keep reporting Playing"
        );
        assert_eq!(
            snap.last_error.as_deref(),
            Some("audio pipeline produced 0 frames — check yt-dlp/ffmpeg logs"),
        );
    }

    /// A clean end-of-stream clears `now_playing` but leaves no error.
    #[tokio::test]
    async fn clean_audio_finish_clears_now_playing_without_error() {
        let tracker = LivenessTracker::default();
        let bot = BotId(2);

        tracker
            .record_event(bot, &BotEvent::NowPlaying(track("good video")))
            .await;
        tracker
            .record_event(
                bot,
                &BotEvent::AudioFinished {
                    reason: "end_of_stream".into(),
                },
            )
            .await;

        let snap = tracker.snapshot(bot).await;
        assert!(snap.now_playing.is_none());
        assert!(snap.last_error.is_none());
    }

    /// A fresh `NowPlaying` (e.g. auto-advance to the next track, or a
    /// retry) supersedes a prior failure.
    #[tokio::test]
    async fn new_track_clears_stale_last_error() {
        let tracker = LivenessTracker::default();
        let bot = BotId(3);

        tracker
            .record_event(
                bot,
                &BotEvent::AudioFinished {
                    reason: "failed: audio send error — boom".into(),
                },
            )
            .await;
        assert!(tracker.snapshot(bot).await.last_error.is_some());

        tracker
            .record_event(bot, &BotEvent::NowPlaying(track("next track")))
            .await;
        let snap = tracker.snapshot(bot).await;
        assert!(snap.now_playing.is_some());
        assert!(snap.last_error.is_none());
    }

    /// PURA-347 — a fresh `NowPlaying` anchors the play clock at 0,
    /// `Progress` ticks bump it, and a finish clears it back to `None`.
    #[tokio::test]
    async fn progress_ticks_track_elapsed_and_clear_on_finish() {
        let tracker = LivenessTracker::default();
        let bot = BotId(4);

        tracker
            .record_event(bot, &BotEvent::NowPlaying(track("a song")))
            .await;
        assert_eq!(
            tracker.snapshot(bot).await.now_playing_elapsed_secs,
            Some(0),
            "a fresh track starts the elapsed clock at 0",
        );

        tracker
            .record_event(bot, &BotEvent::Progress { elapsed_secs: 7 })
            .await;
        assert_eq!(
            tracker.snapshot(bot).await.now_playing_elapsed_secs,
            Some(7),
        );

        tracker
            .record_event(
                bot,
                &BotEvent::AudioFinished {
                    reason: "end_of_stream".into(),
                },
            )
            .await;
        assert_eq!(
            tracker.snapshot(bot).await.now_playing_elapsed_secs,
            None,
            "elapsed clears when playback stops",
        );
    }

    /// A `Progress` tick with nothing playing is ignored — it cannot
    /// resurrect a stale elapsed after a finish.
    #[tokio::test]
    async fn progress_without_now_playing_is_ignored() {
        let tracker = LivenessTracker::default();
        let bot = BotId(5);

        tracker
            .record_event(bot, &BotEvent::Progress { elapsed_secs: 3 })
            .await;
        assert_eq!(tracker.snapshot(bot).await.now_playing_elapsed_secs, None);
    }
}
