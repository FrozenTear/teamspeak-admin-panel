//! Summon cap for one TeamSpeak server address.
//!
//! Chat requests never reach this process. The music process is what
//! enforces the cap. This route only stores the number the music
//! process has already accepted, keyed by `serverAddr`.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use tracing::{error, warn};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAuth, RequireModerator};
use crate::db::Database;
use crate::music_runtime::MusicBotFront;
use crate::routes::music_bots::bots::{bot_visibility, require_server_write};
use crate::routes::music_bots::{internal, map_music_runtime_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/music-summon-caps", get(list).put(set))
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
) -> Result<Json<wire::SummonCapList>, Response> {
    let rows = crate::repos::music_summon_cap::list(&state.db)
        .await
        .map_err(|err| {
            error!(error = %err, "summon cap list failed");
            internal("summon cap list failed")
        })?;
    let mut caps = Vec::with_capacity(rows.len());
    let mut seen = BTreeSet::new();
    for row in rows {
        let server_addr = music_bot::canon_server_addr(&row.serverAddr);
        if server_addr.is_empty() || !seen.insert(server_addr.clone()) {
            continue;
        }
        if bot_visibility(&state, &user, &server_addr).await?.is_none() {
            continue;
        }
        let cap = u32::try_from(row.cap)
            .map_err(|_| internal("stored summon cap is outside the range the page can show"))?;
        caps.push(wire::SummonCap { server_addr, cap });
    }
    Ok(Json(wire::SummonCapList { caps }))
}

async fn set(
    State(state): State<AppState>,
    RequireModerator(user): RequireModerator,
    Json(req): Json<wire::SummonCap>,
) -> Result<Json<wire::SummonCap>, Response> {
    let server_addr = music_bot::canon_server_addr(&req.server_addr);
    if server_addr.is_empty() {
        return Err(validation("serverAddr must not be empty"));
    }
    if req.cap > music_bot::MAX_SUMMON_CAP {
        return Err(validation("summon cap is too large"));
    }
    let _server = require_server_write(&state, &user, &server_addr).await?;
    let mutex = state.music_bots.summon_save_mutex(&server_addr);
    let guard = mutex.clone().lock_owned().await;

    if let Err(err) = crate::repos::music_summon_cap::get(&state.db, &server_addr).await {
        error!(server = %server_addr, error = %err, "summon cap lookup failed");
        return Err(internal("summon cap lookup failed"));
    }

    let mut unsettled = UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server_addr.clone(),
        forwarded: None,
        settled: false,
        mutex: Some(mutex),
        guard: Some(guard),
    };

    if let Err(err) = state
        .music_bots
        .supervisor
        .set_summon_cap(&server_addr, req.cap)
        .await
    {
        error!(
            server = %server_addr,
            cap = req.cap,
            error = %err,
            "summon cap forward failed; restoring the stored number"
        );
        align_held(&mut unsettled).await;
        unsettled.settled = true;
        return Err(map_music_runtime_error(err));
    }
    unsettled.forwarded = Some(req.cap);

    if let Err(err) = crate::repos::music_summon_cap::upsert(&state.db, &server_addr, req.cap).await
    {
        error!(
            server = %server_addr,
            cap = req.cap,
            error = %err,
            "summon cap was not stored; restoring the stored number"
        );
        align_held(&mut unsettled).await;
        unsettled.settled = true;
        return Err(internal("summon cap was not stored"));
    }

    unsettled.settled = true;
    Ok(Json(wire::SummonCap {
        server_addr,
        cap: req.cap,
    }))
}

/// Re-read the stored cap and put the process on that number, while
/// `guard` is held. A failed read is retried. It is not replaced with a
/// number captured before this call. After five failed restores of a
/// number the page does not show, store the forwarded number so the page
/// matches the process.
async fn align_held(unsettled: &mut UnsettledCap) {
    let Some(mutex) = unsettled.mutex.clone() else {
        return;
    };
    let guard = unsettled.guard.take();
    align_cap(
        unsettled.front.clone(),
        unsettled.db.clone(),
        unsettled.server.clone(),
        unsettled.forwarded,
        mutex,
        guard,
    )
    .await;
}

