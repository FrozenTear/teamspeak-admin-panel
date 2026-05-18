//! Permission-catalog endpoints — PURA-373 (spec §7.11, read-only).
//!
//! Mounted at `/api/servers/{configId}/vs/{sid}/permissions`. Read-only —
//! every route uses [`access::check_read`] (any operator with server
//! access); there are no writes here. Permissions are *edited* through a
//! server group / channel group / client, never against the catalog
//! (PURA-370 §1.2).
//!
//! Pure TS6 WebQuery passthrough — no SurrealDB entity, no SSH.
//!
//! ## Fixture findings (PURA-373 open questions, PURA-370 §1.2 / §6.2)
//!
//! - `permissionlist` exposes **no machine-readable category field** — the
//!   row is `{permid, permname, permdesc}`. The UI rail derives categories
//!   from the second `permname` token (`virtualserver` / `channel` /
//!   `client` / `ft` / `serverinstance` / `group` / `serverquery`).
//! - `permoverview` **does** tag each row with its origin: `t` is the
//!   assignment kind and `id1` carries the originating `sgid` / `cid`, so
//!   the §6.2 "why does this client have this permission" SOURCE column
//!   resolves without a second lookup.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use ts6_manager_shared::control::{
    PermFindItem, PermFindQuery, PermOverviewItem, PermOverviewQuery, PermissionCatalogItem,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::webquery::PermSelector;

use super::{access, bad_request, translate_webquery_error, webquery_client};

/// `GET ` — `permissionlist`. The full read-only catalog.
pub async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
) -> Result<Json<Vec<PermissionCatalogItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .permissionlist(sid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|p| PermissionCatalogItem {
            permid: p.permid,
            permname: p.permname,
            permdesc: p.permdesc,
        })
        .collect();
    Ok(Json(out))
}

/// `GET find?permid=|permsid=` — `permfind`. Exactly one selector must be
/// supplied; `permid` wins if both are present.
pub async fn find(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Query(q): Query<PermFindQuery>,
) -> Result<Json<Vec<PermFindItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let selector = match (q.permid, q.permsid.as_deref()) {
        (Some(id), _) => PermSelector::Id(id),
        (None, Some(sid_str)) if !sid_str.trim().is_empty() => PermSelector::Sid(sid_str),
        _ => {
            return Err(bad_request(
                "permfind requires a permid or permsid query parameter",
            ));
        }
    };
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .permfind(sid, selector)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|r| PermFindItem {
            t: r.t,
            id1: r.id1,
            id2: r.id2,
            p: r.p,
        })
        .collect();
    Ok(Json(out))
}

/// `GET overview/:cldbid?cid=&permid=` — `permoverview`. `cid` / `permid`
/// default to `0` (whole-catalog overview for the client).
pub async fn overview(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid, cldbid)): Path<(i64, i64, i64)>,
    Query(q): Query<PermOverviewQuery>,
) -> Result<Json<Vec<PermOverviewItem>>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = webquery_client(&state, &connection).await?;
    let rows = client
        .permoverview(sid, cldbid, q.cid, q.permid)
        .await
        .map_err(translate_webquery_error)?;
    let out = rows
        .into_iter()
        .map(|r| PermOverviewItem {
            t: r.t,
            id1: r.id1,
            id2: r.id2,
            p: r.p,
            v: r.v,
            n: r.n,
            s: r.s,
        })
        .collect();
    Ok(Json(out))
}
