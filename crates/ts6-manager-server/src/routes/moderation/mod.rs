//! Phase 9.0 moderation REST surface — `/api/moderation/*` (PURA-286,
//! workstream `9.0-routes` of [PURA-262](/PURA/issues/PURA-262) §7).
//!
//! Resources (every endpoint `RequirePermission`-gated — the action-level
//! `moderation.*` catalog from `9.0-rbac`, layered on the coarse role gate):
//!
//! - `cases` — `GET /cases`, `GET /cases/{id}`, `POST /cases`,
//!   `POST /cases/{id}/actions`, `POST /cases/{id}/resolve`,
//!   `POST /cases/{id}/reopen`.
//! - `notes` — `GET|POST /subjects/{uid}/notes`.
//! - `history` — `GET /subjects/{uid}/history`.
//! - `complaints` — `GET /complaints`, `POST /complaints/resolve` (the
//!   TS6 complaint sub-surface, PURA-289).
//!
//! The TS6 **complaint** sub-surface landed in PURA-289 as a follow-up:
//! it needed new `complainlist` / `complaindel` / `complaindelall`
//! WebQuery + SSH-backend wrappers and a cross-cutting `ControlBackend`
//! trait change (see the `9.0-spike` findings,
//! `study-documents/spikes/phase-9.0-ts6-complaint-ban-surface.md`).
//! `POST /complaints/resolve` deviates from the plan §7 path shape
//! `POST /complaints/{id}/resolve` — a TS6 complaint is a
//! `(tcldbid, fcldbid)` pair with no single id, so the pair travels in
//! a JSON body instead. `complainadd` is intentionally absent (board-
//! acked §7.15 deviation, PURA-283).
//!
//! Case state machine (brief §7): `open → actioned → resolved`, plus
//! `resolved → open` (reopen). Every transition appends a
//! `moderation_case_action` row with a required reason **and** writes an
//! `admin_audit_log` row — the action endpoints wrap the existing
//! `routes/control` primitives (kick / ban / mute) rather than
//! re-implementing them.

mod actions;
mod appeals;
mod cases;
mod complaints;
mod history;
mod notes;
mod reports;

#[cfg(test)]
mod tests;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::control::ControlBackendError;
use crate::repos::moderation_appeals::ModerationAppeal;
use crate::repos::moderation_case_actions::ModerationCaseAction;
use crate::repos::moderation_cases::ModerationCase;
use crate::repos::moderation_notes::ModerationNote;
use crate::repos::moderation_reports::ModerationReport;

/// Build the moderation sub-router. Absolute paths — the caller `merge`s
/// this into the top-level router so the URIs line up with plan §7.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/moderation/cases", get(cases::list).post(cases::open))
        .route("/api/moderation/cases/{id}", get(cases::detail))
        .route("/api/moderation/cases/{id}/actions", post(actions::append))
        .route("/api/moderation/cases/{id}/resolve", post(cases::resolve))
        .route("/api/moderation/cases/{id}/reopen", post(cases::reopen))
        .route(
            "/api/moderation/subjects/{uid}/history",
            get(history::subject_history),
        )
        .route(
            "/api/moderation/subjects/{uid}/notes",
            get(notes::list).post(notes::create),
        )
        .route("/api/moderation/complaints", get(complaints::list))
        .route(
            "/api/moderation/complaints/resolve",
            post(complaints::resolve),
        )
        // Phase 9.2 (PURA-308) — report intake triage + appeal decisions.
        .route("/api/moderation/reports", get(reports::list))
        .route(
            "/api/moderation/reports/{id}/promote",
            post(reports::promote),
        )
        .route(
            "/api/moderation/reports/{id}/dismiss",
            post(reports::dismiss),
        )
        .route(
            "/api/moderation/cases/{id}/appeal/uphold",
            post(appeals::uphold),
        )
        .route(
            "/api/moderation/cases/{id}/appeal/overturn",
            post(appeals::overturn),
        )
}

// ---- error helpers (shared by every submodule) ------------------------

pub(super) fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(wire::ErrorBody::new(message))).into_response()
}

pub(super) fn err_with_code(status: StatusCode, message: &str, code: &str) -> Response {
    (status, Json(wire::ErrorBody::new(message).with_code(code))).into_response()
}

