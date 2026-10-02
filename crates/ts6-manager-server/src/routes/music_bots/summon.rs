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
    let _save = state.music_bots.lock_summon_save(&server_addr).await;

    let previous = crate::repos::music_summon_cap::get(&state.db, &server_addr)
        .await
        .map_err(|err| {
            error!(server = %server_addr, error = %err, "summon cap lookup failed");
            internal("summon cap lookup failed")
        })?;

    let mut unsettled = UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server_addr.clone(),
        previous,
        settled: false,
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
        restore_stored(&state, &server_addr, previous).await;
        unsettled.settled = true;
        return Err(map_music_runtime_error(err));
    }

    if let Err(err) = crate::repos::music_summon_cap::upsert(&state.db, &server_addr, req.cap).await
    {
        error!(
            server = %server_addr,
            cap = req.cap,
            error = %err,
            "summon cap was not stored; restoring the stored number"
        );
        restore_stored(&state, &server_addr, previous).await;
        unsettled.settled = true;
        return Err(internal("summon cap was not stored"));
    }

    unsettled.settled = true;
    Ok(Json(wire::SummonCap {
        server_addr,
        cap: req.cap,
    }))
}

/// Put the music process on the number the database has now. A failed
/// read falls back to the value this save observed under the lock.
async fn restore_stored(state: &AppState, server: &str, fallback: Option<u32>) {
    let stored = match crate::repos::music_summon_cap::get(&state.db, server).await {
        Ok(cap) => cap,
        Err(err) => {
            error!(server = %server, error = %err, "summon cap re-read failed");
            fallback
        }
    };
    if let Err(err) = restore_with_retry(&state.music_bots.supervisor, server, stored).await {
        error!(
            server = %server,
            error = %err,
            "summon cap restore failed"
        );
    }
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
/// put the process back on whatever the database currently has.
struct UnsettledCap {
    front: MusicBotFront,
    db: std::sync::Arc<Database>,
    server: String,
    previous: Option<u32>,
    settled: bool,
}

impl Drop for UnsettledCap {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let front = self.front.clone();
        let db = self.db.clone();
        let server = std::mem::take(&mut self.server);
        let previous = self.previous;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let stored = crate::repos::music_summon_cap::get(&db, &server)
                    .await
                    .unwrap_or(previous);
                if let Err(err) = restore_with_retry(&front, &server, stored).await {
                    error!(server = %server, error = %err, "summon cap restore failed");
                }
            });
        }
    }
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
