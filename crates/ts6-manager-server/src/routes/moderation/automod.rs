//! Automod review & override surface — Phase 9.1.4 (PURA-303).
//!
//! Two endpoints layered on the Phase 9.0 case model. Automod cases are
//! ordinary `moderation_case` rows with `origin = automod`; the 9.1.2
//! case bridge (`flow/dispatch.rs`) writes them with an `originRef` of
//! `<ruleKey>:<flowId>` and timeline-action payloads carrying `ruleKey`
//! and the safeguard `mode`. This module reads that shape back:
//!
//! - **`POST …/cases/{caseId}/actions/{actionId}/revert`** — one-click
//!   revert of a `mute` / `ban` automod action. It issues the inverse TS6
//!   command (restore the talker flag / `bandel`) and appends an `unmute`
//!   / `unban` timeline row attributed to the operator. Revert is
//!   idempotent: a second call 409s once a reverting row exists.
//! - **`GET …/automod/metrics`** — per-`ruleKey` aggregates (actions
//!   enforced, shadow hits, false-positive count) an operator reads to
//!   decide whether to promote a rule from `shadow` to `enforce`.
//!
//! Like `actions::append`, the revert endpoint resolves the caller's
//! grants dynamically — a `mute` revert needs `moderation.action.mute`,
//! a `ban` revert needs `moderation.action.ban` — because one handler
//! serves two kinds. The metrics endpoint is `CaseView`-gated.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;
use crate::audit::{self, AuditKind, Event, Outcome, Target};
use crate::auth::extractors::{RequestMeta, RequireAuth, RequirePermission};
use crate::auth::permissions::{self, ActionBan, ActionMute, CaseView, ModPermission};
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, CaseFilter, ModerationCase};
use crate::repos::{server_connections, user_permissions};

use super::{
    action_to_wire, conflict, err, internal, not_found, translate_control_error, validation,
};

/// Action kinds a revert applies to. `kick` / `warn` are point-in-time
/// effects with no inverse; `note` / lifecycle rows are not effects.
const REVERTABLE_KINDS: &[&str] = &["mute", "ban"];

/// Upper bound on automod cases scanned for the metrics view. The metrics
/// surface is a decision aid, not an archive — a generous single page
/// covers any realistic per-server automod history.
const METRICS_CASE_SCAN: i64 = 1000;

// ── revert ──────────────────────────────────────────────────────────────

/// `POST /api/moderation/cases/{caseId}/actions/{actionId}/revert` —
/// undo a `mute` / `ban` automod action.
pub(super) async fn revert_action(
    State(state): State<AppState>,
    RequireAuth(actor): RequireAuth,
    meta: RequestMeta,
    Path((case_id, action_id)): Path<(i64, i64)>,
) -> Result<(StatusCode, Json<wire::ModerationCaseAction>), Response> {
    let case = moderation_cases::find_by_id(&state.db, case_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "automod revert case lookup failed");
            internal()
        })?
        .ok_or_else(|| not_found("case not found"))?;

    // Revert is the automod review affordance — it does not apply to
    // operator / complaint cases, which have their own `unmute` composer.
    if case.origin != "automod" {
        return Err(conflict("revert is available on automod cases only"));
    }

    // Load the timeline once: it locates the target action and proves
    // whether a revert already exists (idempotency).
    let timeline = moderation_case_actions::list_for_case(&state.db, case_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, case_id, "automod revert timeline load failed");
            internal()
        })?;

    let target = timeline
        .iter()
        .find(|a| a.id == action_id)
        .ok_or_else(|| not_found("action not found on this case"))?;

    if !REVERTABLE_KINDS.contains(&target.actionKind.as_str()) {
        return Err(validation("only a mute or ban action can be reverted"));
    }

    // Idempotency — a prior revert appends a row tagged with the source
    // action id. A second call must not fire a second TS6 command.
    let already = timeline.iter().any(|a| {
        a.payload
            .as_ref()
            .and_then(|p| p.get("revertsActionId"))
            .and_then(serde_json::Value::as_i64)
            == Some(action_id)
    });
    if already {
        return Err(conflict("this action has already been reverted"));
    }

    // Per-kind catalog permission: reverting a mute is a mute capability,
    // reverting a ban is a ban capability.
    let wanted = match target.actionKind.as_str() {
        "mute" => ActionMute::PERMISSION,
        "ban" => ActionBan::PERMISSION,
        _ => unreachable!("kind validated against REVERTABLE_KINDS"),
    };
    let grants: Vec<String> = if actor.is_admin() {
        Vec::new()
    } else {
        user_permissions::permissions_for_user(&state.db, actor.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, "automod revert grant lookup failed");
                internal()
            })?
    };
    if !permissions::has_permission(&actor.role, &grants, wanted) {
        return Err(err(StatusCode::FORBIDDEN, "Insufficient permissions"));
    }

    let revert_kind = if target.actionKind == "mute" {
        "unmute"
    } else {
        "unban"
    };
    let reason = format!("Reverted automod {} action #{action_id}", target.actionKind);

    // Dispatch the inverse TS6 command. `tsRef` is the ban id for a `ban`
    // row; a `mute` row records none — the inverse keys on the live clid.
    let ts_ref =
        revert_dispatch(&state, &case, &target.actionKind, target.tsRef.as_deref()).await?;

    let payload = json!({
        "revertsActionId": action_id,
        "revertedKind": target.actionKind,
    });
    let row = moderation_case_actions::insert(
        &state.db,
        NewModerationCaseAction {
            caseId: case.id,
            actorUserId: Some(actor.id),
            actorUsernameSnapshot: actor.username.clone(),
            actionKind: revert_kind.to_string(),
            reason: reason.clone(),
            tsRef: ts_ref.clone(),
            payload: Some(payload),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!(err = %e, case_id, "automod revert row insert failed");
        internal()
    })?;

    audit::record(
        &state.db,
        Event {
            actor,
            kind: AuditKind::ModerationCaseActioned,
            target: Some(Target::moderation_case(case.id, case.subjectUid.as_str())),
            payload: Some(json!({
                "caseId": case.id,
                "actionKind": revert_kind,
                "revertsActionId": action_id,
            })),
            outcome: Outcome::Success,
            error_msg: None,
            request: meta,
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(action_to_wire(row))))
}

