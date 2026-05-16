//! Public report intake — `/api/public/moderation/{request-report-link,reports}`
//! (PURA-307, brief §3.1).
//!
//! Two handlers, one flow:
//!
//! 1. [`request_report_link`] — a connected client asks the server to
//!    mint a `report_challenge_token` and deliver it over the TS6 poke
//!    channel to the client holding the named UID. The token reaches
//!    whoever actually controls the UID, never the HTTP caller — so the
//!    caller cannot harvest a token for a UID they do not control (token
//!    spec hook 3).
//! 2. [`submit`] — the report form POSTs here carrying that token. The
//!    reporter UID is taken from the verified token, not the body; the
//!    report lands in `moderation_report` as `pending` for an operator to
//!    triage. External reports never create a `moderation_case` directly
//!    (brief §3.1) — promotion is an operator-only action.

// `Result<_, Response>` is the module idiom — see the note in `mod.rs`.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::repos::moderation_reports::{self, NewModerationReport};
use crate::repos::server_connections;
use crate::routes::moderation::tokens::{self, DeliverError, VerifiedToken, VerifyError};

use super::metrics;
use super::{
    ClientIp, FLAG_REPORTS_ENABLED, conflict, disabled, flag_enabled, hash_source_ip, internal,
    invalid_token, validate_text, validation,
};

/// Report categories accepted by convention (brief §5). A free-text
/// category would be one more un-vetted string on an unauthenticated
/// route; a closed set keeps the operator triage queue tidy.
pub(super) const CATEGORIES: &[&str] = &["spam", "harassment", "other"];

/// Max length of the optional evidence URL. No file uploads on the 9.2
/// public surface — a URL field only (brief §4.5).
pub(super) const MAX_URL_LEN: usize = 2048;

/// `POST /api/public/moderation/request-report-link` — mint + poke-deliver
/// a `report_challenge_token` to the connected client holding `uid`.
pub async fn request_report_link(
    State(state): State<AppState>,
    Json(req): Json<wire::RequestReportLinkRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Response> {
    if !flag_enabled(&state, FLAG_REPORTS_ENABLED).await {
        return Err(disabled());
    }

    let uid = req.uid.trim();
    if uid.is_empty() {
        metrics::record("report_link", "rejected");
        return Err(validation("uid is required"));
    }

    // Resolve the server connection and a control backend for it.
    let connection = match server_connections::find_by_id(&state.db, req.server_config_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            metrics::record("report_link", "rejected");
            return Err(validation("unknown server"));
        }
        Err(e) => {
            tracing::error!(err = %e, "request-report-link: server lookup failed");
            metrics::record("report_link", "error");
            return Err(internal());
        }
    };
    let backend = match state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %e, "request-report-link: control backend unavailable");
            metrics::record("report_link", "error");
            return Err(super::err(
                StatusCode::BAD_GATEWAY,
                "TeamSpeak control backend unavailable",
                "backend_unavailable",
            ));
        }
    };

    // Mint first, then deliver. A mint failure is internal; a delivery
    // miss (`NotConnected`) is a client-actionable 409 — the report form
    // must be opened while connected to the server.
    let minted = match tokens::mint_report_challenge(&state.db, uid).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(err = %e, "request-report-link: token mint failed");
            metrics::record("report_link", "error");
            return Err(internal());
        }
    };
    let base_url = tokens::public_base_url(&state.db).await;

    match tokens::deliver_report_challenge(
        backend.as_ref(),
        req.server_config_id,
        req.virtual_server_id,
        uid,
        &minted.plaintext,
        base_url.as_deref(),
    )
    .await
    {
        Ok(()) => {
            metrics::record("report_link", "accepted");
            tracing::info!(
                server_config_id = req.server_config_id,
                virtual_server_id = req.virtual_server_id,
                "public moderation: report-challenge token delivered",
            );
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "delivered",
                    "detail": "A single-use report link was sent to your TeamSpeak client.",
                })),
            ))
        }
        Err(DeliverError::NotConnected) => {
            metrics::record("report_link", "rejected");
            Err(conflict(
                "the target client is not connected — open the report form while connected to the server",
            ))
        }
        Err(DeliverError::Control(e)) => {
            tracing::warn!(err = %e, "request-report-link: poke delivery failed");
            metrics::record("report_link", "error");
            Err(super::err(
                StatusCode::BAD_GATEWAY,
                "TeamSpeak control backend unavailable",
                "backend_unavailable",
            ))
        }
    }
}

