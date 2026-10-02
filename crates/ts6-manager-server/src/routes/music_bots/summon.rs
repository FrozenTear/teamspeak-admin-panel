//! Summon cap for one TeamSpeak server address.
//!
//! Chat requests never reach this process. The music process is what
//! enforces the cap. This route only stores the number the music
//! process has already accepted, keyed by `serverAddr`.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use tracing::{error, warn};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAuth, RequireModerator};
use crate::routes::music_bots::bots::require_server_write;
use crate::routes::music_bots::{internal, map_music_runtime_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/music-summon-caps", get(list).put(set))
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
) -> Result<Json<wire::SummonCapList>, Response> {
    let rows = crate::repos::music_summon_cap::list(&state.db)
        .await
        .map_err(|err| {
            error!(error = %err, "summon cap list failed");
            internal("summon cap list failed")
        })?;
    let mut caps = Vec::with_capacity(rows.len());
    for row in rows {
        let cap = u32::try_from(row.cap)
            .map_err(|_| internal("stored summon cap is outside the range the page can show"))?;
        caps.push(wire::SummonCap {
            server_addr: row.serverAddr,
            cap,
        });
    }
    Ok(Json(wire::SummonCapList { caps }))
}

async fn set(
    State(state): State<AppState>,
    RequireModerator(user): RequireModerator,
    Json(req): Json<wire::SummonCap>,
) -> Result<Json<wire::SummonCap>, Response> {
    if req.server_addr.trim().is_empty() {
        return Err(validation("serverAddr must not be empty"));
    }
    if req.cap > music_bot::MAX_SUMMON_CAP {
        return Err(validation("summon cap is too large"));
    }
    let _server = require_server_write(&state, &user, &req.server_addr).await?;

    state
        .music_bots
        .supervisor
        .set_summon_cap(&req.server_addr, req.cap)
        .await
        .map_err(map_music_runtime_error)?;

    if let Err(err) =
        crate::repos::music_summon_cap::upsert(&state.db, &req.server_addr, req.cap).await
    {
        error!(
            server = %req.server_addr,
            cap = req.cap,
            error = %err,
            "summon cap accepted by the music process but not stored"
        );
        return Err(internal(
            "summon cap was accepted by the music process but was not stored",
        ));
    }

    Ok(Json(wire::SummonCap {
        server_addr: req.server_addr,
        cap: req.cap,
    }))
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
