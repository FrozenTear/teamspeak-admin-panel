//! `/moderation/automod` — per-rule automod metrics. PURA-303.
//!
//! One table, scoped to the globally-selected server: every automod
//! `ruleKey` that has produced a case, with the counts an operator reads
//! to decide whether to promote a rule from `shadow` to `enforce`:
//!
//! - **Enforced** / **Shadow hits** — automod timeline actions split by
//!   the safeguard `mode`. A rule firing cleanly in shadow is a promotion
//!   candidate; a rule with a high false-positive rate is not.
//! - **False positives** — `resolve` actions an operator flagged as a
//!   misfire ([`super::case_detail`]).
//! - **Breaker trips** — per-rule circuit-breaker trips. Trips are not
//!   yet recorded to a queryable store, so the column currently reads `0`.
//!
//! Page-gated to `admin` + `moderator`, like the rest of `/moderation/*`.
//!
//! Hydration: the metrics `use_resource` is registered on every render
//! and gated on `session.ready` + authenticated + the header pick. A
//! one-shot bail on Anonymous / empty chrome list left this page stuck
//! on **No server selected** after hard-refresh (same family as #20).

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::moderation::AutomodRuleMetrics;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::{ServersData, use_servers_context};
use crate::ui::pages::active_server::{self, ActiveServerSelection};
use crate::ui::routes::Route;

use super::perm;
use super::{AccessDenied, WaitingOnServer, format_error, no_server_selected};

/// False-positive rate at or above which the cell is tinted — a rule
/// misfiring this often should not be promoted to `enforce`.
const HIGH_FP_RATE: f64 = 20.0;

/// Outcome of an automod-metrics load. Loading is `None` on the resource
/// itself — only resolved variants live here.
#[derive(Clone, Debug)]
enum AutomodLoaded {
    WaitingOnSession,
    WaitingOnList,
    NoServers,
    NoSelection,
    Ready(Box<AutomodReadyPayload>),
}

#[derive(Clone, Debug)]
struct AutomodReadyPayload {
    server: ServerSummary,
    rows: Vec<AutomodRuleMetrics>,
}

#[component]
pub fn AutomodMetricsPage() -> Element {
    let session = use_session();
    let gate = use_auth_gate();
    let servers_ctx = use_servers_context();

    // Memo the authed bit so a refresh-gate `update_pair` (token rotation)
    // does not cancel an in-flight metrics fetch. Same as #20's chrome
    // `/api/servers` resource.
    let is_authed = use_memo(move || session.state.read().is_authenticated());

    // Dioxus 0.7: `use_resource` re-runs whenever a tracked signal it
    // depends on changes. Reading ready / authed / chrome list / selected
    // id *inside* the closure means a header pick (or list arrival after
    // rehydrate) cancels any in-flight fetch and starts a new one. A
    // one-shot `use_future`, or an early return before this hook is
    // registered, cannot do that.
    let metrics = use_resource(move || {
        let gate = gate.clone();
        let ready = *session.ready.read();
        let authed = is_authed();
        let list = servers_ctx.data.read().clone();
        let selected_id = *servers_ctx.selected.read();
        async move { fetch_automod_metrics(gate, ready, authed, list, selected_id).await }
    });

    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }

    let role = session
        .state
        .read()
        .user()
        .map(|u| u.role.clone())
        .unwrap_or_default();
    if !perm::role_can_moderate(&role) {
        return rsx! {
            AccessDenied {
                crumb: "Moderation · Automod".to_string(),
                heading: "Automod metrics".to_string(),
                detail: "Automod metrics are available to moderator and admin accounts only.".to_string(),
            }
        };
    }

    let snapshot = metrics.read().clone();

    rsx! {
        { match snapshot {
            None | Some(Ok(AutomodLoaded::WaitingOnSession | AutomodLoaded::WaitingOnList)) => {
                rsx! {
                    WaitingOnServer {
                        crumb: "Moderation · Automod".to_string(),
                        heading: "Automod metrics".to_string(),
                    }
                }
            }
            Some(Ok(AutomodLoaded::NoServers)) => {
                no_server_selected(
                    "Moderation · Automod",
                    "Automod metrics",
                    "Add a TeamSpeak instance from Servers before automod metrics can load.",
                )
            }
            Some(Ok(AutomodLoaded::NoSelection)) => {
                no_server_selected(
                    "Moderation · Automod",
                    "Automod metrics",
                    "Pick a server from the selector to see its automod metrics.",
                )
            }
            Some(Ok(AutomodLoaded::Ready(payload))) => rsx! {
                AutomodReady {
                    server_name: payload.server.name.clone(),
                    rows: payload.rows.clone(),
                }
            },
            Some(Err(e)) => rsx! {
                div { class: "crumb",
                    Link { to: Route::ModerationQueuePage {}, "Moderation" }
                    " · Automod"
                }
                h1 { "Automod metrics" }
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load automod metrics".to_string(),
                    "{format_error(&e)}"
                }
            },
        } }
    }
}