/// `POST /api/public/moderation/reports` — file a report. Requires a valid
/// `report_challenge_token`; the reporter UID comes from the token.
pub async fn submit(
    State(state): State<AppState>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
    Json(req): Json<wire::PublicReportRequest>,
) -> Result<(StatusCode, Json<wire::PublicSubmissionAccepted>), Response> {
    if !flag_enabled(&state, FLAG_REPORTS_ENABLED).await {
        return Err(disabled());
    }

    // --- field validation (cheap, before any DB / token work) ---------
    let subject =
        validate_text("subjectUidOrNickname", &req.subject_uid_or_nickname).map_err(reject)?;
    let statement = validate_text("statement", &req.statement).map_err(reject)?;
    let category = req.category.trim().to_string();
    if !CATEGORIES.contains(&category.as_str()) {
        return Err(reject(validation(
            "category must be one of spam / harassment / other",
        )));
    }
    let evidence_url = match req.evidence_url.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => Some(validate_evidence_url(u).map_err(reject)?),
        _ => None,
    };

    // --- identity gate: verify (non-consuming) to learn the UID -------
    let uid = match tokens::verify(&state.db, &req.token).await {
        Ok(VerifiedToken::ReportChallenge { uid }) => uid,
        Ok(VerifiedToken::Appeal { .. }) => return Err(reject(invalid_token())),
        Err(VerifyError::Invalid) => return Err(reject(invalid_token())),
        Err(VerifyError::Db(e)) => {
            tracing::error!(err = %e, "report submit: token verify failed");
            metrics::record("report", "error");
            return Err(internal());
        }
    };

    // Per-reporter-UID rate limit — checked before the token is spent so
    // a throttled reporter does not burn a still-valid challenge token.
    super::check_uid_rate_limit(&uid).map_err(reject)?;

    // Spend the token. A racing consumer between `verify` and here makes
    // this fail — collapse to the same generic invalid-token error.
    match tokens::verify_and_consume(&state.db, &req.token).await {
        Ok(VerifiedToken::ReportChallenge { uid: consumed }) if consumed == uid => {}
        Ok(_) => return Err(reject(invalid_token())),
        Err(VerifyError::Invalid) => return Err(reject(invalid_token())),
        Err(VerifyError::Db(e)) => {
            tracing::error!(err = %e, "report submit: token consume failed");
            metrics::record("report", "error");
            return Err(internal());
        }
    }

    let row = moderation_reports::insert(
        &state.db,
        NewModerationReport {
            serverConfigId: req.server_config_id,
            virtualServerId: req.virtual_server_id,
            reporterUid: uid.clone(),
            subjectUidOrNickname: subject,
            category,
            statement,
            evidenceUrl: evidence_url,
            sourceIpHash: hash_source_ip(client_ip, &state.jwt_secret),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, "report submit: moderation_report insert failed");
        metrics::record("report", "error");
        internal()
    })?;

    metrics::record("report", "accepted");
    tracing::info!(
        report_id = row.id,
        server_config_id = req.server_config_id,
        client_ip = %client_ip,
        "public moderation: report filed",
    );
    Ok((
        StatusCode::CREATED,
        Json(wire::PublicSubmissionAccepted {
            id: row.id,
            status: row.status,
        }),
    ))
}

/// Tag a client-side refusal as a `rejected` report submission, then hand
/// back the response unchanged. Keeps the metric call off every early
/// return site.
fn reject(resp: Response) -> Response {
    metrics::record("report", "rejected");
    resp
}

/// Validate the optional evidence URL: an `http` / `https` absolute URL
/// within [`MAX_URL_LEN`]. A non-HTTP scheme (`javascript:`, `data:`) is
/// rejected so the stored value cannot become an XSS vector if a future
/// operator UI mishandles it — defence in depth on top of output
/// escaping (brief §6 hook 5).
fn validate_evidence_url(raw: &str) -> Result<String, Response> {
    if raw.len() > MAX_URL_LEN {
        return Err(validation("evidenceUrl is too long"));
    }
    match url::Url::parse(raw) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => Ok(raw.to_string()),
        _ => Err(validation("evidenceUrl must be an http or https URL")),
    }
}
