//! `GET|POST /api/moderation/subjects/{uid}/notes` — PURA-286.
//!
//! Free-text moderator notes on a subject UID, independent of cases
//! (brief §5). Reads are gated by `moderation.note.view`, writes by
//! `moderation.note.write`. A note write emits a `moderationNoteAdded`
//! `admin_audit_log` row — notes are personal data, so their creation is
//! itself an auditable moderation event.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequirePermission};
use crate::auth::permissions::{NoteView, NoteWrite};
use crate::repos::moderation_notes::{self, NewModerationNote};

use super::{internal, note_to_wire, validation};

/// `GET /api/moderation/subjects/{uid}/notes` — newest-first.
pub(super) async fn list(
    State(state): State<AppState>,
    _gate: RequirePermission<NoteView>,
    Path(uid): Path<String>,
) -> Result<Json<Vec<wire::ModerationNote>>, Response> {
    let notes = moderation_notes::list_for_subject(&state.db, &uid)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation note list failed");
            internal()
        })?;
    Ok(Json(notes.into_iter().map(note_to_wire).collect()))
}

/// `POST /api/moderation/subjects/{uid}/notes` — create a note.
pub(super) async fn create(
    State(state): State<AppState>,
    gate: RequirePermission<NoteWrite>,
    meta: RequestMeta,
    Path(uid): Path<String>,
    Json(req): Json<wire::CreateNoteRequest>,
) -> Result<(StatusCode, Json<wire::ModerationNote>), Response> {
    let actor = gate.0;
    let body = req.body.trim();
    if body.is_empty() {
        return Err(validation("body is required"));
    }

    let note = moderation_notes::insert(
        &state.db,
        NewModerationNote {
            subjectUid: uid.clone(),
            body: body.to_string(),
            authorUserId: Some(actor.id),
            authorUsernameSnapshot: actor.username.clone(),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, "moderation note insert failed");
        internal()
    })?;

    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationNoteAdded,
            target: Some(Target::moderation_subject(uid.as_str())),
            payload: Some(serde_json::json!({
                "noteId": note.id,
                "subjectUid": uid,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(note_to_wire(note))))
}
