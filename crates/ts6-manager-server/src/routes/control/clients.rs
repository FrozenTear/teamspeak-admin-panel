//! Client-scoped control endpoints — PURA-71, migrated to the
//! backend-agnostic [`ControlBackend`] dispatch in PURA-99.
//!
//! Read endpoints:
//! - `GET /api/servers/{configId}/vs/{sid}/clients` — clientlist with the
//!   spec §7.8 flag set. `-ip` only for admin callers.
//! - `GET /api/servers/{configId}/vs/{sid}/clients/{cldbid}` — `clientdbinfo`
//!   passthrough, augmented with a `liveClient` field when the database
//!   client is currently online (single `clientlist` scan + `clientinfo`
//!   round-trip).
//!
//! Write endpoints (kick / mute / unmute / move) all:
//! - run `access::check_write` for RBAC,
//! - call the typed [`ControlBackend`] write,
//! - publish a §8.4 event on `server:{configId}:clients` via the WS hub,
//! - emit a `control::audit` log entry.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{
    ClientDetail, ClientListItem, KickKind, KickRequest, LiveClient, MoveRequest, MuteRequest,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::control::{ControlBackend, ControlBackendError};
use crate::repos::server_connections::ServerConnection;
use crate::ws::topic::{Topic, TopicKind};

use super::{access, audit, bad_request, translate_control_error};

/// Spec §7.8 — read flag set the FE always wants for the active list.
/// `-ip` is admin-only and is appended in [`list`] when the caller is admin.
const BASE_CLIENT_FLAGS: &[&str] = &["uid", "away", "voice", "times", "groups", "info", "country"];

pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<ClientListItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;

    // Admin-only `-ip` flag. Spec §7.8 marks `connection_client_ip` as
    // admin-only; non-admin callers also get the field stripped to empty
    // when projecting. Belt + braces.
    let mut flags: Vec<&str> = BASE_CLIENT_FLAGS.to_vec();
    if user.is_admin() {
        flags.push("ip");
    }

    let rows = client
        .clientlist_with_flags(sid, &flags)
        .await
        .map_err(translate_control_error)?;
    let projected: Vec<ClientListItem> = rows
        .into_iter()
        .map(|r| project_client_list_item(r, user.is_admin()))
        .collect();
    Ok(Json(projected))
}

pub async fn detail(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cldbid)): Path<(i64, i64, i64)>,
) -> Result<Json<ClientDetail>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;

    let db_row = client
        .clientdbinfo(sid, cldbid)
        .await
        .map_err(translate_control_error)?;

    // Locate the live `clid`, if any, by scanning the active client list
    // for a match on `client_database_id`. Cheap on small servers; for
    // large servers this is an O(N) scan we should swap for a directly
    // targeted `clientgetids` upstream once that command lands in the
    // typed surface.
    let live_clid = client
        .clientlist(sid)
        .await
        .map_err(translate_control_error)?
        .into_iter()
        .find(|c| c.client_database_id == cldbid)
        .map(|c| c.clid);

    let live_client = if let Some(clid) = live_clid {
        Some(
            client
                .clientinfo(sid, clid)
                .await
                .map(|info| project_live_client(clid, info, user.is_admin()))
                .map_err(translate_control_error)?,
        )
    } else {
        None
    };

    Ok(Json(ClientDetail {
        cldbid: db_row.cldbid,
        client_unique_identifier: db_row.client_unique_identifier,
        client_nickname: db_row.client_nickname,
        client_created: db_row.client_created,
        client_lastconnected: db_row.client_lastconnected,
        client_totalconnections: db_row.client_totalconnections,
        client_description: db_row.client_description,
        client_lastip: if user.is_admin() {
            db_row.client_lastip
        } else {
            String::new()
        },
        live_client,
    }))
}

