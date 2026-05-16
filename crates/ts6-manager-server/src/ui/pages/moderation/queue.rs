//! `/moderation` — the moderation landing surface. PURA-287.
//!
//! Two stacked panels, both scoped to the globally-selected server:
//!
//! - **Cases** — the `moderation_case` queue (`GET /api/moderation/cases`).
//!   A status filter (open / actioned / resolved / all) drives the
//!   server-side query; the open-case form appends a new operator-origin
//!   case. Each row links to [`super::ModerationCasePage`].
//! - **Complaints** — the live TS6 complaint queue
//!   (`GET /api/moderation/complaints`). Read + dismiss only: a TS6
//!   complaint carries a client-database id (`tcldbid`), not the durable
//!   UID a case keys on, so there is no one-click "escalate to case" —
//!   the operator opens a case from the UID once they have it.
//!
//! Page-gated to `admin` + `moderator`; the open-case form and the
//! complaint-dismiss button are additionally suppressed unless the role
//! holds `moderation.case.manage` / `moderation.complaint.resolve`.

use dioxus::prelude::*;
use ts6_manager_shared::admin::Page;
use ts6_manager_shared::moderation::{
    Complaint, DismissReportRequest, ModerationCase, ModerationReport, OpenCaseRequest,
    PromoteReportRequest,
};

use crate::client::api::{self};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;
use crate::ui::routes::Route;

use super::perm;
use super::{
    AccessDenied, case_status_class, fmt_datetime, format_error, origin_label, relative_from_unix,
};

/// Case rows fetched per query. The queue is a working surface, not an
/// archive — a generous single page covers any realistic open backlog.
const CASE_LIMIT: i64 = 100;

/// Status filter options — label + the `?status=` query value (`None` =
/// unconstrained). `Appealed` (Phase 9.2) surfaces cases whose subject
/// has lodged an appeal awaiting an operator decision.
const STATUS_FILTERS: [(&str, Option<&str>); 5] = [
    ("Open", Some("open")),
    ("Actioned", Some("actioned")),
    ("Appealed", Some("appealed")),
    ("Resolved", Some("resolved")),
    ("All", None),
];

/// Origin filter options — label + the `?origin=` query value (`None` =
/// every origin). The `Automod` preset (PURA-303) is how an operator
/// pulls up just the auto-moderation queue.
const ORIGIN_FILTERS: [(&str, Option<&str>); 4] = [
    ("All origins", None),
    ("Operator", Some("operator")),
    ("Complaint", Some("complaint")),
    ("Automod", Some("automod")),
];