async fn restore_with_retry(
    front: &MusicBotFront,
    server: &str,
    cap: Option<u32>,
) -> Result<(), crate::music_runtime::MusicRuntimeError> {
    let mut wait = Duration::from_millis(20);
    let mut last = None;
    for _ in 0..5 {
        match front.restore_summon_cap(server, cap).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
            }
        }
    }
    Err(last.expect("restore retried"))
}

/// If this save is dropped after the forward and before it settles,
/// put the process and the store on the same number. The restore runs
/// while the per-server lock is still held.
pub(super) struct UnsettledCap {
    front: MusicBotFront,
    db: std::sync::Arc<Database>,
    server: String,
    /// Set only after the music process accepted this save's number.
    forwarded: Option<u32>,
    settled: bool,
    mutex: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for UnsettledCap {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let Some(mutex) = self.mutex.take() else {
            return;
        };
        let guard = self.guard.take();
        let front = self.front.clone();
        let db = self.db.clone();
        let server = std::mem::take(&mut self.server);
        let forwarded = self.forwarded;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                align_cap(front, db, server, forwarded, mutex, guard).await;
            });
        }
    }
}

const ALIGN_CYCLES: u32 = 3;

async fn align_cap(
    front: MusicBotFront,
    db: std::sync::Arc<Database>,
    server: String,
    forwarded: Option<u32>,
    mutex: std::sync::Arc<tokio::sync::Mutex<()>>,
    mut guard: Option<tokio::sync::OwnedMutexGuard<()>>,
) {
    for cycle in 0..ALIGN_CYCLES {
        if guard.is_none() {
            guard = Some(mutex.clone().lock_owned().await);
        }
        match read_cap_retry(&db, &server).await {
            Ok(stored) => {
                if restore_with_retry(&front, &server, stored).await.is_ok() {
                    return;
                }
                if let Some(live) = forwarded {
                    if Some(live) == stored {
                        return;
                    }
                    match crate::repos::music_summon_cap::upsert(&db, &server, live).await {
                        Ok(()) => return,
                        Err(err) => {
                            error!(
                                server = %server,
                                error = %err,
                                "summon cap page was not aligned to the process"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                error!(
                    server = %server,
                    error = %err,
                    "summon cap re-read failed; the captured number is not restored"
                );
            }
        }
        drop(guard.take());
        if cycle + 1 < ALIGN_CYCLES {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    error!(server = %server, "summon cap process and store were not aligned");
}

async fn read_cap_retry(db: &Database, server: &str) -> Result<Option<u32>, anyhow::Error> {
    let mut wait = Duration::from_millis(20);
    let mut last = None;
    for _ in 0..5 {
        match crate::repos::music_summon_cap::get(db, server).await {
            Ok(cap) => return Ok(cap),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
            }
        }
    }
    Err(last.expect("cap re-read retried"))
}

/// A save that forwarded `cap` and then stopped before the store write.
/// Dropping the guard aligns the process to the stored number under the
/// lock. Tests use this for a handler that does not return.
#[cfg(test)]
pub(super) async fn hold_forwarded_cap(state: &AppState, server: &str, cap: u32) -> UnsettledCap {
    let mutex = state.music_bots.summon_save_mutex(server);
    let guard = mutex.clone().lock_owned().await;
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, cap)
        .await
        .expect("test forward");
    UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server.to_string(),
        forwarded: Some(cap),
        settled: false,
        mutex: Some(mutex),
        guard: Some(guard),
    }
}

/// Align once, as a dropped save would, against `front`. `forwarded` is
/// the number that front already accepted. The database is `state.db`.
#[cfg(test)]
pub(super) async fn align_cap_for_test(
    state: &AppState,
    front: MusicBotFront,
    server: &str,
    forwarded: Option<u32>,
) {
    let mutex = state.music_bots.summon_save_mutex(server);
    let guard = mutex.clone().lock_owned().await;
    align_cap(
        front,
        state.db.clone(),
        server.to_string(),
        forwarded,
        mutex,
        Some(guard),
    )
    .await;
}

/// Cap to carry on a saved-bot push. A lookup error is `None`: the push
/// must not invent a number and must not fail bot create.
pub(super) async fn cap_for_push(state: &AppState, server_addr: &str) -> Option<u32> {
    match crate::repos::music_summon_cap::get(&state.db, server_addr).await {
        Ok(cap) => cap,
        Err(err) => {
            warn!(
                server = %server_addr,
                error = %err,
                "summon cap lookup failed; this push will not arm summon"
            );
            None
        }
    }
}