pub async fn kick(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, clid)): Path<(i64, i64, i64)>,
    Json(req): Json<KickRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;
    let reason_id = req.kind.reason_id();
    let started = Instant::now();
    let action = "client.kick";
    let details = format!(
        "kind={kind:?} reasonid={reason_id} reason={reason:?}",
        kind = req.kind,
        reason = req.reason
    );
    match client
        .clientkick(sid, clid, reason_id, req.reason.as_deref())
        .await
    {
        Ok(()) => {
            emit_success(
                &state,
                &user,
                &connection,
                sid,
                action,
                Some(clid),
                &details,
                started,
            )
            .await;
            publish_client_event(
                &state,
                config_id,
                kick_event_name(req.kind),
                json!({
                    "clid": clid,
                    "reasonid": reason_id,
                    "reason": req.reason,
                }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure_and_translate(
            &state,
            &user,
            &connection,
            sid,
            action,
            Some(clid),
            &details,
            e,
            started,
        )
        .await),
    }
}

pub async fn mute(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, clid)): Path<(i64, i64, i64)>,
    body: Option<Json<MuteRequest>>,
) -> Result<StatusCode, Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;
    // Default behaviour with no body: mute both directions.
    let req = body.map(|Json(b)| b).unwrap_or(MuteRequest {
        input_muted: Some(true),
        output_muted: Some(true),
    });
    if req.input_muted.is_none() && req.output_muted.is_none() {
        return Err(bad_request(
            "mute body must set at least one of inputMuted/outputMuted",
        ));
    }
    let started = Instant::now();
    let action = "client.mute";
    let details = format!(
        "input_muted={:?} output_muted={:?}",
        req.input_muted, req.output_muted
    );
    match client
        .client_set_muted(sid, clid, req.input_muted, req.output_muted)
        .await
    {
        Ok(()) => {
            emit_success(
                &state,
                &user,
                &connection,
                sid,
                action,
                Some(clid),
                &details,
                started,
            )
            .await;
            publish_client_event(
                &state,
                config_id,
                "ts:client:muted",
                json!({
                    "clid": clid,
                    "inputMuted": req.input_muted,
                    "outputMuted": req.output_muted,
                }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure_and_translate(
            &state,
            &user,
            &connection,
            sid,
            action,
            Some(clid),
            &details,
            e,
            started,
        )
        .await),
    }
}

pub async fn unmute(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, clid)): Path<(i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;
    let started = Instant::now();
    let action = "client.unmute";
    let details = "input_muted=false output_muted=false".to_string();
    match client
        .client_set_muted(sid, clid, Some(false), Some(false))
        .await
    {
        Ok(()) => {
            emit_success(
                &state,
                &user,
                &connection,
                sid,
                action,
                Some(clid),
                &details,
                started,
            )
            .await;
            publish_client_event(
                &state,
                config_id,
                "ts:client:unmuted",
                json!({ "clid": clid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure_and_translate(
            &state,
            &user,
            &connection,
            sid,
            action,
            Some(clid),
            &details,
            e,
            started,
        )
        .await),
    }
}

pub async fn move_to(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, clid)): Path<(i64, i64, i64)>,
    Json(req): Json<MoveRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = backend(&state, &connection).await?;
    let started = Instant::now();
    let action = "client.move";
    let details = format!("cid={cid}", cid = req.cid);
    match client
        .clientmove(sid, clid, req.cid, req.channel_password.as_deref())
        .await
    {
        Ok(()) => {
            emit_success(
                &state,
                &user,
                &connection,
                sid,
                action,
                Some(clid),
                &details,
                started,
            )
            .await;
            publish_client_event(
                &state,
                config_id,
                "ts:client:moved",
                json!({
                    "clid": clid,
                    "cid": req.cid,
                }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure_and_translate(
            &state,
            &user,
            &connection,
            sid,
            action,
            Some(clid),
            &details,
            e,
            started,
        )
        .await),
    }
}

// =====================================================================
// shared helpers
// =====================================================================

fn kick_event_name(kind: KickKind) -> &'static str {
    match kind {
        KickKind::Channel => "ts:client:kicked_from_channel",
        KickKind::Server => "ts:client:kicked_from_server",
    }
}

async fn backend(
    state: &AppState,
    connection: &ServerConnection,
) -> Result<Arc<dyn ControlBackend>, Response> {
    state
        .control
        .get_or_build(connection.id, Some(connection))
        .await
        .map_err(translate_control_error)
}

async fn emit_success(
    _state: &AppState,
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

async fn emit_failure_and_translate(
    _state: &AppState,
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

async fn publish_client_event(
    state: &AppState,
    config_id: i64,
    kind: &'static str,
    data: serde_json::Value,
) {
    let topic = Topic::new(config_id, TopicKind::Clients);
    let _ = state.ws_hub.publish(topic, kind, data).await;
}

fn project_client_list_item(
    e: crate::webquery::models::ClientEntry,
    is_admin: bool,
) -> ClientListItem {
    ClientListItem {
        clid: e.clid,
        cid: e.cid,
        client_database_id: e.client_database_id,
        client_type: e.client_type,
        client_nickname: e.client_nickname,
        client_unique_identifier: e.client_unique_identifier,
        client_away: e.client_away,
        client_away_message: e.client_away_message,
        client_flag_talking: e.client_flag_talking,
        client_input_muted: e.client_input_muted,
        client_output_muted: e.client_output_muted,
        client_input_hardware: e.client_input_hardware,
        client_output_hardware: e.client_output_hardware,
        client_idle_time: e.client_idle_time,
        client_lastconnected: e.client_lastconnected,
        client_created: e.client_created,
        client_servergroups: e.client_servergroups,
        client_channel_group_id: e.client_channel_group_id,
        client_version: e.client_version,
        client_platform: e.client_platform,
        client_country: e.client_country,
        connection_client_ip: if is_admin {
            e.connection_client_ip
        } else {
            String::new()
        },
    }
}

fn project_live_client(
    clid: i64,
    e: crate::webquery::models::ClientInfo,
    is_admin: bool,
) -> LiveClient {
    LiveClient {
        clid,
        cid: e.cid,
        client_type: e.client_type,
        client_nickname: e.client_nickname,
        client_platform: e.client_platform,
        client_version: e.client_version,
        client_idle_time: e.client_idle_time,
        client_away: e.client_away,
        client_away_message: e.client_away_message,
        client_input_muted: e.client_input_muted,
        client_output_muted: e.client_output_muted,
        client_country: e.client_country,
        client_servergroups: e.client_servergroups,
        client_channel_group_id: e.client_channel_group_id,
        client_totalconnections: e.client_totalconnections,
        connection_client_ip: if is_admin {
            e.connection_client_ip
        } else {
            String::new()
        },
    }
}
