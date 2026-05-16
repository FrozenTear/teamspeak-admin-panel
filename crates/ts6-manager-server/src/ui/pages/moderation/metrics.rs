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

use dioxus::prelude::*;
use ts6_manager_shared::moderation::AutomodRuleMetrics;

use crate::client::api;
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::routes::Route;

use super::perm;
use super::{AccessDenied, format_error};

/// False-positive rate at or above which the cell is tinted — a rule
/// misfiring this often should not be promoted to `enforce`.
const HIGH_FP_RATE: f64 = 20.0;

#[component]
pub fn AutomodMetricsPage() -> Element {
    let session = use_session();
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

    let gate = use_auth_gate();
    let servers_ctx = use_servers_context();
    let storage = session.storage.clone();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb",
                Link { to: Route::ModerationQueuePage {}, "Moderation" }
                " · Automod"
            }
            h1 { "Automod metrics" }
            div { class: "empty",
                div { class: "icon", "⊙" }
                h3 { "No server selected" }
                p { "Pick a server from the selector to see its automod metrics." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    let metrics = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!(
                    "/api/moderation/automod/metrics?serverConfigId={server_id}&virtualServerId={sid}"
                );
                api::authorized_get_json::<Vec<AutomodRuleMetrics>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let snapshot = metrics.read().clone();

    rsx! {
        div { class: "crumb",
            Link { to: Route::ModerationQueuePage {}, "Moderation" }
            " · Automod · {server_name}"
        }
        h1 { "Automod metrics" }
        p { class: "info-hint",
            "Per-rule outcomes for auto-moderation on the selected server. Use the shadow-hit "
            "and false-positive counts to decide whether a rule is ready to promote from "
            "shadow to enforce."
        }

        section { class: "stack-md mod-panel",
            match snapshot {
                None => rsx! {
                    p { class: "info-hint", "Loading metrics…" }
                },
                Some(Err(e)) => rsx! {
                    Banner {
                        variant: BannerVariant::Danger,
                        title: "Could not load automod metrics".to_string(),
                        "{format_error(&e)}"
                    }
                },
                Some(Ok(rows)) => rsx! {
                    MetricsTable { rows }
                },
            }
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