/// Issue the inverse TS6 command for a revert. Returns the `tsRef` to
/// store on the reverting row — the lifted ban id for an `unban`, `None`
/// for an `unmute`.
async fn revert_dispatch(
    state: &AppState,
    case: &ModerationCase,
    kind: &str,
    ts_ref: Option<&str>,
) -> Result<Option<String>, Response> {
    let connection = server_connections::find_by_id(&state.db, case.serverConfigId)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "automod revert server lookup failed");
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
        "mute" => {
            // Restore the talker flag on the subject's live connection.
            // If the subject is offline the mute is already moot (the
            // talker flag is per-connection) — record the revert anyway
            // so the operator's intent is on the timeline.
            let clients = backend
                .clientlist(sid)
                .await
                .map_err(translate_control_error)?;
            let clid = clients
                .into_iter()
                .find(|c| c.client_unique_identifier == case.subjectUid)
                .map(|c| c.clid);
            match clid {
                Some(clid) => match backend.client_set_talker(sid, clid, true).await {
                    Ok(()) => {}
                    // TS6 1538 — target not in a moderated channel, so the
                    // talker flag is moot and the unmute intent is met.
                    Err(e) if e.upstream_code() == 1538 => {
                        tracing::info!(
                            case_id = case.id,
                            clid,
                            "automod revert: TS6 1538 — talker flag moot"
                        );
                    }
                    Err(e) => return Err(translate_control_error(e)),
                },
                None => tracing::info!(
                    case_id = case.id,
                    subject = %case.subjectUid,
                    "automod revert: subject offline — mute already moot"
                ),
            }
            Ok(None)
        }
        "ban" => {
            // Lift the ban by the id the 9.1.2 bridge recorded on the
            // `ban` row's `tsRef`. A missing / non-numeric id means the
            // automod ban never landed a server-side ban — nothing to
            // lift, but the revert row is still recorded.
            match ts_ref.and_then(|r| r.parse::<i64>().ok()) {
                Some(ban_id) => {
                    backend
                        .bandel(sid, ban_id)
                        .await
                        .map_err(translate_control_error)?;
                    Ok(Some(ban_id.to_string()))
                }
                None => {
                    tracing::info!(
                        case_id = case.id,
                        "automod revert: ban row carries no ban id — nothing to lift"
                    );
                    Ok(None)
                }
            }
        }
        _ => unreachable!("kind validated against REVERTABLE_KINDS"),
    }
}

// ── per-rule metrics ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct MetricsQuery {
    server_config_id: Option<i64>,
    virtual_server_id: Option<i64>,
}

/// `GET /api/moderation/automod/metrics` — per-`ruleKey` automod metrics.
pub(super) async fn metrics(
    State(state): State<AppState>,
    _gate: RequirePermission<CaseView>,
    Query(q): Query<MetricsQuery>,
) -> Result<Json<Vec<wire::AutomodRuleMetrics>>, Response> {
    let filter = CaseFilter {
        origin: Some("automod".to_string()),
        serverConfigId: q.server_config_id,
        virtualServerId: q.virtual_server_id,
        ..Default::default()
    };
    let (cases, _total) = moderation_cases::list(&state.db, &filter, METRICS_CASE_SCAN, 0)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "automod metrics case list failed");
            internal()
        })?;

    Ok(Json(aggregate_metrics(&state, &cases).await?))
}

