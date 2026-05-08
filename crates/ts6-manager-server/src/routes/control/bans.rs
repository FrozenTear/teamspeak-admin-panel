//! Ban-list endpoints — PURA-71.
//!
//! - `GET    /api/servers/{configId}/vs/{sid}/bans`           — banlist passthrough.
//! - `POST   /api/servers/{configId}/vs/{sid}/bans`           — banadd.
//! - `DELETE /api/servers/{configId}/vs/{sid}/bans/{banid}`   — bandel.
//!
//! Bans publish on the `clients` topic — TS lumps ban events with the
//! client connection lifecycle (§8.4 `ts:client:banned`-style events) and
//! the FE consumers that care about bans already subscribe to clients.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{BanCreateRequest, BanCreated, BanListItem};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::repos::server_connections::ServerConnection;
use crate::webquery::{BanAddParams, WebQueryError};
use crate::ws::topic::{Topic, TopicKind};

use super::{access, audit, bad_request, translate_webquery_error};

pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<BanListItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;
    let rows = client
        .banlist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let projected: Vec<BanListItem> = rows
        .into_iter()
        .map(|b| BanListItem {
            banid: b.banid,
            ip: b.ip,
            uid: b.uid,
            mytsid: b.mytsid,
            name: b.name,
            created: b.created,
            duration: b.duration,
            reason: b.reason,
            invokername: b.invokername,
            invokercldbid: b.invokercldbid,
            invokeruid: b.invokeruid,
            enforcements: b.enforcements,
            lastnickname: b.lastnickname,
        })
        .collect();
    Ok(Json(projected))
}

pub async fn create(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<BanCreateRequest>,
) -> Result<(StatusCode, Json<BanCreated>), Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;

    // At least one matcher must be set, otherwise the upstream returns
    // a generic parameter-missing error and we'd record a useless audit
    // entry. Catch up front.
    if req.ip.is_none() && req.uid.is_none() && req.my_ts_id.is_none() && req.name.is_none() {
        return Err(bad_request(
            "ban body must set at least one of ip / uid / myTsId / name",
        ));
    }

    let params = BanAddParams {
        ip: req.ip.as_deref(),
        uid: req.uid.as_deref(),
        mytsid: req.my_ts_id.as_deref(),
        name: req.name.as_deref(),
        banreason: req.reason.as_deref(),
        time: req.duration,
    };
    let started = Instant::now();
    let action = "ban.add";
    let details = ban_create_details(&req);
    match client.banadd(sid, &params).await {
        Ok(banid) => {
            audit::AuditEntry::success(
                connection.id,
                sid,
                user.id,
                &user.username,
                action,
                Some(banid),
                &details,
                started.elapsed(),
            )
            .emit();
            publish(
                &state,
                config_id,
                "ts:ban:added",
                json!({
                    "banid": banid,
                    "ip": req.ip,
                    "uid": req.uid,
                    "name": req.name,
                    "reason": req.reason,
                    "duration": req.duration,
                }),
            )
            .await;
            Ok((StatusCode::CREATED, Json(BanCreated { banid })))
        }
        Err(e) => Err(emit_failure(
            &user, &connection, sid, action, None, &details, e, started,
        )),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, banid)): Path<(i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_write(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;
    let started = Instant::now();
    let action = "ban.delete";
    let details = format!("banid={banid}");
    match client.bandel(sid, banid).await {
        Ok(()) => {
            audit::AuditEntry::success(
                connection.id,
                sid,
                user.id,
                &user.username,
                action,
                Some(banid),
                &details,
                started.elapsed(),
            )
            .emit();
            publish(&state, config_id, "ts:ban:deleted", json!({ "banid": banid })).await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_failure(
            &user,
            &connection,
            sid,
            action,
            Some(banid),
            &details,
            e,
            started,
        )),
    }
}

fn ban_create_details(req: &BanCreateRequest) -> String {
    let mut parts = Vec::with_capacity(6);
    if let Some(v) = &req.ip {
        parts.push(format!("ip={v}"));
    }
    if let Some(v) = &req.uid {
        parts.push(format!("uid={v}"));
    }
    if let Some(v) = &req.my_ts_id {
        parts.push(format!("mytsid={v}"));
    }
    if let Some(v) = &req.name {
        parts.push(format!("name={v}"));
    }
    if let Some(v) = req.duration {
        parts.push(format!("time={v}"));
    }
    if let Some(v) = &req.reason {
        // Truncate reason to keep the audit record terse.
        let trimmed: String = v.chars().take(120).collect();
        parts.push(format!("reason={trimmed:?}"));
    }
    parts.join(" ")
}

fn emit_failure(
    user: &crate::auth::extractors::AuthUser,
    connection: &ServerConnection,
    sid: i64,
    action: &'static str,
    target_id: Option<i64>,
    details: &str,
    err: WebQueryError,
    started: Instant,
) -> Response {
    let elapsed = started.elapsed();
    let entry = match &err {
        WebQueryError::Upstream { code, message } => audit::AuditEntry::upstream_error(
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
    translate_webquery_error(err)
}

async fn publish(
    state: &AppState,
    config_id: i64,
    kind: &'static str,
    data: serde_json::Value,
) {
    let topic = Topic::new(config_id, TopicKind::Clients);
    let _ = state.ws_hub.publish(topic, kind, data).await;
}
