//! Channel-group endpoints — PURA-373 (spec §7.10).
//!
//! Mounted at `/api/servers/{configId}/vs/{sid}/channel-groups`. Reads use
//! [`access::check_read`]; every write is admin-only via
//! [`access::check_admin`] (spec §7.10 "Y+admin" rows). Mutations publish
//! on the per-server `moderation` WS topic.
//!
//! Mirrors [`super::server_groups`]. The one structural difference: TS6
//! channel-group permissions carry only a value — `channelgroupaddperm`
//! has no `permnegated` / `permskip`, so [`set_permission`] silently
//! drops those fields from the request body.
//!
//! Pure TS6 WebQuery passthrough — no SurrealDB entity, no SSH.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{
    ChannelGroupAssignRequest, ChannelGroupClientItem, ChannelGroupCreated, ChannelGroupItem,
    GroupCreateRequest, GroupPermDeleteQuery, GroupPermItem, GroupPermSetRequest,
    GroupRenameRequest,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

use super::{
    access, audit_ok, bad_request, emit_webquery_failure, publish_moderation,
    translate_webquery_error, webquery_client,
};

/// `GET ` — `channelgrouplist`.
pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<ChannelGroupItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .channelgrouplist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|g| ChannelGroupItem {
            cgid: g.cgid,
            name: g.name,
            group_type: g.r#type,
            iconid: g.iconid,
            savedb: g.savedb,
            sortid: g.sortid,
            namemode: g.namemode,
            n_modifyp: g.n_modifyp,
            n_member_addp: g.n_member_addp,
            n_member_removep: g.n_member_removep,
        })
        .collect();
    Ok(Json(out))
}

/// `POST ` — `channelgroupadd`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<GroupCreateRequest>,
) -> Result<(StatusCode, Json<ChannelGroupCreated>), Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.name.trim().is_empty() {
        return Err(bad_request("channel group name must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.create";
    let details = format!("name={:?} type={:?}", req.name, req.r#type);
    match client.channelgroupadd(sid, &req.name, req.r#type).await {
        Ok(cgid) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:created",
                json!({ "cgid": cgid, "name": req.name }),
            )
            .await;
            Ok((StatusCode::CREATED, Json(ChannelGroupCreated { cgid })))
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

/// `PUT :cgid` — `channelgrouprename`.
pub async fn rename(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
    Json(req): Json<GroupRenameRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.name.trim().is_empty() {
        return Err(bad_request("channel group name must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.rename";
    let details = format!("cgid={cgid} name={:?}", req.name);
    match client.channelgrouprename(sid, cgid, &req.name).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:updated",
                json!({ "cgid": cgid, "name": req.name }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cgid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :cgid` — `channelgroupdel` (`force=1`).
pub async fn delete(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.delete";
    let details = format!("cgid={cgid}");
    match client.channelgroupdel(sid, cgid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:deleted",
                json!({ "cgid": cgid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cgid),
            &details,
            e,
            started,
        )),
    }
}

/// `GET :cgid/clients` — `channelgroupclientlist`.
pub async fn clients(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
) -> Result<Json<Vec<ChannelGroupClientItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .channelgroupclientlist(sid, cgid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|c| ChannelGroupClientItem {
            cid: c.cid,
            cldbid: c.cldbid,
            cgid: c.cgid,
        })
        .collect();
    Ok(Json(out))
}

/// `POST :cgid/assign` — `setclientchannelgroup`.
pub async fn assign(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
    Json(req): Json<ChannelGroupAssignRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.assign";
    let details = format!("cgid={cgid} cid={} cldbid={}", req.cid, req.cldbid);
    match client
        .setclientchannelgroup(sid, cgid, req.cid, req.cldbid)
        .await
    {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:assigned",
                json!({ "cgid": cgid, "cid": req.cid, "cldbid": req.cldbid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cgid),
            &details,
            e,
            started,
        )),
    }
}

/// `GET :cgid/permissions` — `channelgrouppermlist -permsid`.
pub async fn permissions(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
) -> Result<Json<Vec<GroupPermItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .channelgrouppermlist(sid, cgid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|p| GroupPermItem {
            permid: p.permid,
            permsid: p.permsid,
            permvalue: p.permvalue,
            permnegated: p.permnegated,
            permskip: p.permskip,
        })
        .collect();
    Ok(Json(out))
}

/// `PUT :cgid/permissions` — `channelgroupaddperm`. Only `permvalue` is
/// forwarded; channel-group permissions have no negate / skip flags.
pub async fn set_permission(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
    Json(req): Json<GroupPermSetRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.permsid.trim().is_empty() {
        return Err(bad_request("permsid must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.permission.set";
    let details = format!(
        "cgid={cgid} permsid={:?} value={}",
        req.permsid, req.permvalue
    );
    match client
        .channelgroupaddperm(sid, cgid, &req.permsid, req.permvalue)
        .await
    {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:permissions_changed",
                json!({ "cgid": cgid, "permsid": req.permsid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cgid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :cgid/permissions?permsid=` — `channelgroupdelperm`.
pub async fn delete_permission(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cgid)): Path<(i64, i64, i64)>,
    Query(q): Query<GroupPermDeleteQuery>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if q.permsid.trim().is_empty() {
        return Err(bad_request("permsid query parameter is required"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "channel_group.permission.delete";
    let details = format!("cgid={cgid} permsid={:?}", q.permsid);
    match client.channelgroupdelperm(sid, cgid, &q.permsid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:channel_group:permissions_changed",
                json!({ "cgid": cgid, "permsid": q.permsid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cgid),
            &details,
            e,
            started,
        )),
    }
}
