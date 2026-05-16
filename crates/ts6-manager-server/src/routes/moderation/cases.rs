//! Case list / detail / open / resolve / reopen — PURA-286.
//!
//! The state machine lives here: [`resolve`] and [`reopen`] are the two
//! lifecycle transitions an operator drives directly (`actioned` is
//! reached implicitly by [`super::actions::append`]). Every transition
//! appends a `moderation_case_action` row with the operator's reason and
//! writes an `admin_audit_log` row.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use ts6_manager_shared::admin::Page;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequirePermission};
use crate::auth::permissions::{CaseManage, CaseView};
use crate::repos::moderation_appeals;
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, CaseFilter, NewModerationCase, ORIGINS};

use super::{case_to_wire, conflict, internal, not_found, validation};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct CaseListQuery {
    subject_uid: Option<String>,
    status: Option<String>,
    server_config_id: Option<i64>,
    virtual_server_id: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /api/moderation/cases` — paginated case queue.
pub(super) async fn list(
    State(state): State<AppState>,
    _gate: RequirePermission<CaseView>,
    Query(q): Query<CaseListQuery>,
) -> Result<Json<Page<wire::ModerationCase>>, Response> {
    if let Some(ref s) = q.status
        && !moderation_cases::STATUSES.contains(&s.as_str())
    {
        return Err(validation(
            "status must be one of open / actioned / resolved / appealed",
        ));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0).max(0);
    let filter = CaseFilter {
        subjectUid: q.subject_uid,
        status: q.status,
        serverConfigId: q.server_config_id,
        virtualServerId: q.virtual_server_id,
    };
    let (rows, total) = moderation_cases::list(&state.db, &filter, limit, offset)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation case list failed");
            internal()
        })?;
    Ok(Json(Page {
        items: rows.into_iter().map(case_to_wire).collect(),
        total,
        limit,
        offset,
    }))
}

/// `GET /api/moderation/cases/{id}` — case detail with the full timeline.
pub(super) async fn detail(
    State(state): State<AppState>,
    _gate: RequirePermission<CaseView>,
    Path(id): Path<i64>,
) -> Result<Json<wire::CaseDetail>, Response> {
    let case = moderation_cases::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;
    let timeline = moderation_case_actions::list_for_case(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case timeline failed");
            internal()
        })?;
    // Phase 9.2 — appeals lodged against this case (newest-first). Empty
    // for the common case; the operator decision panel reads it when the
    // case is in `appealed` status.
    let appeals = moderation_appeals::list_for_case(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case appeals failed");
            internal()
        })?;
    Ok(Json(wire::CaseDetail {
        case: case_to_wire(case),
        timeline: timeline.into_iter().map(super::action_to_wire).collect(),
        appeals: appeals.into_iter().map(super::appeal_to_wire).collect(),
    }))
}

/// `POST /api/moderation/cases` — open a new case.
pub(super) async fn open(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Json(req): Json<wire::OpenCaseRequest>,
) -> Result<(StatusCode, Json<wire::ModerationCase>), Response> {
    let actor = gate.0;

    let subject_uid = req.subject_uid.trim();
    let reason = req.reason.trim();
    if subject_uid.is_empty() {
        return Err(validation("subjectUid is required"));
    }
    if reason.is_empty() {
        return Err(validation("reason is required"));
    }
    let origin = req.origin.as_deref().unwrap_or("operator");
    if !ORIGINS.contains(&origin) {
        return Err(validation(
            "origin must be one of operator / complaint / automod",
        ));
    }

    let case = moderation_cases::insert(
        &state.db,
        NewModerationCase {
            serverConfigId: req.server_config_id,
            virtualServerId: req.virtual_server_id,
            subjectUid: subject_uid.to_string(),
            subjectNicknameSnapshot: req.subject_nickname_snapshot.clone(),
            origin: origin.to_string(),
            originRef: req.origin_ref.clone(),
            reason: reason.to_string(),
            openedByUserId: Some(actor.id),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, "moderation case open failed");
        internal()
    })?;

    audit::record(
        &state.db,
        Event {
            actor: actor.clone(),
            kind: AuditKind::ModerationCaseOpened,
            target: Some(Target::moderation_case(case.id, case.subjectUid.as_str())),
            payload: Some(serde_json::json!({
                "caseId": case.id,
                "subjectUid": case.subjectUid,
                "origin": case.origin,
                "serverConfigId": case.serverConfigId,
                "virtualServerId": case.virtualServerId,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(case_to_wire(case))))
}

/// `POST /api/moderation/cases/{id}/resolve` — `open|actioned → resolved`.
pub(super) async fn resolve(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::ResolveCaseRequest>,
) -> Result<Json<wire::ModerationCase>, Response> {
    let actor = gate.0;
    let note = req.resolution_note.trim();
    if note.is_empty() {
        return Err(validation("resolutionNote is required"));
    }

    let case = load_case(&state, id).await?;
    if case.status == "resolved" {
        return Err(conflict("case is already resolved"));
    }

    let updated = moderation_cases::set_status(&state.db, id, "resolved", Some(note.to_string()))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case resolve failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;

    append_transition(&state, &actor, &updated, "resolve", note).await?;
    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationCaseResolved,
            target: Some(Target::moderation_case(
                updated.id,
                updated.subjectUid.as_str(),
            )),
            payload: Some(serde_json::json!({ "caseId": updated.id })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok(Json(case_to_wire(updated)))
}

/// `POST /api/moderation/cases/{id}/reopen` — `resolved → open`.
pub(super) async fn reopen(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::ReopenCaseRequest>,
) -> Result<Json<wire::ModerationCase>, Response> {
    let actor = gate.0;
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(validation("reason is required"));
    }

    let case = load_case(&state, id).await?;
    if case.status != "resolved" {
        return Err(conflict("only a resolved case can be reopened"));
    }

    let updated = moderation_cases::set_status(&state.db, id, "open", None)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case reopen failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;

    append_transition(&state, &actor, &updated, "reopen", reason).await?;
    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationCaseReopened,
            target: Some(Target::moderation_case(
                updated.id,
                updated.subjectUid.as_str(),
            )),
            payload: Some(serde_json::json!({ "caseId": updated.id })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok(Json(case_to_wire(updated)))
}

/// Load a case or map the absence to `404`.
async fn load_case(
    state: &AppState,
    id: i64,
) -> Result<crate::repos::moderation_cases::ModerationCase, Response> {
    moderation_cases::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))
}

/// Append the `resolve` / `reopen` timeline row that pairs every
/// lifecycle transition (brief §7).
async fn append_transition(
    state: &AppState,
    actor: &crate::auth::extractors::AuthUser,
    case: &crate::repos::moderation_cases::ModerationCase,
    kind: &str,
    reason: &str,
) -> Result<(), Response> {
    moderation_case_actions::insert(
        &state.db,
        NewModerationCaseAction {
            caseId: case.id,
            actorUserId: Some(actor.id),
            actorUsernameSnapshot: actor.username.clone(),
            actionKind: kind.to_string(),
            reason: reason.to_string(),
            tsRef: None,
            payload: None,
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, case_id = case.id, kind, "moderation transition row failed");
        internal()
    })?;
    Ok(())
}