#[component]
pub fn ModerationQueuePage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        // AppShell bounces anon sessions to /login; render nothing so
        // there is no flash of moderation chrome.
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
                crumb: "Moderation".to_string(),
                heading: "Moderation".to_string(),
                detail: "The moderation queue is available to moderator and admin accounts only.".to_string(),
            }
        };
    }

    let can_manage_cases = perm::role_holds(&role, "moderation.case.manage");
    let can_view_complaints = perm::role_holds(&role, "moderation.complaint.view");
    let can_resolve_complaints = perm::role_holds(&role, "moderation.complaint.resolve");

    let gate = use_auth_gate();
    let toaster = use_toaster();
    let servers_ctx = use_servers_context();
    let storage = session.storage.clone();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Moderation" }
            h1 { "Moderation" }
            div { class: "empty",
                div { class: "icon", "⊙" }
                h3 { "No server selected" }
                p { "Pick a server from the selector to see its moderation queue." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // ── case queue ──────────────────────────────────────────────────────
    let mut status_idx: Signal<usize> = use_signal(|| 0usize); // default: Open
    let mut origin_idx: Signal<usize> = use_signal(|| 0usize); // default: All
    let cases_reload: Signal<u64> = use_signal(|| 0u64);

    let cases = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let idx = *status_idx.read();
            let oidx = *origin_idx.read();
            let _ = *cases_reload.read(); // subscribe so restart re-fetches
            async move {
                let mut path = format!(
                    "/api/moderation/cases?serverConfigId={server_id}&virtualServerId={sid}&limit={CASE_LIMIT}"
                );
                if let Some(status) = STATUS_FILTERS[idx].1 {
                    path.push_str(&format!("&status={status}"));
                }
                if let Some(origin) = ORIGIN_FILTERS[oidx].1 {
                    path.push_str(&format!("&origin={origin}"));
                }
                api::authorized_get_json::<Page<ModerationCase>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    // ── complaint queue ─────────────────────────────────────────────────
    let mut complaints_reload: Signal<u64> = use_signal(|| 0u64);
    let complaints = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *complaints_reload.read();
            async move {
                if !can_view_complaints {
                    // Don't fan a 403 at the API for a surface the role
                    // can't see — return an empty list quietly.
                    return Ok(Vec::<Complaint>::new());
                }
                let path = format!(
                    "/api/moderation/complaints?serverConfigId={server_id}&virtualServerId={sid}"
                );
                api::authorized_get_json::<Vec<Complaint>>(&gate, &api::api_base(), &path).await
            }
        }
    });

    // ── report intake queue (Phase 9.2) ─────────────────────────────────
    let mut reports_reload: Signal<u64> = use_signal(|| 0u64);
    let reports = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reports_reload.read();
            async move {
                let path = format!(
                    "/api/moderation/reports?serverConfigId={server_id}&virtualServerId={sid}&status=pending"
                );
                api::authorized_get_json::<Vec<ModerationReport>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    // ── open-case form ──────────────────────────────────────────────────
    let mut form_uid: Signal<String> = use_signal(String::new);
    let mut form_nick: Signal<String> = use_signal(String::new);
    let mut form_reason: Signal<String> = use_signal(String::new);
    let mut form_busy: Signal<bool> = use_signal(|| false);
    let nav = use_navigator();

    let on_open_case = {
        let gate = gate.clone();
        move |_| {
            if *form_busy.peek() {
                return;
            }
            let uid = form_uid.peek().trim().to_string();
            let nick = form_nick.peek().trim().to_string();
            let reason = form_reason.peek().trim().to_string();
            if uid.is_empty() || reason.is_empty() {
                toaster.push(
                    ToastVariant::Warning,
                    "Subject UID and reason are required",
                    Some("A case must name the subject and why it was opened.".into()),
                );
                return;
            }
            let req = OpenCaseRequest {
                server_config_id: server_id,
                virtual_server_id: sid,
                subject_uid: uid,
                // Snapshot is best-effort; fall back to the UID when the
                // operator did not type a nickname.
                subject_nickname_snapshot: if nick.is_empty() {
                    form_uid.peek().trim().to_string()
                } else {
                    nick
                },
                reason,
                origin: Some("operator".into()),
                origin_ref: None,
            };
            let gate = gate.clone();
            form_busy.set(true);
            spawn(async move {
                let res = api::authorized_post_json::<_, ModerationCase>(
                    &gate,
                    &api::api_base(),
                    "/api/moderation/cases",
                    Some(&req),
                )
                .await;
                form_busy.set(false);
                match res {
                    Ok(case) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Case #{} opened", case.id),
                            None,
                        );
                        form_uid.set(String::new());
                        form_nick.set(String::new());
                        form_reason.set(String::new());
                        nav.push(Route::ModerationCasePage { case_id: case.id });
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not open case",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let dismiss_complaint = {
        let gate = gate.clone();
        move |(tcldbid, fcldbid): (i64, i64)| {
            let gate = gate.clone();
            spawn(async move {
                let body = ts6_manager_shared::moderation::ResolveComplaintRequest {
                    server_config_id: server_id,
                    virtual_server_id: sid,
                    tcldbid,
                    fcldbid: Some(fcldbid),
                };
                let res = api::authorized_post_json::<_, serde_json::Value>(
                    &gate,
                    &api::api_base(),
                    "/api/moderation/complaints/resolve",
                    Some(&body),
                )
                .await;
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Complaint dismissed", None);
                        complaints_reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not dismiss complaint",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    // Promote a pending report to a case. The report statement becomes
    // the new case's opening reason; on success we jump straight to the
    // freshly-opened case.
    let promote_report = {
        let gate = gate.clone();
        move |(report_id, reason): (i64, String)| {
            let gate = gate.clone();
            spawn(async move {
                let req = PromoteReportRequest { reason };
                let path = format!("/api/moderation/reports/{report_id}/promote");
                let res = api::authorized_post_json::<_, ModerationCase>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                match res {
                    Ok(case) => {
                        toaster.push(
                            ToastVariant::Success,
                            format!("Report promoted — case #{} opened", case.id),
                            None,
                        );
                        reports_reload.with_mut(|n| *n += 1);
                        nav.push(Route::ModerationCasePage { case_id: case.id });
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not promote report",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let dismiss_report = {
        let gate = gate.clone();
        move |report_id: i64| {
            let gate = gate.clone();
            spawn(async move {
                let req = DismissReportRequest { reason: None };
                let path = format!("/api/moderation/reports/{report_id}/dismiss");
                let res = api::authorized_post_json::<_, ModerationReport>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Report dismissed", None);
                        reports_reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not dismiss report",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    let cases_snapshot = cases.read().clone();
    let complaints_snapshot = complaints.read().clone();
    let reports_snapshot = reports.read().clone();
    let active_status = *status_idx.read();
    let active_origin = *origin_idx.read();

    rsx! {
        div { class: "crumb", "Moderation · {server_name}" }
        h1 { "Moderation" }
        p { class: "info-hint",
            "Open cases and live complaints for the selected server. Every case action and resolution is recorded to the audit log."
        }

        // ── Cases panel ─────────────────────────────────────────────────
        section { class: "stack-md mod-panel",
            div { class: "mod-panel-head",
                h2 { "Cases" }
                Link { class: "mod-panel-link", to: Route::AutomodMetricsPage {},
                    "Automod rule metrics →"
                }
            }
            div { class: "mod-filter-bar",
                div { class: "tabs", role: "tablist", "aria-label": "Filter cases by status",
                    for (i , (label , _)) in STATUS_FILTERS.iter().enumerate() {
                        button {
                            key: "{label}",
                            r#type: "button",
                            role: "tab",
                            "aria-selected": if i == active_status { "true" } else { "false" },
                            class: if i == active_status { "tab is-active" } else { "tab" },
                            onclick: move |_| status_idx.set(i),
                            "{label}"
                        }
                    }
                }
                div { class: "tabs", role: "tablist", "aria-label": "Filter cases by origin",
                    for (i , (label , _)) in ORIGIN_FILTERS.iter().enumerate() {
                        button {
                            key: "{label}",
                            r#type: "button",
                            role: "tab",
                            "aria-selected": if i == active_origin { "true" } else { "false" },
                            class: if i == active_origin { "tab is-active" } else { "tab" },
                            onclick: move |_| origin_idx.set(i),
                            "{label}"
                        }
                    }
                }
            }

            if can_manage_cases {
                form {
                    class: "ban-create",
                    "aria-label": "Open a moderation case",
                    onsubmit: move |evt| { evt.prevent_default(); on_open_case.clone()(()); },
                    div { class: "form-row",
                        label { r#for: "case-uid", "Subject UID" }
                        input {
                            id: "case-uid",
                            class: "input",
                            placeholder: "TS unique identifier",
                            value: "{form_uid.read()}",
                            oninput: move |e| form_uid.set(e.value()),
                        }
                    }
                    div { class: "form-row",
                        label { r#for: "case-nick", "Nickname (optional)" }
                        input {
                            id: "case-nick",
                            class: "input",
                            placeholder: "Last known nickname",
                            value: "{form_nick.read()}",
                            oninput: move |e| form_nick.set(e.value()),
                        }
                    }
                    div { class: "form-row",
                        label { r#for: "case-reason", "Reason" }
                        input {
                            id: "case-reason",
                            class: "input",
                            placeholder: "Why this case is being opened",
                            value: "{form_reason.read()}",
                            oninput: move |e| form_reason.set(e.value()),
                        }
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Submit,
                        loading: *form_busy.read(),
                        "Open case"
                    }
                }
            }

            match cases_snapshot {
                None => rsx! {
                    p { class: "info-hint", "Loading cases…" }
                },
                Some(Err(e)) => rsx! {
                    Banner {
                        variant: BannerVariant::Danger,
                        title: "Could not load cases".to_string(),
                        "{format_error(&e)}"
                    }
                },
                Some(Ok(page)) => rsx! {
                    CaseTable { rows: page.items }
                },
            }
        }

        // ── Reports panel (Phase 9.2) ───────────────────────────────────
        section { class: "stack-md mod-panel",
            div { class: "mod-panel-head",
                h2 { "Reports" }
            }
            p { class: "info-hint",
                "Player-filed reports awaiting triage. Promote a report to open a moderation case, or dismiss it without opening one."
            }
            match reports_snapshot {
                None => rsx! {
                    p { class: "info-hint", "Loading reports…" }
                },
                Some(Err(e)) => rsx! {
                    Banner {
                        variant: BannerVariant::Danger,
                        title: "Could not load reports".to_string(),
                        "{format_error(&e)}"
                    }
                },
                Some(Ok(rows)) => rsx! {
                    ReportTable {
                        rows,
                        can_manage: can_manage_cases,
                        on_promote: EventHandler::new(promote_report),
                        on_dismiss: EventHandler::new(dismiss_report),
                    }
                },
            }
        }

        // ── Complaints panel ────────────────────────────────────────────
        if can_view_complaints {
            section { class: "stack-md mod-panel",
                div { class: "mod-panel-head",
                    h2 { "Complaints" }
                }
                p { class: "info-hint",
                    "Player-filed complaints reported by the TeamSpeak server. Dismissing a complaint removes it from the server queue."
                }
                match complaints_snapshot {
                    None => rsx! {
                        p { class: "info-hint", "Loading complaints…" }
                    },
                    Some(Err(e)) => rsx! {
                        Banner {
                            variant: BannerVariant::Danger,
                            title: "Could not load complaints".to_string(),
                            "{format_error(&e)}"
                        }
                    },
                    Some(Ok(rows)) => rsx! {
                        ComplaintTable {
                            rows,
                            can_resolve: can_resolve_complaints,
                            on_dismiss: EventHandler::new(dismiss_complaint),
                        }
                    },
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CaseTableProps {
    rows: Vec<ModerationCase>,
}

#[component]
fn CaseTable(props: CaseTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✓" }
                h3 { "Nothing in this view" }
                p { "No cases match the selected status filter." }
            }
        };
    }
    rsx! {
        table { class: "data-table", "aria-label": "Moderation cases",
            thead {
                tr {
                    th { scope: "col", "Case" }
                    th { scope: "col", "Subject" }
                    th { scope: "col", "Reason" }
                    th { scope: "col", "Origin" }
                    th { scope: "col", "Status" }
                    th { scope: "col", "Opened" }
                }
            }
            tbody {
                for c in props.rows.iter() {
                    {
                        let c = c.clone();
                        rsx! {
                            tr { key: "{c.id}",
                                td {
                                    Link {
                                        to: Route::ModerationCasePage { case_id: c.id },
                                        "#{c.id}"
                                    }
                                }
                                td { class: "client-cell",
                                    span { class: "client-name", "{c.subject_nickname_snapshot}" }
                                    span { class: "client-uid", "{c.subject_uid}" }
                                }
                                td { "{c.reason}" }
                                td { "{origin_label(&c.origin)}" }
                                td {
                                    span { class: case_status_class(&c.status), "{c.status}" }
                                }
                                td { "{fmt_datetime(c.opened_at)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ComplaintTableProps {
    rows: Vec<Complaint>,
    can_resolve: bool,
    /// Carries the `(tcldbid, fcldbid)` pair that addresses the complaint.
    on_dismiss: EventHandler<(i64, i64)>,
}

#[component]
fn ComplaintTable(props: ComplaintTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✓" }
                h3 { "No open complaints" }
                p { "Complaints filed by players will appear here." }
            }
        };
    }
    rsx! {
        table { class: "data-table", "aria-label": "TeamSpeak complaints",
            thead {
                tr {
                    th { scope: "col", "Target" }
                    th { scope: "col", "Complainant" }
                    th { scope: "col", "Message" }
                    th { scope: "col", "Filed" }
                    if props.can_resolve {
                        th { scope: "col", class: "actions-col", "Actions" }
                    }
                }
            }
            tbody {
                for r in props.rows.iter() {
                    {
                        let r = r.clone();
                        let tcldbid = r.tcldbid;
                        let fcldbid = r.fcldbid;
                        let on_dismiss = props.on_dismiss;
                        rsx! {
                            tr { key: "{tcldbid}-{fcldbid}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{r.tname}" }
                                    span { class: "client-uid", "cldbid {tcldbid}" }
                                }
                                td { class: "client-cell",
                                    span { class: "client-name", "{r.fname}" }
                                    span { class: "client-uid", "cldbid {fcldbid}" }
                                }
                                td { "{r.message}" }
                                td { "{relative_from_unix(r.timestamp)}" }
                                if props.can_resolve {
                                    td { class: "actions-col",
                                        Button {
                                            variant: ButtonVariant::Ghost,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_dismiss.call((tcldbid, fcldbid)),
                                            "Dismiss"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ReportTableProps {
    rows: Vec<ModerationReport>,
    can_manage: bool,
    /// Carries `(reportId, caseReason)` — the report statement is sent as
    /// the opening reason of the case the promotion creates.
    on_promote: EventHandler<(i64, String)>,
    on_dismiss: EventHandler<i64>,
}

#[component]
fn ReportTable(props: ReportTableProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✓" }
                h3 { "No pending reports" }
                p { "Reports filed by players will appear here for triage." }
            }
        };
    }
    rsx! {
        table { class: "data-table", "aria-label": "Pending moderation reports",
            thead {
                tr {
                    th { scope: "col", "Subject" }
                    th { scope: "col", "Category" }
                    th { scope: "col", "Report" }
                    th { scope: "col", "Reporter" }
                    th { scope: "col", "Filed" }
                    if props.can_manage {
                        th { scope: "col", class: "actions-col", "Actions" }
                    }
                }
            }
            tbody {
                for r in props.rows.iter() {
                    {
                        // All report-derived text below is rendered through
                        // Dioxus text nodes / attribute bindings, which
                        // escape by default — no `dangerous_inner_html` on
                        // this low-trust prose (PURA-269 plan §6 hook 5).
                        // The evidence URL is shown as inert text, never a
                        // live `href`, so a `javascript:` / `data:` URL
                        // cannot execute from a click.
                        let r = r.clone();
                        let report_id = r.id;
                        let statement = r.statement.clone();
                        let on_promote = props.on_promote;
                        let on_dismiss = props.on_dismiss;
                        rsx! {
                            tr { key: "{report_id}",
                                td { class: "client-cell",
                                    span { class: "client-name", "{r.subject_uid_or_nickname}" }
                                }
                                td { "{r.category}" }
                                td {
                                    p { class: "mod-timeline-reason", "{r.statement}" }
                                    if let Some(url) = r.evidence_url.as_ref().filter(|u| !u.is_empty()) {
                                        p { class: "info-hint",
                                            "Evidence (verify before opening): "
                                            span { class: "mono", "{url}" }
                                        }
                                    }
                                }
                                td { class: "mono", "{r.reporter_uid}" }
                                td { "{fmt_datetime(r.created_at)}" }
                                if props.can_manage {
                                    td { class: "actions-col",
                                        Button {
                                            variant: ButtonVariant::Primary,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_promote.call((report_id, statement.clone())),
                                            "Promote"
                                        }
                                        Button {
                                            variant: ButtonVariant::Ghost,
                                            size: ButtonSize::Small,
                                            onclick: move |_| on_dismiss.call(report_id),
                                            "Dismiss"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
