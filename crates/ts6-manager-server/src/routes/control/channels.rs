//! Channel-scoped control endpoints — spec §7.7.
//!
//! - `GET    /api/servers/{configId}/vs/{sid}/channels`         — flat list
//!   with the §7.7 flag set (`-topic -flags -voice -limits -icon
//!   -secondsempty`). The FE assembles a tree from `pid` / `channel_order`.
//! - `POST   /api/servers/{configId}/vs/{sid}/channels`         — `channelcreate`.
//! - `PUT    /api/servers/{configId}/vs/{sid}/channels/{cid}`   — `channeledit`.
//! - `DELETE /api/servers/{configId}/vs/{sid}/channels/{cid}`   — `channeldelete`
//!   (`?force=0|1`, default `1`).
//! - `POST   /api/servers/{configId}/vs/{sid}/channels/{cid}/move` — `channelmove`.
//!
//! Reads use [`access::check_read`]. Writes are admin-only via
//! [`access::check_admin`] (spec §7.7 "Y+admin" rows). Mutations publish
//! on `server:{configId}:channels` using the same `ts:channel:*` kinds
//! the SSH notify bridge already emits.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{
    ChannelCreateRequest, ChannelCreated, ChannelDeleteQuery, ChannelEditRequest,
    ChannelMoveRequest, ChannelProperties, ChannelTreeNode,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireServerAccess;
use crate::control::ControlBackendError;
use crate::repos::server_connections::ServerConnection;
use crate::webquery::ChannelWriteParams;
use crate::ws::topic::{Topic, TopicKind};

use super::{access, audit, bad_request, translate_control_error};

/// Spec §7.7 flag set — required at the REST layer per the deviations
/// table in [`crate::webquery::models::ChannelEntry`].
const CHANNEL_FLAGS: &[&str] = &["topic", "flags", "voice", "limits", "icon", "secondsempty"];

pub async fn list(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<ChannelTreeNode>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;

    let rows = client
        .channellist_with_flags(sid, CHANNEL_FLAGS)
        .await
        .map_err(translate_control_error)?;
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

/// `POST ` — `channelcreate`.
pub async fn create(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<ChannelCreateRequest>,
) -> Result<(StatusCode, Json<ChannelCreated>), Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.channel_name.trim().is_empty() {
        return Err(bad_request("channel name must not be empty"));
    }
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;
    let params = create_params(&req);
    let started = Instant::now();
    let action = "channel.create";
    let details = format!("name={:?} cpid={:?}", req.channel_name.as_str(), req.cpid);
    match client.channelcreate(sid, &params).await {
        Ok(cid) => {
            emit_success(
                &user,
                &connection,
                sid,
                action,
                Some(cid),
                &details,
                started,
            );
            publish(
                &state,
                config_id,
                "ts:channel:created",
                json!({ "cid": cid, "channelName": req.channel_name, "cpid": req.cpid }),
            )
            .await;
            Ok((StatusCode::CREATED, Json(ChannelCreated { cid })))
        }
        Err(e) => Err(emit_failure(
            &user,
            &connection,
            sid,
            action,
            None,
            &details,
            e,
            started,
        )),
    }
}

