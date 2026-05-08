//! `GET /api/servers/{configId}/vs/{sid}/info` — `serverinfo` passthrough.
//! PURA-71.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Response;
use ts6_manager_shared::control::ServerInfoResponse;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

use super::{access, translate_webquery_error};

pub async fn server_info(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<ServerInfoResponse>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;
    let info = client
        .serverinfo(sid)
        .await
        .map_err(translate_webquery_error)?;
    Ok(Json(ServerInfoResponse {
        virtualserver_name: info.virtualserver_name,
        virtualserver_platform: info.virtualserver_platform,
        virtualserver_version: info.virtualserver_version,
        virtualserver_maxclients: info.virtualserver_maxclients,
        virtualserver_uptime: info.virtualserver_uptime,
        virtualserver_total_packetloss_total: info.virtualserver_total_packetloss_total,
        virtualserver_total_ping: info.virtualserver_total_ping,
    }))
}
