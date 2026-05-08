//! Channel-scoped control endpoints — PURA-71.
//!
//! `GET /api/servers/{configId}/vs/{sid}/channels` — flat channel list with
//! the spec §7.7 flag set (`-topic -flags -voice -limits -icon -secondsempty`).
//! The FE assembles a tree from `pid` / `channel_order`. Phase 2 does not
//! ship channel-create / edit / delete — those land alongside the FE
//! channel admin page in a separate child issue.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Response;
use ts6_manager_shared::control::ChannelTreeNode;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

use super::{access, translate_webquery_error};

/// Spec §7.7 flag set — required at the REST layer per the deviations
/// table in [`crate::webquery::models::ChannelEntry`].
const CHANNEL_FLAGS: &[&str] = &["topic", "flags", "voice", "limits", "icon", "secondsempty"];

pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<ChannelTreeNode>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;

    let rows = client
        .channellist_with_flags(sid, CHANNEL_FLAGS)
        .await
        .map_err(translate_webquery_error)?;
    let projected: Vec<ChannelTreeNode> = rows
        .into_iter()
        .map(|c| ChannelTreeNode {
            cid: c.cid,
            pid: c.pid,
            channel_name: c.channel_name,
            channel_order: c.channel_order,
            channel_topic: c.channel_topic,
            channel_flag_default: c.channel_flag_default,
            channel_flag_password: c.channel_flag_password,
            channel_flag_permanent: c.channel_flag_permanent,
            channel_flag_semi_permanent: c.channel_flag_semi_permanent,
            channel_maxclients: c.channel_maxclients,
            channel_maxfamilyclients: c.channel_maxfamilyclients,
            total_clients: c.total_clients,
            total_clients_family: c.total_clients_family,
            channel_icon_id: c.channel_icon_id,
            seconds_empty: c.seconds_empty,
            channel_needed_subscribe_power: c.channel_needed_subscribe_power,
        })
        .collect();
    Ok(Json(projected))
}