#[derive(Props, Clone, PartialEq)]
struct AutomodReadyProps {
    server_name: String,
    rows: Vec<AutomodRuleMetrics>,
}

#[component]
fn AutomodReady(props: AutomodReadyProps) -> Element {
    rsx! {
        div { class: "crumb",
            Link { to: Route::ModerationQueuePage {}, "Moderation" }
            " · Automod · {props.server_name}"
        }
        h1 { "Automod metrics" }
        p { class: "info-hint",
            "Per-rule outcomes for auto-moderation on the selected server. Use the shadow-hit "
            "and false-positive counts to decide whether a rule is ready to promote from "
            "shadow to enforce."
        }

        section { class: "stack-md mod-panel",
            MetricsTable { rows: props.rows.clone() }
        }
    }
}

async fn fetch_automod_metrics(
    gate: Arc<RefreshGate>,
    ready: bool,
    authenticated: bool,
    list: ServersData,
    selected_id: Option<i64>,
) -> Result<AutomodLoaded, ApiError> {
    if !active_server::should_load_server_scoped(ready, authenticated) {
        return Ok(AutomodLoaded::WaitingOnSession);
    }

    match active_server::selection_from_context(&list, selected_id) {
        ActiveServerSelection::WaitingOnList => Ok(AutomodLoaded::WaitingOnList),
        ActiveServerSelection::ListError(err) => Err(err),
        ActiveServerSelection::NoServers => Ok(AutomodLoaded::NoServers),
        ActiveServerSelection::NoSelection => Ok(AutomodLoaded::NoSelection),
        ActiveServerSelection::Selected(server) => {
            let path = format!(
                "/api/moderation/automod/metrics?serverConfigId={}&virtualServerId={}",
                server.id,
                active_server::DEFAULT_VIRTUAL_SERVER_ID
            );
            let rows =
                api::authorized_get_json::<Vec<AutomodRuleMetrics>>(&gate, &api::api_base(), &path)
                    .await?;
            Ok(AutomodLoaded::Ready(Box::new(AutomodReadyPayload {
                server: *server,
                rows,
            })))
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct MetricsTableProps {
    rows: Vec<AutomodRuleMetrics>,
}

#[component]
fn MetricsTable(props: MetricsTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✓" }
                h3 { "No automod activity" }
                p { "No automation rule has opened a moderation case on this server yet." }
            }
        };
    }
    rsx! {
        table { class: "data-table", "aria-label": "Automod rule metrics",
            thead {
                tr {
                    th { scope: "col", "Rule" }
                    th { scope: "col", "Cases" }
                    th { scope: "col", "Enforced" }
                    th { scope: "col", "Shadow hits" }
                    th { scope: "col", "False positives" }
                    th { scope: "col", "FP rate" }
                    th { scope: "col", "Breaker trips" }
                }
            }
            tbody {
                for m in props.rows.iter() {
                    {
                        let m = m.clone();
                        // FP rate is false positives over total cases; a
                        // rule with no cases yet shows a dash, not 0 %.
                        let (rate_label, rate_high) = if m.cases_total > 0 {
                            let rate = m.false_positives as f64 / m.cases_total as f64 * 100.0;
                            (format!("{rate:.0}%"), rate >= HIGH_FP_RATE)
                        } else {
                            ("—".to_string(), false)
                        };
                        rsx! {
                            tr { key: "{m.rule_key}",
                                td { class: "mono", "{m.rule_key}" }
                                td { "{m.cases_total}" }
                                td { "{m.actions_enforced}" }
                                td { "{m.shadow_hits}" }
                                td { "{m.false_positives}" }
                                td {
                                    class: if rate_high { "mod-rate--high" } else { "" },
                                    "{rate_label}"
                                }
                                td { "{m.circuit_breaker_trips}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hydrate_waits_when_session_is_not_ready() {
        assert!(
            !active_server::should_load_server_scoped(false, false),
            "Anonymous first paint must not fire GET /automod/metrics"
        );
    }

    #[test]
    fn loading_chrome_list_is_not_no_server_selected() {
        assert_eq!(
            active_server::selection_from_context(&ServersData::Loading, Some(42)),
            ActiveServerSelection::WaitingOnList
        );
    }
}