/// `PUT :cid` — `channeledit`.
pub async fn edit(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid, cid)): Path<(i64, i64, i64)>,
    Json(req): Json<ChannelEditRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.is_empty() {
        return Err(bad_request(
            "channel edit body must set at least one property",
        ));
    }
    if req
        .channel_name
        .as_deref()
        .is_some_and(|n| n.trim().is_empty())
    {
        return Err(bad_request("channel name must not be empty"));
    }
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;
    let params = edit_params(&req);
    let started = Instant::now();
    let action = "channel.edit";
    let details = format!("cid={cid} name={:?}", req.channel_name);
    match client.channeledit(sid, cid, &params).await {
        Ok(()) => {
            emit_success(
                &user,
                &connection,
                sid,
                action,
                Some(cid),
                &details,
                started,
            );
            publish(
                &state,
                config_id,
                "ts:channel:edited",
                json!({ "cid": cid, "channelName": req.channel_name }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure(
            &user,
            &connection,
            sid,
            action,
            Some(cid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :cid` — `channeldelete` (`?force=0|1`, default `1`).
pub async fn delete(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid, cid)): Path<(i64, i64, i64)>,
    Query(q): Query<ChannelDeleteQuery>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let force = match q.force {
        None => true,
        Some(1) => true,
        Some(0) => false,
        Some(other) => {
            return Err(bad_request(&format!("force must be 0 or 1 (got {other})")));
        }
    };
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;
    let started = Instant::now();
    let action = "channel.delete";
    let details = format!("cid={cid} force={}", if force { 1 } else { 0 });
    match client.channeldelete(sid, cid, force).await {
        Ok(()) => {
            emit_success(
                &user,
                &connection,
                sid,
                action,
                Some(cid),
                &details,
                started,
            );
            publish(
                &state,
                config_id,
                "ts:channel:deleted",
                json!({ "cid": cid, "force": if force { 1 } else { 0 } }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure(
            &user,
            &connection,
            sid,
            action,
            Some(cid),
            &details,
            e,
            started,
        )),
    }
}

/// `POST :cid/move` — `channelmove`.
pub async fn move_channel(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid, cid)): Path<(i64, i64, i64)>,
    Json(req): Json<ChannelMoveRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;
    let started = Instant::now();
    let action = "channel.move";
    let details = format!("cid={cid} cpid={} order={:?}", req.cpid, req.order);
    match client.channelmove(sid, cid, req.cpid, req.order).await {
        Ok(()) => {
            emit_success(
                &user,
                &connection,
                sid,
                action,
                Some(cid),
                &details,
                started,
            );
            publish(
                &state,
                config_id,
                "ts:channel:moved",
                json!({ "cid": cid, "cpid": req.cpid, "order": req.order }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure(
            &user,
            &connection,
            sid,
            action,
            Some(cid),
            &details,
            e,
            started,
        )),
    }
}

fn create_params(req: &ChannelCreateRequest) -> ChannelWriteParams<'_> {
    ChannelWriteParams {
        channel_name: Some(req.channel_name.as_str()),
        cpid: req.cpid,
        ..props_params(&req.properties)
    }
}

fn edit_params(req: &ChannelEditRequest) -> ChannelWriteParams<'_> {
    ChannelWriteParams {
        channel_name: req.channel_name.as_deref(),
        cpid: None,
        ..props_params(&req.properties)
    }
}

fn props_params(p: &ChannelProperties) -> ChannelWriteParams<'_> {
    ChannelWriteParams {
        channel_name: None,
        cpid: None,
        channel_topic: p.channel_topic.as_deref(),
        channel_password: p.channel_password.as_deref(),
        channel_description: p.channel_description.as_deref(),
        channel_maxclients: p.channel_maxclients,
        channel_maxfamilyclients: p.channel_maxfamilyclients,
        channel_order: p.channel_order,
        channel_flag_permanent: p.channel_flag_permanent,
        channel_flag_semi_permanent: p.channel_flag_semi_permanent,
        channel_flag_temporary: p.channel_flag_temporary,
        channel_flag_default: p.channel_flag_default,
        channel_needed_talk_power: p.channel_needed_talk_power,
        channel_icon_id: p.channel_icon_id,
    }
}

fn emit_success(
    user: &crate::auth::extractors::AuthUser,
    connection: &ServerConnection,
    sid: i64,
    action: &'static str,
    target_id: Option<i64>,
    details: &str,
    started: Instant,
) {
    audit::AuditEntry::success(
        connection.id,
        sid,
        user.id,
        &user.username,
        action,
        target_id,
        details,
        started.elapsed(),
    )
    .emit();
}

#[allow(clippy::too_many_arguments)]
fn emit_failure(
    user: &crate::auth::extractors::AuthUser,
    connection: &ServerConnection,
    sid: i64,
    action: &'static str,
    target_id: Option<i64>,
    details: &str,
    err: ControlBackendError,
    started: Instant,
) -> Response {
    let elapsed = started.elapsed();
    let entry = match &err {
        ControlBackendError::Upstream { code, message } => audit::AuditEntry::upstream_error(
            connection.id,
            sid,
            user.id,
            &user.username,
            action,
            target_id,
            details,
            *code,
            message.clone(),
            elapsed,
        ),
        other => audit::AuditEntry::transport(
            connection.id,
            sid,
            user.id,
            &user.username,
            action,
            target_id,
            details,
            other.to_string(),
            elapsed,
        ),
    };
    entry.emit();
    translate_control_error(err)
}

async fn publish(state: &AppState, config_id: i64, kind: &'static str, data: serde_json::Value) {
    let topic = Topic::new(config_id, TopicKind::Channels);
    let _ = state.ws_hub.publish(topic, kind, data).await;
}
