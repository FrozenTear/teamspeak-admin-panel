//! `POST /api/moderation/cases/{id}/actions` — append a kick / ban /
//! mute / unmute / note action to a case (PURA-286).
//!
//! This handler **wraps** the existing `routes/control` primitives — it
//! dispatches the same typed [`ControlBackend`] calls the control surface
//! uses, then layers the moderation bookkeeping on top: a
//! `moderation_case_action` timeline row, the `open → actioned` status
//! transition, and an `admin_audit_log` row. It does not re-implement
//! kick / ban / mute (plan §7).
//!
//! Per-kind authorization is dynamic: a single handler serves five action
//! kinds, so it cannot use the type-level `RequirePermission<P>` extractor.
//! It resolves the caller's grants once and checks the catalog permission
//! that matches `actionKind` via [`permissions::has_permission`] — the
//! same fail-closed predicate the extractor is built on.
//!
//! The `ban` kind is **UID-keyed** (`banadd?uid=`, `9.0-spike`
//! recommendation 6): a case keys on the durable subject UID, so the ban
//! is too. `ban_ip` is the deliberate exception — it keys on a raw `ip`
//! supplied in the request and is gated by the separate
//! `moderation.action.ban_ip` collateral-damage permission (PURA-290).
//! `clid` in the request is used only by kick / mute / unmute, which act
//! on a live connection.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequireAuth};
use crate::auth::permissions::{
    self, ActionBan, ActionBanIp, ActionKick, ActionMute, ModPermission, NoteWrite,
};
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, ModerationCase};
use crate::repos::{server_connections, user_permissions};
use crate::webquery::BanAddParams;

use super::{
    action_to_wire, conflict, err, internal, not_found, translate_control_error, validation,
};

/// Action kinds this endpoint accepts. `resolve` / `reopen` are *not*
/// here — those are driven by the dedicated lifecycle endpoints.
const ACTION_KINDS: &[&str] = &["kick", "ban", "ban_ip", "mute", "unmute", "note"];

/// TS6 kick reason id for a server kick (`clientkick` `reasonid=5`).
const SERVER_KICK_REASON_ID: i64 = 5;

pub(super) async fn append(
    State(state): State<AppState>,
    RequireAuth(actor): RequireAuth,
    meta: RequestMeta,
    Path(id): Path<i64>,
    Json(req): Json<wire::AppendActionRequest>,
) -> Result<(StatusCode, Json<wire::ModerationCaseAction>), Response> {
    let kind = req.action_kind.trim();
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(validation("reason is required"));
    }
    if !ACTION_KINDS.contains(&kind) {
        return Err(validation(
            "actionKind must be one of kick / ban / ban_ip / mute / unmute / note",
        ));
    }

    // Per-kind catalog permission. `note` is gated by note.write — a case
    // note and a subject note are the same write capability. `ban_ip` is
    // gated by its own collateral-damage permission, distinct from `ban`.
    let wanted = match kind {
        "kick" => ActionKick::PERMISSION,
        "ban" => ActionBan::PERMISSION,
        "ban_ip" => ActionBanIp::PERMISSION,
        "mute" | "unmute" => ActionMute::PERMISSION,
        "note" => NoteWrite::PERMISSION,
        _ => unreachable!("kind validated against ACTION_KINDS above"),
    };
    let grants: Vec<String> = if actor.is_admin() {
        Vec::new()
    } else {
        user_permissions::permissions_for_user(&state.db, actor.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, "moderation action grant lookup failed");
                internal()
            })?
    };
    if !permissions::has_permission(&actor.role, &grants, wanted) {
        return Err(err(StatusCode::FORBIDDEN, "Insufficient permissions"));
    }

    let case = moderation_cases::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id = id, "moderation case lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;
    if case.status == "resolved" {
        return Err(conflict("case is resolved — reopen it before acting"));
    }

    // `kick` / `mute` / `unmute` act on a live connection; `ban` keys on
    // the subject UID; `ban_ip` keys on a request-supplied IP; `note`
    // touches TS6 not at all.
    let needs_clid = matches!(kind, "kick" | "mute" | "unmute");
    if needs_clid && req.clid.is_none() {
        return Err(validation("clid is required for kick / mute / unmute"));
    }
    let ip = if kind == "ban_ip" {
        let ip = req.ip.as_deref().map(str::trim).unwrap_or_default();
        if ip.is_empty() {
            return Err(validation("ip is required for ban_ip"));
        }
        Some(ip)
    } else {
        None
    };

    let ts_ref = dispatch(
        &state,
        &case,
        kind,
        reason,
        req.clid,
        ip,
        req.ban_duration_secs,
    )
    .await?;

    let payload = action_payload(kind, req.clid, ip, req.ban_duration_secs, ts_ref.as_deref());
    let row = moderation_case_actions::insert(
        &state.db,
        NewModerationCaseAction {
            caseId: case.id,
            actorUserId: Some(actor.id),
            actorUsernameSnapshot: actor.username.clone(),
            actionKind: kind.to_string(),
            reason: reason.to_string(),
            tsRef: ts_ref.clone(),
            payload: Some(payload),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, case_id = case.id, "moderation action row insert failed");
        internal()
    })?;

    // State machine: a punitive action moves an `open` case to
    // `actioned`. `unmute` (de-escalation) and `note` leave it untouched.
    if case.status == "open"
        && matches!(kind, "kick" | "ban" | "ban_ip" | "mute")
        && let Err(e) = moderation_cases::set_status(&state.db, case.id, "actioned", None).await
    {
        tracing::error!(err = %e, case_id = case.id, "moderation case actioned transition failed");
        return Err(internal());
    }

    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationCaseActioned,
            target: Some(Target::moderation_case(case.id, case.subjectUid.as_str())),
            payload: Some(json!({
                "caseId": case.id,
                "actionKind": kind,
                "tsRef": ts_ref,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(action_to_wire(row))))
}

