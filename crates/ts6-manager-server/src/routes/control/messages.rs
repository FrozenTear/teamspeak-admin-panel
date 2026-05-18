//! Offline-message endpoints — PURA-373 (spec §7.16).
//!
//! Mounted at `/api/servers/{configId}/vs/{sid}/messages`. Listing and
//! reading messages are open to any operator with server access
//! ([`access::check_read`]); composing and deleting are admin-only
//! ([`access::check_admin`], spec §7.16 "Y+admin"). Mutations publish on
//! the per-server `moderation` WS topic.
//!
//! TS6 offline messages are the server's client-to-client inbox. An empty
//! inbox surfaces from the upstream as error code `1281`
//! (`database_empty_result`); the WebQuery layer normalises that to `[]`
//! (`WebQueryClient::messagelist` → `list_lenient`), so this route never
//! sees the 1281 envelope.
//!
//! Pure TS6 WebQuery passthrough — no SurrealDB entity, no SSH.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{MessageCreateRequest, MessageDetailResponse, MessageListItem};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

use super::{
    access, audit_ok, bad_request, emit_webquery_failure, publish_moderation,
    translate_webquery_error, webquery_client,
};

/// `GET ` — `messagelist`. Empty inbox → `[]` (1281 normalised upstream).
pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<MessageListItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .messagelist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|m| MessageListItem {
            msgid: m.msgid,
            cluid: m.cluid,
            subject: m.subject,
            timestamp: m.timestamp,
            flag_read: m.flag_read,
        })
        .collect();
    Ok(Json(out))
}

/// `GET :msgid` — `messageget` (includes the message body).
pub async fn detail(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, msgid)): Path<(i64, i64, i64)>,
) -> Result<Json<MessageDetailResponse>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let m = client
        .messageget(sid, msgid)
        .await
        .map_err(translate_webquery_error)?;
    Ok(Json(MessageDetailResponse {
        msgid: m.msgid,
        cluid: m.cluid,
        subject: m.subject,
        message: m.message,
        timestamp: m.timestamp,
    }))
}

/// `POST ` — `messageadd`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<MessageCreateRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.cluid.trim().is_empty() {
        return Err(bad_request("message cluid (recipient) must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "message.create";
    // Subject only — the body may contain personal content; keep it out
    // of the audit detail.
    let details = format!("cluid={:?} subject={:?}", req.cluid, req.subject);
    match client
        .messageadd(sid, &req.cluid, &req.subject, &req.message)
        .await
    {
        Ok(()) => {
            audit_ok(connection.id, sid, &user, action, None, &details, started);
            publish_moderation(
                &state,
                config_id,
                "ts:message:created",
                json!({ "cluid": req.cluid }),
            )
            .await;
            Ok(StatusCode::CREATED)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            None,
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :msgid` — `messagedel`.
pub async fn delete(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, msgid)): Path<(i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "message.delete";
    let details = format!("msgid={msgid}");
    match client.messagedel(sid, msgid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(msgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:message:deleted",
                json!({ "msgid": msgid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(msgid),
            &details,
            e,
            started,
        )),
    }
}