/// Running per-rule tally accumulated over a server's automod cases.
#[derive(Default)]
struct RuleAccum {
    cases_total: i64,
    actions_enforced: i64,
    shadow_hits: i64,
    false_positives: i64,
}

/// Fold every automod case + its timeline into per-`ruleKey` rows. Two
/// queries total: the case list (caller-supplied) plus one fan-in over
/// every case's actions.
async fn aggregate_metrics(
    state: &AppState,
    cases: &[ModerationCase],
) -> Result<Vec<wire::AutomodRuleMetrics>, Response> {
    let mut by_rule: HashMap<String, RuleAccum> = HashMap::new();
    let mut case_rule: HashMap<i64, String> = HashMap::new();

    for case in cases {
        let rule_key = rule_key_of(case);
        case_rule.insert(case.id, rule_key.clone());
        by_rule.entry(rule_key).or_default().cases_total += 1;
    }

    let case_ids: Vec<i64> = case_rule.keys().copied().collect();
    let actions = moderation_case_actions::list_for_cases(&state.db, &case_ids)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "automod metrics action fan-in failed");
            internal()
        })?;

    for a in &actions {
        let Some(rule_key) = case_rule.get(&a.caseId) else {
            continue;
        };
        let accum = by_rule.entry(rule_key.clone()).or_default();
        // automod-authored effect rows split by safeguard mode; operator
        // rows (revert, resolve) are attributed to a username, not
        // `automod`, so they never land in the effect counts.
        if a.actorUsernameSnapshot == "automod" {
            let mode = a
                .payload
                .as_ref()
                .and_then(|p| p.get("mode"))
                .and_then(serde_json::Value::as_str);
            if mode == Some("shadow") {
                accum.shadow_hits += 1;
            } else {
                accum.actions_enforced += 1;
            }
        }
        if a.actionKind == "resolve"
            && a.payload
                .as_ref()
                .and_then(|p| p.get("falsePositive"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            accum.false_positives += 1;
        }
    }

    let mut rows: Vec<wire::AutomodRuleMetrics> = by_rule
        .into_iter()
        .map(|(rule_key, a)| wire::AutomodRuleMetrics {
            rule_key,
            cases_total: a.cases_total,
            actions_enforced: a.actions_enforced,
            shadow_hits: a.shadow_hits,
            false_positives: a.false_positives,
            // Circuit-breaker trips are not yet written to a queryable
            // store — the safeguard engine demotes the rule in-process
            // without an audit row to count. Reported as 0 until that
            // instrumentation lands.
            circuit_breaker_trips: 0,
        })
        .collect();
    // Busiest rule first, ties broken by key for a stable view.
    rows.sort_by(|x, y| {
        y.cases_total
            .cmp(&x.cases_total)
            .then_with(|| x.rule_key.cmp(&y.rule_key))
    });
    Ok(rows)
}

/// Extract the `ruleKey` from a case `originRef`. The 9.1.2 bridge writes
/// `<ruleKey>:<flowId>`; a `ruleKey` may itself contain `:`, so the split
/// drops only the trailing numeric flow id. An absent / malformed
/// `originRef` degrades to `"unknown"` rather than dropping the case.
fn rule_key_of(case: &ModerationCase) -> String {
    match case.originRef.as_deref() {
        Some(r) => match r.rsplit_once(':') {
            Some((key, _flow)) if !key.is_empty() => key.to_string(),
            _ => r.to_string(),
        },
        None => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: i64, origin_ref: Option<&str>) -> ModerationCase {
        ModerationCase {
            id,
            serverConfigId: 1,
            virtualServerId: 1,
            subjectUid: format!("uid-{id}"),
            subjectNicknameSnapshot: "Nick".into(),
            origin: "automod".into(),
            originRef: origin_ref.map(str::to_string),
            status: "actioned".into(),
            reason: "spam".into(),
            resolutionNote: None,
            openedByUserId: None,
            openedAt: chrono::Utc::now(),
            updatedAt: chrono::Utc::now(),
            resolvedAt: None,
        }
    }

    #[test]
    fn rule_key_drops_trailing_flow_id() {
        assert_eq!(rule_key_of(&case(1, Some("bad-name:7"))), "bad-name");
        // A ruleKey containing a colon keeps everything but the flow id.
        assert_eq!(rule_key_of(&case(2, Some("ns:bad-name:42"))), "ns:bad-name");
    }

    #[test]
    fn rule_key_degrades_for_missing_origin_ref() {
        assert_eq!(rule_key_of(&case(3, None)), "unknown");
    }
}
