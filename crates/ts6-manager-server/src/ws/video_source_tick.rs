//! PURA-144 (WS-6) — video-source live status poller.
//!
//! Bridges the sidecar's pull-based `/stats` endpoint and the manager's
//! push-based WS hub. One supervised task that:
//!
//! - polls the sidecar every 2s,
//! - diffs against the previous snapshot,
//! - emits `video_source:update` envelopes on the per-server
//!   `video_sources` topic whenever a tracked source changes state.
//!
//! Cheap because `/stats` is in-memory on the sidecar (see
//! `ts6-media-sidecar::http::stats_handler`). Single task — the sidecar
//! is one process, so a per-server worker pool would be wasteful.
//!
//! No-op when the operator hasn't configured a sidecar (`SIDECAR_URL`
//! unset). The supervisor checks this once at boot and exits cleanly so
//! it doesn't burn cycles polling nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::control::sidecar::{SidecarClient, SourceStats};
use crate::db::Database;
use crate::repos::video_sources;
use crate::ws::Hub;
use crate::ws::topic::{Topic, TopicKind};

const POLL_INTERVAL_SECS: u64 = 2;
/// Emit a heartbeat update for every tracked source at most once per
/// this many ticks even when nothing changed. Lets a freshly-connected
/// operator dashboard catch up on counter values without waiting for
/// the next genuine transition. 10 ticks × 2 s = 20 s heartbeat.
const HEARTBEAT_EVERY_TICKS: u64 = 10;
const UPDATE_KIND: &str = "video_source:update";

#[derive(Clone)]
pub struct VideoTickDeps {
    pub db: Arc<Database>,
    pub hub: Hub,
    pub sidecar: SidecarClient,
}

pub struct VideoTickHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl VideoTickHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
    }
}

/// Spawn the poller. `None` when `deps.sidecar` is unset (operator did
/// not configure the sidecar) — main owns the optional wiring.
pub fn spawn(deps: VideoTickDeps) -> VideoTickHandle {
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut state: HashMap<String, SourceState> = HashMap::new();
        let mut tick_count: u64 = 0;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tick_count = tick_count.wrapping_add(1);
                    if let Err(e) = poll_once(&deps, &mut state, tick_count).await {
                        tracing::debug!(error = %e, "video_source_tick poll failed");
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
        tracing::info!("video_source_tick supervisor shut down");
    });
    VideoTickHandle { shutdown_tx, join }
}

#[derive(Clone, Debug)]
struct SourceState {
    server_config_id: i64,
    db_id: i64,
    label: String,
    preset: String,
    last_status: String,
    last_video_alive: bool,
    last_audio_alive: bool,
    last_emit_tick: u64,
}

