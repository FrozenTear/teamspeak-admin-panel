//! `GET /api/moderation/subjects/{uid}/history` — PURA-286.
//!
//! The per-user history pane fans in over every record keyed to a subject
//! UID: all cases, all actions across those cases, and all free-text
//! notes. Cases key on the durable UID (brief §4) so nickname churn does
//! not fork a subject's history.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Response;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequirePermission;
use crate::auth::permissions::HistoryView;
use crate::repos::{moderation_case_actions, moderation_cases, moderation_notes};

use super::{action_to_wire, case_to_wire, internal, note_to_wire};

pub(super) async fn subject_history(
    State(state): State<AppState>,
    _gate: RequirePermission<HistoryView>,
    Path(uid): Path<String>,
) -> Result<Json<wire::SubjectHistory>, Response> {
    let cases = moderation_cases::list_for_subject(&state.db, &uid)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation history: case fan-in failed");
            internal()
        })?;
    let case_ids: Vec<i64> = cases.iter().map(|c| c.id).collect();

    let actions = moderation_case_actions::list_for_cases(&state.db, &case_ids)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation history: action fan-in failed");
            internal()
        })?;

    let notes = moderation_notes::list_for_subject(&state.db, &uid)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation history: note fan-in failed");
            internal()
        })?;

    Ok(Json(wire::SubjectHistory {
        subject_uid: uid,
        cases: cases.into_iter().map(case_to_wire).collect(),
        actions: actions.into_iter().map(action_to_wire).collect(),
        notes: notes.into_iter().map(note_to_wire).collect(),
    }))
}
