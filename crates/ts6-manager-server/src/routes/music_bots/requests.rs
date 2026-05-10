//! `/music-requests` — request log read API (PURA-123 WS-5).
//!
//! Read-only. Rows land here as a side-effect of WS-4 chat commands
//! and the `/playlists/{}/enqueue` + `/radio-stations/{}/play`
//! shortcuts; this surface is purely a query.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::music_bots::RequestFilter;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/music-requests", get(list))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestQuery {
    #[serde(default)]
    bot: Option<wire::BotId>,
    #[serde(default)]
    requested_by: Option<String>,
    #[serde(default)]
    since: Option<DateTime<Utc>>,
    #[serde(default)]
    until: Option<DateTime<Utc>>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Query(q): Query<RequestQuery>,
) -> Result<Json<Vec<wire::MusicRequest>>, Response> {
    let filter = RequestFilter {
        bot: q.bot,
        requested_by: q.requested_by,
        since: q.since,
        until: q.until,
        limit: q.limit,
    };
    let rows = state.music_bots.requests.list(filter).await;
    Ok(Json(rows))
}
