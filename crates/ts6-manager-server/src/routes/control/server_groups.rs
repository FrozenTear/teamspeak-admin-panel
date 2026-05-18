//! Server-group endpoints — PURA-373 (spec §7.9).
//!
//! Mounted at `/api/servers/{configId}/vs/{sid}/server-groups`. Reads use
//! [`access::check_read`] (any operator with server access); every write
//! is admin-only via [`access::check_admin`] — spec §7.9 marks each
//! mutating row "Y+admin" (a grant-holding moderator is NOT sufficient,
//! unlike the §7.8 client actions). Mutations publish on the per-server
//! `moderation` WS topic.
//!
//! Pure TS6 WebQuery passthrough — no SurrealDB entity, no SSH.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::control::{
    GroupCreateRequest, GroupMemberAddRequest, GroupPermDeleteQuery, GroupPermItem,
    GroupPermSetRequest, GroupRenameRequest, ServerGroupCopyRequest, ServerGroupCreated,
    ServerGroupItem, ServerGroupMember,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::webquery::GroupPermWrite;

use super::{
    access, audit_ok, bad_request, emit_webquery_failure, publish_moderation,
    translate_webquery_error, webquery_client,
};

/// `GET ` — `servergrouplist`.
pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<ServerGroupItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .servergrouplist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|g| ServerGroupItem {
            sgid: g.sgid,
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

/// `POST ` — `servergroupadd`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Json(req): Json<GroupCreateRequest>,
) -> Result<(StatusCode, Json<ServerGroupCreated>), Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.name.trim().is_empty() {
        return Err(bad_request("server group name must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.create";
    let details = format!("name={:?} type={:?}", req.name, req.r#type);
    match client.servergroupadd(sid, &req.name, req.r#type).await {
        Ok(sgid) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:created",
                json!({ "sgid": sgid, "name": req.name }),
            )
            .await;
            Ok((StatusCode::CREATED, Json(ServerGroupCreated { sgid })))
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

/// `PUT :sgid` — `servergrouprename`.
pub async fn rename(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
    Json(req): Json<GroupRenameRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.name.trim().is_empty() {
        return Err(bad_request("server group name must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.rename";
    let details = format!("sgid={sgid} name={:?}", req.name);
    match client.servergrouprename(sid, sgid, &req.name).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:updated",
                json!({ "sgid": sgid, "name": req.name }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(sgid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :sgid` — `servergroupdel` (`force=1`).
pub async fn delete(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.delete";
    let details = format!("sgid={sgid}");
    match client.servergroupdel(sid, sgid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:deleted",
                json!({ "sgid": sgid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(sgid),
            &details,
            e,
            started,
        )),
    }
}

/// `POST :sgid/copy` — `servergroupcopy` into a new group (`tsgid=0`).
pub async fn copy(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
    Json(req): Json<ServerGroupCopyRequest>,
) -> Result<(StatusCode, Json<ServerGroupCreated>), Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.name.trim().is_empty() {
        return Err(bad_request("server group name must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.copy";
    // Default the copy to a regular group (`type=1`) when unspecified.
    let group_type = req.r#type.unwrap_or(1);
    let details = format!("ssgid={sgid} name={:?} type={group_type}", req.name);
    match client
        .servergroupcopy(sid, sgid, &req.name, group_type)
        .await
    {
        Ok(new_sgid) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(new_sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:created",
                json!({ "sgid": new_sgid, "name": req.name, "copiedFrom": sgid }),
            )
            .await;
            Ok((
                StatusCode::CREATED,
                Json(ServerGroupCreated { sgid: new_sgid }),
            ))
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(sgid),
            &details,
            e,
            started,
        )),
    }
}

/// `GET :sgid/members` — `servergroupclientlist -names`.
pub async fn members(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
) -> Result<Json<Vec<ServerGroupMember>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .servergroupclientlist(sid, sgid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|m| ServerGroupMember {
            cldbid: m.cldbid,
            client_nickname: m.client_nickname,
            client_unique_identifier: m.client_unique_identifier,
        })
        .collect();
    Ok(Json(out))
}

/// `POST :sgid/members` — `servergroupaddclient`.
pub async fn add_member(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
    Json(req): Json<GroupMemberAddRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.member.add";
    let details = format!("sgid={sgid} cldbid={}", req.cldbid);
    match client.servergroupaddclient(sid, sgid, req.cldbid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(req.cldbid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:member_added",
                json!({ "sgid": sgid, "cldbid": req.cldbid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(req.cldbid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :sgid/members/:cldbid` — `servergroupdelclient`.
pub async fn remove_member(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid, cldbid)): Path<(i64, i64, i64, i64)>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.member.remove";
    let details = format!("sgid={sgid} cldbid={cldbid}");
    match client.servergroupdelclient(sid, sgid, cldbid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(cldbid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:member_removed",
                json!({ "sgid": sgid, "cldbid": cldbid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(cldbid),
            &details,
            e,
            started,
        )),
    }
}

/// `GET :sgid/permissions` — `servergrouppermlist -permsid`.
pub async fn permissions(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
) -> Result<Json<Vec<GroupPermItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .servergrouppermlist(sid, sgid)
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

/// `PUT :sgid/permissions` — `servergroupaddperm` (upsert one permission).
pub async fn set_permission(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
    Json(req): Json<GroupPermSetRequest>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if req.permsid.trim().is_empty() {
        return Err(bad_request("permsid must not be empty"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.permission.set";
    let details = format!(
        "sgid={sgid} permsid={:?} value={} negated={} skip={}",
        req.permsid, req.permvalue, req.permnegated, req.permskip
    );
    let perm = GroupPermWrite {
        permsid: &req.permsid,
        permvalue: req.permvalue,
        permnegated: req.permnegated,
        permskip: req.permskip,
    };
    match client.servergroupaddperm(sid, sgid, &perm).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:permissions_changed",
                json!({ "sgid": sgid, "permsid": req.permsid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(sgid),
            &details,
            e,
            started,
        )),
    }
}

/// `DELETE :sgid/permissions?permsid=` — `servergroupdelperm`.
pub async fn delete_permission(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, sgid)): Path<(i64, i64, i64)>,
    Query(q): Query<GroupPermDeleteQuery>,
) -> Result<StatusCode, Response> {
    let connection = access::check_admin(&state, &user, config_id).await?;
    if q.permsid.trim().is_empty() {
        return Err(bad_request("permsid query parameter is required"));
    }
    let client = webquery_client(&state, &connection).await?;
    let started = Instant::now();
    let action = "server_group.permission.delete";
    let details = format!("sgid={sgid} permsid={:?}", q.permsid);
    match client.servergroupdelperm(sid, sgid, &q.permsid).await {
        Ok(()) => {
            audit_ok(
                connection.id,
                sid,
                &user,
                action,
                Some(sgid),
                &details,
                started,
            );
            publish_moderation(
                &state,
                config_id,
                "ts:server_group:permissions_changed",
                json!({ "sgid": sgid, "permsid": q.permsid }),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(emit_webquery_failure(
            &user,
            connection.id,
            sid,
            action,
            Some(sgid),
            &details,
            e,
            started,
        )),
    }
}
