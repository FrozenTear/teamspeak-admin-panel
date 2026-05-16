//! Public appeal surface — `/api/public/moderation/{case,appeals}`
//! (PURA-307, brief §3.2).
//!
//! Two handlers, one flow:
//!
//! 1. [`view_redacted_case`] — a banned subject opens their appeal URL;
//!    the `appeal_token` resolves to a case and the server returns a
//!    **redacted** view (action taken + public reason only). Verification
//!    here is non-consuming — the subject may reload the page before
//!    deciding to appeal; the token is spent exactly once, at submission.
//! 2. [`submit`] — the appeal form POSTs the statement. The server spends
//!    the token, writes a `moderation_appeal` row, moves the case
//!    `actioned → appealed`, and appends an `appeal_filed` timeline row
//!    with `actorUserId = NULL`. Raw appeal prose stays quarantined in
//!    `moderation_appeal`; the case timeline records only that an appeal
//!    was filed (brief §4.7).

// `Result<_, Response>` is the module idiom — see the note in `mod.rs`.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::repos::moderation_appeals::{self, NewModerationAppeal};
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, ModerationCase};
use crate::routes::moderation::tokens::{self, VerifiedToken, VerifyError};

use super::metrics;
use super::{
    ClientIp, FLAG_APPEALS_ENABLED, conflict, disabled, flag_enabled, hash_source_ip, internal,
    invalid_token, validate_text,
};

/// `actionKind`s the redacted case view exposes to the appealing subject:
/// the enforcement actions taken against them. Lifecycle rows
/// (`resolve` / `reopen` / `appeal_filed`) and — critically — operator
/// `note` rows are omitted; a `note` is a moderator note, which the
/// redacted view MUST NOT disclose (brief §6 hook 4).
const VISIBLE_ACTION_KINDS: &[&str] = &["warn", "kick", "ban", "ban_ip", "mute", "unmute", "unban"];

/// `identityProof` recorded on a public appeal — the method, not a
/// password. The single-use, case-scoped `appeal_token` *is* the proof;
/// this string is the audit breadcrumb the operator review pane shows.
const IDENTITY_PROOF: &str = "appeal_token (single-use, case-scoped)";

/// `?token=` query string on the redacted-case view.
#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: String,
}

/// `GET /api/public/moderation/case?token=…` — the redacted view of the
/// case an `appeal_token` is scoped to. Non-consuming (see module docs).
pub async fn view_redacted_case(
    State(state): State<AppState>,
    Query(q): Query<TokenQuery>,
) -> Result<Json<wire::RedactedCase>, Response> {
    if !flag_enabled(&state, FLAG_APPEALS_ENABLED).await {
        return Err(disabled());
    }

    let case_id = match tokens::verify(&state.db, &q.token).await {
        Ok(VerifiedToken::Appeal { case_id }) => case_id,
        Ok(VerifiedToken::ReportChallenge { .. }) => return Err(invalid_token()),
        Err(VerifyError::Invalid) => return Err(invalid_token()),
        Err(VerifyError::Db(e)) => {
            tracing::error!(err = %e, "redacted case view: token verify failed");
            return Err(internal());
        }
    };

    let case = load_case(&state, case_id).await?;
    let pending = pending_appeal_exists(&state, case_id).await?;

    let timeline = moderation_case_actions::list_for_case(&state.db, case_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "redacted case view: timeline lookup failed");
            internal()
        })?
        .into_iter()
        .filter(|a| VISIBLE_ACTION_KINDS.contains(&a.actionKind.as_str()))
        .map(|a| wire::RedactedCaseAction {
            action_kind: a.actionKind,
            reason: a.reason,
            created_at: a.createdAt,
        })
        .collect();

    tracing::debug!(case_id, "public moderation: redacted case view served");
    Ok(Json(wire::RedactedCase {
        case_id: case.id,
        status: case.status.clone(),
        reason: case.reason,
        opened_at: case.openedAt,
        appealable: case.status == "actioned" && !pending,
        timeline,
    }))
}

