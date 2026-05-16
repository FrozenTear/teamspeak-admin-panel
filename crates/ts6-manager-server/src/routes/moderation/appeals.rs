//! Appeal decisions — `/api/moderation/cases/{id}/appeal/{uphold,
//! overturn}` (PURA-308, workstream `9.2-operator-ui` of
//! [PURA-269](/PURA/issues/PURA-269) §9).
//!
//! A subject lodges an appeal via the public surface (9.2-public-routes,
//! PURA-307), which moves the case `actioned → appealed` and appends an
//! `appeal_filed` timeline row. The two endpoints here are the operator's
//! terminal decisions, both `appealed → resolved`:
//!
//! - **uphold** — the original action stands. `appeal.status=upheld`.
//! - **overturn** — the action was wrong. `appeal.status=overturned`,
//!   and any TeamSpeak ban tied to the case is **lifted** (`bandel`) and
//!   recorded as its own `unban` timeline row before the case closes.
//!
//! Both append an `appeal_decided` row and resolve the case. A mute or a
//! kick needs no reversal dispatch — the talker flag and a kick are
//! per-session and do not survive the subject's disconnect — so only a
//! ban has a server-side reversal. Gated by `moderation.case.manage`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{AuthUser, RequestMeta, RequirePermission};
use crate::auth::permissions::CaseManage;
use crate::control::ControlBackend;
use crate::repos::moderation_appeals::{self, ModerationAppeal};
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, ModerationCase};
use crate::repos::server_connections;

use super::{case_to_wire, conflict, internal, not_found, translate_control_error};

/// `POST /api/moderation/cases/{id}/appeal/uphold` — the appeal fails;
/// the original moderation action stands and the case resolves.
pub(super) async fn uphold(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::DecideAppealRequest>,
) -> Result<Json<wire::ModerationCase>, Response> {
    let actor = gate.0;
    let (case, appeal) = load_appeal_under_review(&state, id).await?;
    let note = trimmed_note(&req.decision_note);

    decide_appeal(&state, appeal.id, "upheld", actor.id, note.clone()).await?;
    append_appeal_decided(
        &state,
        &actor,
        case.id,
        appeal.id,
        "upheld",
        "Appeal upheld — original moderation action stands.",
        note.as_deref(),
        None,
    )
    .await?;
    let updated = resolve_case(&state, case.id, "Appeal upheld.", note.as_deref()).await?;
    record_audit(&state, actor, meta, &case, appeal.id, "upheld", None).await;

    Ok(Json(case_to_wire(updated)))
}

/// `POST /api/moderation/cases/{id}/appeal/overturn` — the appeal
/// succeeds. Any TeamSpeak ban on the case timeline is lifted as its own
/// `unban` timeline row, then the case resolves.
pub(super) async fn overturn(
    State(state): State<AppState>,
    gate: RequirePermission<CaseManage>,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::DecideAppealRequest>,
) -> Result<Json<wire::ModerationCase>, Response> {
    let actor = gate.0;
    let (case, appeal) = load_appeal_under_review(&state, id).await?;
    let note = trimmed_note(&req.decision_note);

    // Reversal first — if the ban lift fails upstream, nothing has been
    // mutated yet, so the appeal stays cleanly re-decidable.
    let lifted_ban = lift_case_ban(&state, &case).await?;

    decide_appeal(&state, appeal.id, "overturned", actor.id, note.clone()).await?;

    // The reversal is its own timeline action (plan §3.2).
    if let Some(ban_id) = lifted_ban {
        moderation_case_actions::insert(
            &state.db,
            NewModerationCaseAction {
                caseId: case.id,
                actorUserId: Some(actor.id),
                actorUsernameSnapshot: actor.username.clone(),
                actionKind: "unban".to_string(),
                reason: format!("Appeal overturned — TeamSpeak ban #{ban_id} lifted."),
                tsRef: Some(ban_id.to_string()),
                payload: Some(json!({ "banId": ban_id, "appealId": appeal.id })),
            },
        )
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = case.id, "overturn unban row insert failed");
            internal()
        })?;
    }

    let reversal = if lifted_ban.is_some() {
        "unban"
    } else {
        "none"
    };
    append_appeal_decided(
        &state,
        &actor,
        case.id,
        appeal.id,
        "overturned",
        "Appeal overturned — original moderation action reversed.",
        note.as_deref(),
        Some(reversal),
    )
    .await?;

    let resolution = match lifted_ban {
        Some(ban_id) => format!("Appeal overturned. TeamSpeak ban #{ban_id} lifted."),
        None => "Appeal overturned.".to_string(),
    };
    let updated = resolve_case(&state, case.id, &resolution, note.as_deref()).await?;
    record_audit(
        &state,
        actor,
        meta,
        &case,
        appeal.id,
        "overturned",
        Some(reversal),
    )
    .await;

    Ok(Json(case_to_wire(updated)))
}

// ── shared helpers ──────────────────────────────────────────────────────