pub(super) fn not_found(what: &str) -> Response {
    err_with_code(StatusCode::NOT_FOUND, what, "not_found")
}

pub(super) fn validation(message: &str) -> Response {
    err_with_code(StatusCode::BAD_REQUEST, message, "validation")
}

pub(super) fn conflict(message: &str) -> Response {
    err_with_code(StatusCode::CONFLICT, message, "conflict")
}

pub(super) fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

/// Translate a [`ControlBackendError`] from a wrapped kick / ban / mute
/// into the moderation `ErrorBody` envelope. An upstream TS6 error keeps
/// its code + message; transport / internal errors degrade to a terse
/// `502` so server internals never reach the browser.
pub(super) fn translate_control_error(e: ControlBackendError) -> Response {
    let status = e.http_status();
    if status == StatusCode::BAD_GATEWAY {
        let body = wire::ErrorBody::new("TeamSpeak API Error")
            .with_code(format!("ts6:{}", e.upstream_code()));
        (
            status,
            Json(wire::ErrorBody {
                details: Some(e.upstream_message()),
                ..body
            }),
        )
            .into_response()
    } else {
        err(status, "TeamSpeak control backend unavailable")
    }
}

// ---- repo-row → wire-type conversions ---------------------------------

pub(super) fn case_to_wire(c: ModerationCase) -> wire::ModerationCase {
    wire::ModerationCase {
        id: c.id,
        server_config_id: c.serverConfigId,
        virtual_server_id: c.virtualServerId,
        subject_uid: c.subjectUid,
        subject_nickname_snapshot: c.subjectNicknameSnapshot,
        origin: c.origin,
        origin_ref: c.originRef,
        status: c.status,
        reason: c.reason,
        resolution_note: c.resolutionNote,
        opened_by_user_id: c.openedByUserId,
        opened_at: c.openedAt,
        updated_at: c.updatedAt,
        resolved_at: c.resolvedAt,
    }
}

pub(super) fn action_to_wire(a: ModerationCaseAction) -> wire::ModerationCaseAction {
    wire::ModerationCaseAction {
        id: a.id,
        case_id: a.caseId,
        actor_user_id: a.actorUserId,
        actor_username_snapshot: a.actorUsernameSnapshot,
        action_kind: a.actionKind,
        reason: a.reason,
        ts_ref: a.tsRef,
        payload: a.payload,
        created_at: a.createdAt,
    }
}

pub(super) fn note_to_wire(n: ModerationNote) -> wire::ModerationNote {
    wire::ModerationNote {
        id: n.id,
        subject_uid: n.subjectUid,
        body: n.body,
        author_user_id: n.authorUserId,
        author_username_snapshot: n.authorUsernameSnapshot,
        created_at: n.createdAt,
        updated_at: n.updatedAt,
    }
}

/// `moderation_report` row → wire DTO. `sourceIpHash` is intentionally
/// dropped — it is an abuse-correlation field, not operator-UI data
/// (PURA-269 plan §6 hook 6).
pub(super) fn report_to_wire(r: ModerationReport) -> wire::ModerationReport {
    wire::ModerationReport {
        id: r.id,
        server_config_id: r.serverConfigId,
        virtual_server_id: r.virtualServerId,
        reporter_uid: r.reporterUid,
        subject_uid_or_nickname: r.subjectUidOrNickname,
        category: r.category,
        statement: r.statement,
        evidence_url: r.evidenceUrl,
        status: r.status,
        case_id: r.caseId,
        triaged_by_user_id: r.triagedByUserId,
        created_at: r.createdAt,
        updated_at: r.updatedAt,
    }
}

/// `moderation_appeal` row → wire DTO. `sourceIpHash` is dropped for the
/// same reason as [`report_to_wire`].
pub(super) fn appeal_to_wire(a: ModerationAppeal) -> wire::ModerationAppeal {
    wire::ModerationAppeal {
        id: a.id,
        case_id: a.caseId,
        submitter_uid: a.submitterUid,
        identity_proof: a.identityProof,
        statement: a.statement,
        status: a.status,
        decided_by_user_id: a.decidedByUserId,
        decision_note: a.decisionNote,
        created_at: a.createdAt,
        decided_at: a.decidedAt,
    }
}