/// `POST /api/public/moderation/appeals` — file an appeal against the case
/// the `appeal_token` is scoped to.
pub async fn submit(
    State(state): State<AppState>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
    Json(req): Json<wire::PublicAppealRequest>,
) -> Result<(StatusCode, Json<wire::PublicSubmissionAccepted>), Response> {
    if !flag_enabled(&state, FLAG_APPEALS_ENABLED).await {
        return Err(disabled());
    }

    let statement = validate_text("statement", &req.statement).map_err(reject)?;

    // Verify (non-consuming) to learn the case before spending the token —
    // a not-appealable case must leave the token usable so the subject can
    // still load the redacted view.
    let case_id = match tokens::verify(&state.db, &req.token).await {
        Ok(VerifiedToken::Appeal { case_id }) => case_id,
        Ok(VerifiedToken::ReportChallenge { .. }) => return Err(reject(invalid_token())),
        Err(VerifyError::Invalid) => return Err(reject(invalid_token())),
        Err(VerifyError::Db(e)) => {
            tracing::error!(err = %e, "appeal submit: token verify failed");
            metrics::record("appeal", "error");
            return Err(internal());
        }
    };

    let case = load_case_or_reject(&state, case_id).await?;
    if case.status != "actioned" {
        // `appealed` — an appeal is already on file (single appeal per
        // case, brief §3.2). Any other state is simply not appealable.
        let msg = if case.status == "appealed" {
            "an appeal is already on file for this case"
        } else {
            "this case is not in an appealable state"
        };
        return Err(reject(conflict(msg)));
    }
    let pending = match pending_appeal_exists(&state, case_id).await {
        Ok(p) => p,
        Err(resp) => {
            metrics::record("appeal", "error");
            return Err(resp);
        }
    };
    if pending {
        return Err(reject(conflict(
            "an appeal is already pending for this case",
        )));
    }

    // Spend the token. A racing consumer collapses to invalid-token.
    match tokens::verify_and_consume(&state.db, &req.token).await {
        Ok(VerifiedToken::Appeal { case_id: spent }) if spent == case_id => {}
        Ok(_) => return Err(reject(invalid_token())),
        Err(VerifyError::Invalid) => return Err(reject(invalid_token())),
        Err(VerifyError::Db(e)) => {
            tracing::error!(err = %e, "appeal submit: token consume failed");
            metrics::record("appeal", "error");
            return Err(internal());
        }
    }

    let appeal = moderation_appeals::insert(
        &state.db,
        NewModerationAppeal {
            caseId: case_id,
            submitterUid: case.subjectUid.clone(),
            identityProof: IDENTITY_PROOF.to_string(),
            statement,
            sourceIpHash: hash_source_ip(client_ip, &state.jwt_secret),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, case_id, "appeal submit: moderation_appeal insert failed");
        metrics::record("appeal", "error");
        internal()
    })?;

    // Case transition `actioned → appealed`. The token is already spent,
    // so a failure here cannot be retried by the subject — log loudly.
    if let Err(e) = moderation_cases::set_status(&state.db, case_id, "appealed", None).await {
        tracing::error!(err = %e, case_id, appeal_id = appeal.id, "appeal submit: case transition failed");
        metrics::record("appeal", "error");
        return Err(internal());
    }

    // Timeline marker. `actorUserId = NULL` — the appellant is not an
    // operator; the raw appeal prose stays in `moderation_appeal`, only
    // the *fact* of the filing reaches the case timeline (brief §4.7).
    let appended = moderation_case_actions::insert(
        &state.db,
        NewModerationCaseAction {
            caseId: case_id,
            actorUserId: None,
            actorUsernameSnapshot: "appellant".to_string(),
            actionKind: "appeal_filed".to_string(),
            reason: "Appeal filed via the public appeal form".to_string(),
            tsRef: None,
            payload: Some(json!({
                "appealId": appeal.id,
                "identityProof": IDENTITY_PROOF,
            })),
        },
    )
    .await;
    if let Err(e) = appended {
        tracing::error!(err = %e, case_id, appeal_id = appeal.id, "appeal submit: timeline row insert failed");
        metrics::record("appeal", "error");
        return Err(internal());
    }

    metrics::record("appeal", "accepted");
    tracing::info!(
        appeal_id = appeal.id,
        case_id,
        client_ip = %client_ip,
        "public moderation: appeal filed",
    );
    Ok((
        StatusCode::CREATED,
        Json(wire::PublicSubmissionAccepted {
            id: appeal.id,
            status: appeal.status,
        }),
    ))
}

/// Load a case for the redacted view. A missing case behind a valid token
/// means the case was deleted — collapse to the generic invalid-token
/// error rather than confirming the case id ever existed.
async fn load_case(state: &AppState, case_id: i64) -> Result<ModerationCase, Response> {
    match moderation_cases::find_by_id(&state.db, case_id).await {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(invalid_token()),
        Err(e) => {
            tracing::error!(err = %e, case_id, "public moderation: case lookup failed");
            Err(internal())
        }
    }
}

/// As [`load_case`], but tags the failure as a `rejected` appeal for the
/// submission metric.
async fn load_case_or_reject(state: &AppState, case_id: i64) -> Result<ModerationCase, Response> {
    match moderation_cases::find_by_id(&state.db, case_id).await {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(reject(invalid_token())),
        Err(e) => {
            tracing::error!(err = %e, case_id, "appeal submit: case lookup failed");
            metrics::record("appeal", "error");
            Err(internal())
        }
    }
}

/// Whether the case already has a `pending` appeal — the belt to the
/// single-use token's braces on "one appeal per case" (brief §3.2).
async fn pending_appeal_exists(state: &AppState, case_id: i64) -> Result<bool, Response> {
    moderation_appeals::list_for_case(&state.db, case_id)
        .await
        .map(|rows| rows.iter().any(|a| a.status == "pending"))
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "public moderation: appeal lookup failed");
            internal()
        })
}

/// Tag a client-side refusal as a `rejected` appeal submission.
fn reject(resp: Response) -> Response {
    metrics::record("appeal", "rejected");
    resp
}
