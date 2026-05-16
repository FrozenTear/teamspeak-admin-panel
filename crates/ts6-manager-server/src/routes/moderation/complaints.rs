//! TS6 complaint sub-surface — `/api/moderation/complaints/*` (PURA-289,
//! split out of PURA-286 `9.0-routes`).
//!
//! A TS6 complaint is a `(tcldbid, fcldbid)` pair with no single id of
//! its own — the plan §7 path shape `POST /complaints/{id}/resolve`
//! cannot map faithfully, so the resolve endpoint takes a JSON body
//! carrying the pair instead (PR-flagged deviation). With `fcldbid`
//! present it dismisses one complaint (`complaindel`); without, it
//! dismisses every complaint about the target (`complaindelall`).
//!
//! Per the `9.0-spike` findings: `complainadd` is **not** exposed — it is
//! structurally unavailable via WebQuery (board-acked §7.15 deviation,
//! PURA-283). `complaindel` returns `512` for both an invalid id and a
//! non-existent complaint; the two are indistinguishable, so `512` maps
//! to `404` either way.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequirePermission};
use crate::auth::permissions::{ComplaintResolve, ComplaintView};
use crate::control::{ControlBackend, ControlBackendError};
use crate::repos::server_connections;
use crate::webquery::models::ComplaintEntry;

use super::{internal, not_found, translate_control_error, validation};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComplaintListQuery {
    server_config_id: i64,
    virtual_server_id: i64,
    /// Optional filter to one target subject (spec §7.15 `?tcldbid`).
    #[serde(default)]
    tcldbid: Option<i64>,
}

/// `GET /api/moderation/complaints` — the TS6 complaint queue for one
/// virtual server. `RequirePermission<ComplaintView>`-gated.
pub(super) async fn list(
    State(state): State<AppState>,
    _gate: RequirePermission<ComplaintView>,
    Query(q): Query<ComplaintListQuery>,
) -> Result<Json<Vec<wire::Complaint>>, Response> {
    let backend = backend_for(&state, q.server_config_id).await?;
    let rows = backend
        .complainlist(q.virtual_server_id, q.tcldbid)
        .await
        .map_err(translate_control_error)?;
    Ok(Json(rows.into_iter().map(complaint_to_wire).collect()))
}

/// `POST /api/moderation/complaints/resolve` — dismiss a complaint.
/// `RequirePermission<ComplaintResolve>`-gated. With `fcldbid` →
/// `complaindel` (one complaint); without → `complaindelall` (every
/// complaint about the `tcldbid` subject).
pub(super) async fn resolve(
    State(state): State<AppState>,
    gate: RequirePermission<ComplaintResolve>,
    meta: RequestMeta,
    Json(req): Json<wire::ResolveComplaintRequest>,
) -> Result<StatusCode, Response> {
    let actor = gate.0;
    let backend = backend_for(&state, req.server_config_id).await?;
    let sid = req.virtual_server_id;

    // `fcldbid` present → one complaint; absent → all complaints about
    // the target. `complaindelall` is per-target and idempotent.
    let scope = match req.fcldbid {
        Some(fcldbid) => {
            backend
                .complaindel(sid, req.tcldbid, fcldbid)
                .await
                .map_err(translate_complaint_error)?;
            "one"
        }
        None => {
            backend
                .complaindelall(sid, req.tcldbid)
                .await
                .map_err(translate_complaint_error)?;
            "all"
        }
    };

    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationComplaintResolved,
            target: Some(Target::moderation_complaint(req.tcldbid)),
            payload: Some(serde_json::json!({
                "serverConfigId": req.server_config_id,
                "virtualServerId": sid,
                "tcldbid": req.tcldbid,
                "fcldbid": req.fcldbid,
                "scope": scope,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Resolve the `ControlBackend` for `server_config_id`, mapping an
/// absent connection row to `404` and a build failure through the
/// shared §7.0.2 `502` path.
async fn backend_for(
    state: &AppState,
    server_config_id: i64,
) -> Result<Arc<dyn ControlBackend>, Response> {
    let connection = server_connections::find_by_id(&state.db, server_config_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation complaint server lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("server connection not found"))?;
    state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)
}

/// Translate a `complaindel` / `complaindelall` backend error, applying
/// the complaint-specific TS6 code table from the `9.0-spike` findings §7:
///
/// | TS code | meaning | HTTP |
/// |---|---|---|
/// | `512`  | invalid clientID / no such complaint | `404` |
/// | `1538` | invalid parameter | `400` |
/// | `1539` | parameter not found | `400` |
/// | `1542` | missing required parameter | `400` |
///
/// `512` is deliberately indistinguishable between an invalid id and a
/// genuinely absent complaint (`9.0-spike`) — `404` is the correct
/// browser-facing outcome either way. Every other code (transport
/// failures, `7 canceled` on an offline vserver, unmapped upstream
/// codes) falls through to the shared [`translate_control_error`] `502`
/// path so server internals never reach the browser.
fn translate_complaint_error(e: ControlBackendError) -> Response {
    if let ControlBackendError::Upstream { code, .. } = &e {
        match code {
            512 => return not_found("complaint not found"),
            1538 | 1539 | 1542 => {
                return validation("TeamSpeak rejected the complaint request parameters");
            }
            _ => {}
        }
    }
    translate_control_error(e)
}

/// `complainlist` row → wire DTO. The TS6-native field names carry
/// straight through (spec §7.15 passthrough surface).
fn complaint_to_wire(c: ComplaintEntry) -> wire::Complaint {
    wire::Complaint {
        tcldbid: c.tcldbid,
        tname: c.tname,
        fcldbid: c.fcldbid,
        fname: c.fname,
        message: c.message,
        timestamp: c.timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webquery::{ClassifiedTransport, WebQueryTransportKind};

    fn upstream(code: i64) -> ControlBackendError {
        ControlBackendError::Upstream {
            code,
            message: "x".into(),
        }
    }

    #[test]
    fn complaint_error_maps_512_to_404() {
        // `complaindel` 512 — invalid id and no-such-complaint are
        // indistinguishable; both surface as 404 (`9.0-spike` §7).
        let resp = translate_complaint_error(upstream(512));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn complaint_error_maps_param_codes_to_400() {
        for code in [1538, 1539, 1542] {
            let resp = translate_complaint_error(upstream(code));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "TS code {code} should map to 400"
            );
        }
    }

    #[test]
    fn complaint_error_falls_through_to_502_for_transport() {
        // Transport-class failures and unmapped upstream codes degrade
        // to the shared 502 path.
        let resp = translate_complaint_error(ControlBackendError::Transport(ClassifiedTransport {
            kind: WebQueryTransportKind::Other,
            message: "boom".into(),
        }));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let resp = translate_complaint_error(upstream(2568));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