/// Load the case and its single pending appeal, requiring the case to be
/// in `appealed` status. Maps every "nothing to decide" condition to a
/// first-class `404` / `409`.
async fn load_appeal_under_review(
    state: &AppState,
    case_id: i64,
) -> Result<(ModerationCase, ModerationAppeal), Response> {
    let case = moderation_cases::find_by_id(&state.db, case_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "appeal decision — case lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;
    if case.status != "appealed" {
        return Err(conflict("case is not under appeal"));
    }

    let appeals = moderation_appeals::list_for_case(&state.db, case_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "appeal decision — appeal lookup failed");
            internal()
        })?;
    let appeal = appeals
        .into_iter()
        .find(|a| a.status == "pending")
        .ok_or_else(|| conflict("no pending appeal on this case"))?;
    Ok((case, appeal))
}

/// Normalise the optional operator note — `None` for absent / whitespace.
fn trimmed_note(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Record the terminal appeal decision on the `moderation_appeal` row.
async fn decide_appeal(
    state: &AppState,
    appeal_id: i64,
    status: &str,
    actor_id: i64,
    note: Option<String>,
) -> Result<(), Response> {
    moderation_appeals::decide(&state.db, appeal_id, status, Some(actor_id), note)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, appeal_id, status, "appeal decide failed");
            internal()
        })?
        .ok_or_else(|| not_found("appeal not found"))?;
    Ok(())
}

/// Append the `appeal_decided` timeline row that pairs every appeal
/// decision (plan §3.2).
#[allow(clippy::too_many_arguments)]
async fn append_appeal_decided(
    state: &AppState,
    actor: &AuthUser,
    case_id: i64,
    appeal_id: i64,
    outcome: &str,
    base_reason: &str,
    note: Option<&str>,
    reversal: Option<&str>,
) -> Result<(), Response> {
    let reason = match note {
        Some(n) => format!("{base_reason} {n}"),
        None => base_reason.to_string(),
    };
    let mut payload = json!({ "appealId": appeal_id, "outcome": outcome });
    if let Some(r) = reversal {
        payload["reversal"] = json!(r);
    }
    moderation_case_actions::insert(
        &state.db,
        NewModerationCaseAction {
            caseId: case_id,
            actorUserId: Some(actor.id),
            actorUsernameSnapshot: actor.username.clone(),
            actionKind: "appeal_decided".to_string(),
            reason,
            tsRef: None,
            payload: Some(payload),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, case_id, "appeal_decided row insert failed");
        internal()
    })?;
    Ok(())
}

/// Move the case `appealed → resolved`, stamping the resolution note.
async fn resolve_case(
    state: &AppState,
    case_id: i64,
    base: &str,
    note: Option<&str>,
) -> Result<ModerationCase, Response> {
    let resolution = match note {
        Some(n) => format!("{base} {n}"),
        None => base.to_string(),
    };
    moderation_cases::set_status(&state.db, case_id, "resolved", Some(resolution))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "appeal decision — case resolve failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))
}

/// Lift the TeamSpeak ban tied to a case, if any. Scans the timeline for
/// the most recent `ban` / `ban_ip` action carrying a numeric ban id
/// (`tsRef`) and issues `bandel`. Returns the lifted ban id, or `None`
/// when the case carries no reversible ban (mute / kick / note only).
async fn lift_case_ban(state: &AppState, case: &ModerationCase) -> Result<Option<i64>, Response> {
    let timeline = moderation_case_actions::list_for_case(&state.db, case.id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = case.id, "overturn — timeline lookup failed");
            internal()
        })?;
    // `list_for_case` is chronological; the most recent ban is the last
    // ban-kind row carrying a parseable ban id.
    let ban_id = timeline
        .iter()
        .rev()
        .filter(|a| matches!(a.actionKind.as_str(), "ban" | "ban_ip"))
        .find_map(|a| a.tsRef.as_deref().and_then(|r| r.parse::<i64>().ok()));
    let Some(ban_id) = ban_id else {
        return Ok(None);
    };

    let connection = server_connections::find_by_id(&state.db, case.serverConfigId)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "overturn — server lookup failed");
            internal()
        })?
        .ok_or_else(|| conflict("the case's server connection no longer exists"))?;
    let backend: std::sync::Arc<dyn ControlBackend> = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;
    backend
        .bandel(case.virtualServerId, ban_id)
        .await
        .map_err(translate_control_error)?;
    Ok(Some(ban_id))
}

/// Write the `moderationAppealDecided` audit row.
#[allow(clippy::too_many_arguments)]
async fn record_audit(
    state: &AppState,
    actor: AuthUser,
    meta: RequestMeta,
    case: &ModerationCase,
    appeal_id: i64,
    outcome: &str,
    reversal: Option<&str>,
) {
    let mut payload = json!({
        "caseId": case.id,
        "appealId": appeal_id,
        "outcome": outcome,
    });
    if let Some(r) = reversal {
        payload["reversal"] = json!(r);
    }
    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationAppealDecided,
            target: Some(Target::moderation_case(case.id, case.subjectUid.as_str())),
            payload: Some(payload),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;
}