/// Dispatch the action to TS6 via the shared [`ControlBackend`]. Returns
/// the `tsRef` to store on the timeline row — the ban id for `ban`,
/// `None` for every other kind. `note` never touches the backend.
async fn dispatch(
    state: &AppState,
    case: &ModerationCase,
    kind: &str,
    reason: &str,
    clid: Option<i64>,
    ip: Option<&str>,
    ban_duration_secs: Option<i64>,
) -> Result<Option<String>, Response> {
    if kind == "note" {
        return Ok(None);
    }

    let connection = server_connections::find_by_id(&state.db, case.serverConfigId)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "moderation action server lookup failed");
            internal()
        })?
        .ok_or_else(|| conflict("the case's server connection no longer exists"))?;
    let backend = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;

    let sid = case.virtualServerId;
    match kind {
        "kick" => {
            backend
                .clientkick(
                    sid,
                    clid.expect("clid validated"),
                    SERVER_KICK_REASON_ID,
                    Some(reason),
                )
                .await
                .map_err(translate_control_error)?;
            Ok(None)
        }
        "mute" => {
            // PURA-292: server-side muting is the `client_is_talker`
            // talker flag, not the client-self `client_*_muted`
            // properties (those are rejected `1538` on a live TS6 host).
            // Revoking the flag is accepted in any channel.
            backend
                .client_set_talker(sid, clid.expect("clid validated"), false)
                .await
                .map_err(translate_control_error)?;
            Ok(None)
        }
        "unmute" => {
            // Restoring the talker flag (`client_is_talker=1`) is rejected
            // with TS6 `1538` when the target is not in a moderated
            // channel — but there the client can already speak, so the
            // unmute intent is already satisfied. Treat that one upstream
            // code as success; surface every other failure.
            match backend
                .client_set_talker(sid, clid.expect("clid validated"), true)
                .await
            {
                Ok(()) => {}
                Err(e) if e.upstream_code() == 1538 => {
                    tracing::info!(
                        case_id = case.id,
                        clid = clid,
                        "unmute: TS6 1538 — target not in a moderated channel, talker flag moot"
                    );
                }
                Err(e) => return Err(translate_control_error(e)),
            }
            Ok(None)
        }
        "ban" => {
            let params = BanAddParams {
                ip: None,
                uid: Some(case.subjectUid.as_str()),
                mytsid: None,
                name: None,
                banreason: Some(reason),
                time: ban_duration_secs,
            };
            let banid = backend
                .banadd(sid, &params)
                .await
                .map_err(translate_control_error)?;
            Ok(Some(banid.to_string()))
        }
        "ban_ip" => {
            let params = BanAddParams {
                ip: Some(ip.expect("ip validated for ban_ip")),
                uid: None,
                mytsid: None,
                name: None,
                banreason: Some(reason),
                time: ban_duration_secs,
            };
            let banid = backend
                .banadd(sid, &params)
                .await
                .map_err(translate_control_error)?;
            Ok(Some(banid.to_string()))
        }
        _ => unreachable!("kind validated against ACTION_KINDS"),
    }
}

/// Per-kind detail object stored on the timeline row's `payload` field.
fn action_payload(
    kind: &str,
    clid: Option<i64>,
    ip: Option<&str>,
    ban_duration_secs: Option<i64>,
    ts_ref: Option<&str>,
) -> serde_json::Value {
    match kind {
        "kick" => json!({ "clid": clid, "reasonId": SERVER_KICK_REASON_ID }),
        "mute" => json!({ "clid": clid, "talker": false }),
        "unmute" => json!({ "clid": clid, "talker": true }),
        "ban" => json!({ "banId": ts_ref, "durationSecs": ban_duration_secs }),
        "ban_ip" => json!({ "banId": ts_ref, "ip": ip, "durationSecs": ban_duration_secs }),
        _ => json!({}),
    }
}
