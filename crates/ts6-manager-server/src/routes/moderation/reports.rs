//! Report-intake triage — `/api/moderation/reports*` (PURA-308,
//! workstream `9.2-operator-ui` of [PURA-269](/PURA/issues/PURA-269) §9).
//!
//! A `moderation_report` is low-trust inbound: it lands in a `pending`
//! intake queue (the public submit handler is the 9.2-public-routes
//! workstream, PURA-307) and never touches the case aggregate until an
//! operator triages it here. Two outcomes, both operator-only:
//!
//! - **promote** — opens a `moderation_case` (`origin='report'`,
//!   `originRef=<reportId>`) and flips the report to `promoted`.
//!   Promotion is the *only* path from a report to a case, so the abuse
//!   blast radius of the public route stops at this queue (plan §3.1).
//! - **dismiss** — closes the report `dismissed` without opening a case.
//!
//! Listing is gated by `moderation.case.view`; both triage actions by
//! `moderation.case.manage` — a report promotion *is* a case open.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequirePermission};
use crate::auth::permissions::{CaseManage, CaseView};
use crate::repos::moderation_cases::{self, NewModerationCase};
use crate::repos::moderation_reports;

use super::{case_to_wire, conflict, internal, not_found, report_to_wire, validation};

/// Valid report triage states (`moderation_report.status`).
const REPORT_STATUSES: &[&str] = &["pending", "promoted", "dismissed"];

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReportListQuery {
    server_config_id: Option<i64>,
    virtual_server_id: Option<i64>,
    status: Option<String>,
}

/// `GET /api/moderation/reports` — the report intake queue for a server,
/// newest-first. `status` defaults to `pending` (the operator's working
/// set); the server scope is applied here rather than in the repo so the
/// shared `list_by_status` primitive stays scope-agnostic.
pub(super) async fn list(
    State(state): State<AppState>,
    _gate: RequirePermission<CaseView>,
    Query(q): Query<ReportListQuery>,
) -> Result<Json<Vec<wire::ModerationReport>>, Response> {
    let status = q.status.as_deref().unwrap_or("pending");
    if !REPORT_STATUSES.contains(&status) {
        return Err(validation(
            "status must be one of pending / promoted / dismissed",
        ));
    }
    let rows = moderation_reports::list_by_status(&state.db, status)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation report list failed");
            internal()
        })?;
    let items = rows
        .into_iter()
        .filter(|r| {
            q.server_config_id.is_none_or(|v| r.serverConfigId == v)
                && q.virtual_server_id.is_none_or(|v| r.virtualServerId == v)
        })
        .map(report_to_wire)
        .collect();
    Ok(Json(items))
}

/// `POST /api/moderation/reports/{id}/promote` — promote a pending report
/// to a `moderation_case`. The supplied `reason` becomes the case's
/// opening reason; the report's accused becomes the case subject.
pub(super) async fn promote(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::PromoteReportRequest>,
) -> Result<(StatusCode, Json<wire::ModerationCase>), Response> {
    let actor = gate.0;
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(validation("reason is required"));
    }

    let report = load_pending(&state, id).await?;

    // A report names its accused by UID *or* free-text nickname — a
    // durable UID is not always known pre-case. The case keys on whatever
    // the report carried; the snapshot mirrors it.
    let case = moderation_cases::insert(
        &state.db,
        NewModerationCase {
            serverConfigId: report.serverConfigId,
            virtualServerId: report.virtualServerId,
            subjectUid: report.subjectUidOrNickname.clone(),
            subjectNicknameSnapshot: report.subjectUidOrNickname.clone(),
            origin: "report".to_string(),
            originRef: Some(report.id.to_string()),
            reason: reason.to_string(),
            openedByUserId: Some(actor.id),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, report_id = id, "report promote — case open failed");
        internal()
    })?;

    moderation_reports::promote(&state.db, report.id, case.id, Some(actor.id))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, report_id = id, "report promote — status flip failed");
            internal()
        })?;

    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationReportPromoted,
            target: Some(Target::moderation_report(
                report.id,
                report.subjectUidOrNickname.as_str(),
            )),
            payload: Some(serde_json::json!({
                "reportId": report.id,
                "caseId": case.id,
                "subjectUidOrNickname": report.subjectUidOrNickname,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(case_to_wire(case))))
}

/// `POST /api/moderation/reports/{id}/dismiss` — close a pending report
/// without opening a case.
pub(super) async fn dismiss(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::DismissReportRequest>,
) -> Result<Json<wire::ModerationReport>, Response> {
    let actor = gate.0;
    let report = load_pending(&state, id).await?;

    let dismissed = moderation_reports::dismiss(&state.db, report.id, Some(actor.id))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, report_id = id, "report dismiss failed");
            internal()
        })?
        .ok_or_else(|| not_found("report not found"))?;

    let note = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationReportDismissed,
            target: Some(Target::moderation_report(
                dismissed.id,
                dismissed.subjectUidOrNickname.as_str(),
            )),
            payload: Some(serde_json::json!({
                "reportId": dismissed.id,
                "reason": note,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok(Json(report_to_wire(dismissed)))
}

/// Load a report and require it to be `pending` — triage is a one-shot
/// transition, so a `promoted` / `dismissed` report is a `409`.
async fn load_pending(
    state: &AppState,
    id: i64,
) -> Result<crate::repos::moderation_reports::ModerationReport, Response> {
    let report = moderation_reports::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, report_id = id, "moderation report lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("report not found"))?;
    if report.status != "pending" {
        return Err(conflict("report has already been triaged"));
    }
    Ok(report)
}