async fn poll_once(
    deps: &VideoTickDeps,
    state: &mut HashMap<String, SourceState>,
    tick_count: u64,
) -> anyhow::Result<()> {
    let stats = match deps.sidecar.get_stats().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "sidecar /stats unreachable");
            return Ok(());
        }
    };

    for source in &stats.sources {
        let computed = compute_status(source);
        let prior = state.get(&source.source_id).cloned();

        // Make sure we know what server this source belongs to. Look it
        // up on first sight; cache so subsequent ticks don't re-query.
        let entry = match prior {
            Some(p) => p,
            None => match video_sources::find_by_source_id(&deps.db, &source.source_id).await? {
                Some(row) => SourceState {
                    server_config_id: row.serverConfigId,
                    db_id: row.id,
                    label: row.label,
                    preset: row.preset,
                    last_status: row.status,
                    last_video_alive: false,
                    last_audio_alive: false,
                    last_emit_tick: 0,
                },
                None => {
                    // Sidecar is publishing a source we don't track — could
                    // be a manually-issued POST against the sidecar, or a
                    // row that was deleted but the pipeline survived.
                    // Skip silently; the operator can issue DELETE via the
                    // sidecar directly or restart.
                    continue;
                }
            },
        };

        let video_alive = source.video.ffmpeg_alive;
        let audio_alive = source.audio.ffmpeg_alive;
        let changed_alive =
            video_alive != entry.last_video_alive || audio_alive != entry.last_audio_alive;
        let status_changed = computed != entry.last_status;
        let heartbeat_due =
            tick_count.saturating_sub(entry.last_emit_tick) >= HEARTBEAT_EVERY_TICKS;

        if status_changed
            && let Err(e) = video_sources::update_status(&deps.db, entry.db_id, &computed).await
        {
            tracing::warn!(error = %e, source_id = %source.source_id, "video_source status persist failed");
        }

        if changed_alive || status_changed || heartbeat_due {
            let topic = Topic::new(entry.server_config_id, TopicKind::VideoSources);
            let payload = json!({
                "id": entry.db_id,
                "source_id": source.source_id,
                "label": entry.label,
                "preset": entry.preset,
                "server_id": entry.server_config_id,
                "status": computed,
                "video": {
                    "frames_published": source.video.frames_published,
                    "bytes_published": source.video.bytes_published,
                    "ffmpeg_alive": video_alive,
                },
                "audio": {
                    "frames_published": source.audio.frames_published,
                    "bytes_published": source.audio.bytes_published,
                    "ffmpeg_alive": audio_alive,
                },
            });
            deps.hub.publish(topic, UPDATE_KIND, payload).await;
        }

        state.insert(
            source.source_id.clone(),
            SourceState {
                last_status: computed,
                last_video_alive: video_alive,
                last_audio_alive: audio_alive,
                last_emit_tick: tick_count,
                ..entry
            },
        );
    }

    // Cull state for sources the sidecar no longer reports (stopped
    // manually). Their DB rows may still exist but we don't need to
    // track them until they reappear.
    let alive_ids: std::collections::HashSet<&str> =
        stats.sources.iter().map(|s| s.source_id.as_str()).collect();
    state.retain(|k, _| alive_ids.contains(k.as_str()));

    Ok(())
}

/// Derive the high-level pipeline status from the per-track metrics.
/// See `VideoSource::status` doc comment for the state-machine
/// semantics.
fn compute_status(source: &SourceStats) -> String {
    let video_alive = source.video.ffmpeg_alive;
    let audio_alive = source.audio.ffmpeg_alive;
    let has_frames = source.video.frames_published > 0 || source.audio.frames_published > 0;
    if video_alive || audio_alive {
        "live".into()
    } else if has_frames {
        // Both tracks reported alive at some point (frames > 0) but
        // are now dead — the pipeline previously came up and then
        // failed.
        "failed".into()
    } else {
        // No frames yet — pipeline still warming up.
        "starting".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::sidecar::TrackStats;

    fn src(video_alive: bool, audio_alive: bool, vframes: u64, aframes: u64) -> SourceStats {
        SourceStats {
            source_id: "s".into(),
            preset: "720p".into(),
            video: TrackStats {
                frames_published: vframes,
                bytes_published: 0,
                ffmpeg_alive: video_alive,
            },
            audio: TrackStats {
                frames_published: aframes,
                bytes_published: 0,
                ffmpeg_alive: audio_alive,
            },
        }
    }

    #[test]
    fn status_starting_when_no_frames_and_dead() {
        assert_eq!(compute_status(&src(false, false, 0, 0)), "starting");
    }

    #[test]
    fn status_live_when_any_track_alive() {
        assert_eq!(compute_status(&src(true, false, 0, 0)), "live");
        assert_eq!(compute_status(&src(false, true, 0, 0)), "live");
        assert_eq!(compute_status(&src(true, true, 100, 200)), "live");
    }

    #[test]
    fn status_failed_when_dead_after_frames() {
        assert_eq!(compute_status(&src(false, false, 100, 0)), "failed");
        assert_eq!(compute_status(&src(false, false, 0, 50)), "failed");
    }
}
