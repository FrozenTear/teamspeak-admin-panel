//! Token (privilege key) endpoints — PURA-373 (spec §7.13).
//!
//! Mounted at `/api/servers/{configId}/vs/{sid}/tokens`. The list is
//! readable by any operator with server access ([`access::check_read`]);
//! minting and deleting keys are admin-only ([`access::check_admin`],
//! spec §7.13 "Y+admin"). Mutations publish on the per-server
//! `moderation` WS topic.
//!
//! A TS6 token is a privilege key — a one-time string that drops a
//! redeeming client into a server group (`tokenType=0`) or channel group
//! (`tokenType=1`). The key string is also the `DELETE` path segment; the
//! FE percent-encodes it (keys may contain `/`).
//!
//! Pure TS6 WebQuery passthrough — no SurrealDB entity, no SSH.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{TokenCreateRequest, TokenCreated, TokenItem};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::webquery::PrivilegeKeyAddParams;

use super::{
    access, audit_ok, emit_webquery_failure, publish_moderation, translate_webquery_error,
    webquery_client,
};

/// `GET ` — `privilegekeylist`.
pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<TokenItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .privilegekeylist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|t| TokenItem {
            token: t.token,
            token_type: t.token_type,
            token_id1: t.token_id1,
            token_id2: t.token_id2,
            token_description: t.token_description,
            token_created: t.token_created,
            token_customset: t.token_customset,
        })
        .collect();
    Ok(Json(out))
}

/// `POST ` — `privilegekeyadd`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<TokenCreateRequest>,
) -> Result<(StatusCode, Json<TokenCreated>), Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "token.create";
    let details = format!(
        "type={} id1={} id2={}",
        req.token_type, req.token_id1, req.token_id2
    );
    let params = PrivilegeKeyAddParams {
        token_type: req.token_type,
        token_id1: req.token_id1,
        token_id2: req.token_id2,
        description: req.description.as_deref(),
        customset: req.customset.as_deref(),
    };
    match client.privilegekeyadd(sid, &params).await {
        Ok(token) => {
            // The key string is a credential — never log it; the audit
            // detail records only the target group / channel ids.
            audit_ok(connection.id, sid, &user, action, None, &details, started);
            publish_moderation(
                &state,
                config_id,
                "ts:token:created",
                json!({ "tokenType": req.token_type, "tokenId1": req.token_id1 }),
            )
            .await;
            Ok((StatusCode::CREATED, Json(TokenCreated { token })))
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

/// `DELETE :token` — `privilegekeydelete`.
pub async fn delete(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, token)): Path<(i64, i64, String)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "token.delete";
    // The token string is a credential — keep it out of the audit detail.
    let details = "token=<redacted>".to_string();
    match client.privilegekeydelete(sid, &token).await {
        Ok(()) => {
            audit_ok(connection.id, sid, &user, action, None, &details, started);
            publish_moderation(&state, config_id, "ts:token:deleted", json!({})).await;
            Ok(StatusCode::NO_CONTENT)
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
