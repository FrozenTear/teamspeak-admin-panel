//! `/admin/audit` — v1.1 admin audit-log viewer. PURA-238.
//!
//! A read-only viewer over the `admin_audit_log` table, wired to the real
//! `GET /api/audit` route ([`crate::routes::audit`], PURA-235). Per
//! `docs/admin/ui-brief.md` §3.5 the surface is three columns:
//!
//! - **Left rail** — filters: kind, actor, target, outcome, date range.
//!   The four filter dimensions (kind / actor / target / time) each narrow
//!   the server-side result set via `GET /api/audit` query params.
//! - **Centre** — a dense, newest-first table. Pagination is cursor-style
//!   scroll-to-load: 50 rows per fetch, accumulated client-side, with an
//!   explicit "Load more" control that doubles as the keyboard-reachable
//!   a11y affordance for the infinite-scroll behaviour.
//! - **Right panel** — the clicked row's full detail: redacted payload,
//!   request trace fields, and a copy-to-clipboard for the row id.
//!
//! ## Admin gating
//!
//! The nav entry is hidden for non-admins ([`crate::ui::layout`] threads
//! `is_admin` into the sidebar). Reaching `/admin/audit` directly as a
//! non-admin renders an in-page 403 surface. Both are *visual* gates: the
//! JWT role claim drives them, but `GET /api/audit` re-checks the
//! DB-current role server-side (`RequireAdmin`) and is the authoritative
//! boundary — a forged nav entry still hits a 403 at the API.
//!
//! ## Redaction
//!
//! The audit writer (PURA-236) redacts credential-bearing fields before the
//! row is persisted. [`redact_payload`] is a defence-in-depth second pass
//! on the client: even if a future writer regression let a `password` /
//! `apiKey` / `sshKey` field through, the detail panel never renders it.
//!
//! ## Out of v1.1 scope
//!
//! CSV export (`docs/admin/ui-brief.md` §8, architecture §2.2). The toolbar
//! carries a disabled "Export CSV" button as the hook point — wiring it is
//! a v1.2 follow-up that only needs an `on_export` handler here plus the
//! `POST /api/audit/export` route.

use chrono::{Duration, Utc};
use dioxus::prelude::*;
use serde_json::Value;
use ts6_manager_shared::admin::{AdminUser, AuditEvent, Page};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};

/// Rows fetched per `GET /api/audit` call. Matches the route's documented
/// default page size (`docs/admin/http-api.md` §3.4).
const ROWS_PER_PAGE: i64 = 50;

/// The ten v1.1 audit event kinds (`docs/admin/audit-shape.md` §2.1) paired
/// with operator-facing labels. The wire string is the stable external
/// contract; the label is presentation-only and safe to reword.
const AUDIT_KINDS: [(&str, &str); 10] = [
    ("userCreated", "User created"),
    ("userPatched", "User patched"),
    ("userDisabled", "User disabled"),
    ("userEnabled", "User enabled"),
    ("userRoleChanged", "Role changed"),
    ("userPasswordReset", "Password reset"),
    ("userDeleted", "User deleted"),
    ("sessionRevoked", "Session revoked"),
    ("selfPasswordChanged", "Self password change"),
    ("setupCompleted", "Setup completed"),
];

/// The applied filter set. Each field maps to a `GET /api/audit` query
/// param; `None` means "do not constrain this dimension".
#[derive(Clone, PartialEq, Eq, Default)]
struct AuditFilters {
    /// Exact `kind` discriminant match.
    kind: Option<String>,
    /// `actorUserId` exact match.
    actor_user_id: Option<i64>,
    /// Target user — sent as the paired `targetKind=user&targetId=<id>`.
    target_user_id: Option<i64>,
    /// `outcome` — `success` or `failure`.
    outcome: Option<String>,
    /// Inclusive lower bound on `occurredAt`, `YYYY-MM-DD` (local date input).
    from: Option<String>,
    /// Inclusive upper bound on `occurredAt`, `YYYY-MM-DD`.
    to: Option<String>,
}

impl AuditFilters {
    /// `true` when the operator has narrowed past the default 24h window —
    /// drives the empty-state copy (no-events vs no-match, ui-brief §3.5).
    fn is_narrowed(&self) -> bool {
        self.kind.is_some()
            || self.actor_user_id.is_some()
            || self.target_user_id.is_some()
            || self.outcome.is_some()
            || self.to.is_some()
    }
}

#[component]
pub fn AuditPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        // AppShell bounces anon sessions to /login; render nothing in the
        // gap so there's no flash of admin chrome.
        return rsx! { "" };
    }

    // Visual admin gate (ui-brief §5). The JWT role claim is sufficient for
    // hiding the surface; `GET /api/audit` re-checks the DB-current role.
    let is_admin = {
        let state = session.state.read();
        state.user().map(|u| u.role == "admin").unwrap_or(false)
    };
    if !is_admin {
        return rsx! {
            div { class: "crumb", "Admin · Audit log" }
            h1 { "Audit log" }
            div { class: "empty",
                div { class: "icon", "⛔" }
                h3 { "Insufficient permissions" }
                p { "The audit log is available to admin accounts only." }
            }
        };
    }

    let gate = use_auth_gate();

    // Filter set — defaults to the last 24h (ui-brief §1). Mutating any
    // control writes this signal, which the fetch effect below subscribes
    // to, so a filter change re-queries from offset 0.
    let mut filters = use_signal(|| AuditFilters {
        from: Some(default_from_date()),
        ..AuditFilters::default()
    });

    // Accumulated result set + paging cursor.
    let mut rows: Signal<Vec<AuditEvent>> = use_signal(Vec::new);
    let mut total: Signal<i64> = use_signal(|| 0i64);
    let mut offset: Signal<i64> = use_signal(|| 0i64);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut selected: Signal<Option<AuditEvent>> = use_signal(|| None::<AuditEvent>);

    // Actor / target dropdowns are populated from the user list. A failure
    // here only degrades the dropdowns — the table still loads — so the
    // error is swallowed and the selects fall back to "All".
    let users = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                api::authorized_get_json::<Vec<AdminUser>>(&gate, &api::api_base(), "/api/users")
                    .await
                    .unwrap_or_default()
            }
        }
    });

    // Initial fetch + refetch-on-filter-change. The effect reads `filters`
    // (subscribing it) and writes only *other* signals — no self-loop.
    {
        let gate = gate.clone();
        use_effect(move || {
            let f = filters.read().clone();
            let gate = gate.clone();
            offset.set(0);
            loading.set(true);
            spawn(async move {
                let path = build_audit_path(&f, 0);
                match api::authorized_get_json::<Page<AuditEvent>>(&gate, &api::api_base(), &path)
                    .await
                {
                    Ok(page) => {
                        total.set(page.total);
                        rows.set(page.items);
                        error.set(None);
                    }
                    Err(e) => {
                        error.set(Some(e));
                        rows.set(Vec::new());
                        total.set(0);
                    }
                }
                loading.set(false);
            });
        });
    }

    // Scroll-to-load: fetch the next 50-row page and append. `peek()` reads
    // without subscribing — this is an action handler, not reactive code.
    let load_more = {
        let gate = gate.clone();
        move |_| {
            let gate = gate.clone();
            let f = filters.peek().clone();
            let next_offset = *offset.peek() + ROWS_PER_PAGE;
            offset.set(next_offset);
            loading.set(true);
            spawn(async move {
                let path = build_audit_path(&f, next_offset);
                match api::authorized_get_json::<Page<AuditEvent>>(&gate, &api::api_base(), &path)
                    .await
                {
                    Ok(page) => {
                        total.set(page.total);
                        rows.write().extend(page.items);
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)),
                }
                loading.set(false);
            });
        }
    };

    // Manual refresh — re-applies the current filter set. Touching the
    // signal (without changing it would not re-fire the effect, so we
    // re-write the same value to force a re-run).
    let refresh = move |_| {
        let f = filters.peek().clone();
        filters.set(f);
    };

    let user_list = users.read().clone().unwrap_or_default();
    let snapshot = rows.read().clone();
    let total_now = *total.read();
    let loading_now = *loading.read();
    let narrowed = filters.read().is_narrowed();
    let selected_id = selected.read().as_ref().map(|e| e.id);
    let has_more = (snapshot.len() as i64) < total_now;

    rsx! {
        div { class: "crumb", "Admin · Audit log" }
        h1 { "Audit log" }
        p { class: "info-hint",
            "Read-only record of admin activity. Newest first; the default view covers the last 24 hours."
        }

        if let Some(err) = error.read().as_ref() {
            Banner { variant: BannerVariant::Danger, title: "Could not load the audit log".to_string(),
                p { "{format_error(err)}" }
            }
        }

        div { class: "audit-layout",
            // ── Left rail: filters ──────────────────────────────────────
            aside { class: "audit-filters", "aria-label": "Audit log filters",
                div { class: "audit-filter",
                    label { r#for: "audit-f-kind", "Event kind" }
                    select {
                        id: "audit-f-kind",
                        class: "input",
                        value: filters.read().kind.clone().unwrap_or_default(),
                        onchange: move |e| {
                            let v = e.value();
                            filters.write().kind = if v.is_empty() { None } else { Some(v) };
                        },
                        option { value: "", "All kinds" }
                        for (wire , label) in AUDIT_KINDS {
                            option { value: "{wire}", "{label}" }
                        }
                    }
                }

                div { class: "audit-filter",
                    label { r#for: "audit-f-actor", "Actor" }
                    select {
                        id: "audit-f-actor",
                        class: "input",
                        value: filters.read().actor_user_id.map(|i| i.to_string()).unwrap_or_default(),
                        onchange: move |e| {
                            filters.write().actor_user_id = e.value().parse::<i64>().ok();
                        },
                        option { value: "", "Any actor" }
                        for u in user_list.iter() {
                            option { value: "{u.id}", "{u.username}" }
                        }
                    }
                }

                div { class: "audit-filter",
                    label { r#for: "audit-f-target", "Target user" }
                    select {
                        id: "audit-f-target",
                        class: "input",
                        value: filters.read().target_user_id.map(|i| i.to_string()).unwrap_or_default(),
                        onchange: move |e| {
                            filters.write().target_user_id = e.value().parse::<i64>().ok();
                        },
                        option { value: "", "Any target" }
                        for u in user_list.iter() {
                            option { value: "{u.id}", "{u.username}" }
                        }
                    }
                }

                div { class: "audit-filter",
                    label { r#for: "audit-f-outcome", "Outcome" }
                    select {
                        id: "audit-f-outcome",
                        class: "input",
                        value: filters.read().outcome.clone().unwrap_or_default(),
                        onchange: move |e| {
                            let v = e.value();
                            filters.write().outcome = if v.is_empty() { None } else { Some(v) };
                        },
                        option { value: "", "Any outcome" }
                        option { value: "success", "Success" }
                        option { value: "failure", "Failure" }
                    }
                }

                div { class: "audit-filter",
                    label { r#for: "audit-f-from", "From" }
                    input {
                        id: "audit-f-from",
                        class: "input",
                        r#type: "date",
                        value: filters.read().from.clone().unwrap_or_default(),
                        onchange: move |e| {
                            let v = e.value();
                            filters.write().from = if v.is_empty() { None } else { Some(v) };
                        },
                    }
                }

                div { class: "audit-filter",
                    label { r#for: "audit-f-to", "To" }
                    input {
                        id: "audit-f-to",
                        class: "input",
                        r#type: "date",
                        value: filters.read().to.clone().unwrap_or_default(),
                        onchange: move |e| {
                            let v = e.value();
                            filters.write().to = if v.is_empty() { None } else { Some(v) };
                        },
                    }
                }

                button {
                    r#type: "button",
                    class: "btn btn-ghost btn-block",
                    onclick: move |_| {
                        filters.set(AuditFilters {
                            from: Some(default_from_date()),
                            ..AuditFilters::default()
                        });
                    },
                    "Reset filters"
                }
            }

            // ── Centre: results table ───────────────────────────────────
            div { class: "audit-main",
                div { class: "audit-toolbar",
                    span { class: "audit-count",
                        if loading_now && snapshot.is_empty() {
                            "Loading…"
                        } else {
                            "{snapshot.len()} of {total_now} events"
                        }
                    }
                    div { class: "audit-toolbar-actions",
                        button {
                            r#type: "button",
                            class: "btn btn-secondary btn-sm",
                            onclick: refresh,
                            "Refresh"
                        }
                        // CSV export is out of v1.1 scope (ui-brief §8) —
                        // disabled hook point; v1.2 wires `on_export` here.
                        button {
                            r#type: "button",
                            class: "btn btn-secondary btn-sm",
                            disabled: true,
                            title: "CSV export — coming in v1.2",
                            "Export CSV"
                        }
                    }
                }

                if snapshot.is_empty() && !loading_now {
                    div { class: "empty",
                        div { class: "icon", "≡" }
                        if narrowed {
                            h3 { "No events matching this filter" }
                            p { "Try widening the date range or clearing a filter." }
                        } else {
                            h3 { "No admin activity recorded yet" }
                            p {
                                "Mutating an admin user from the Users tab — creating, "
                                "disabling, or deleting — will appear here."
                            }
                        }
                    }
                } else {
                    div { class: "audit-scroll",
                        table { class: "data-table audit-table", "aria-label": "Audit events",
                            thead {
                                tr {
                                    th { scope: "col", "Time" }
                                    th { scope: "col", "Actor" }
                                    th { scope: "col", "Kind" }
                                    th { scope: "col", "Target" }
                                    th { scope: "col", "Outcome" }
                                    th { scope: "col", "Payload" }
                                }
                            }
                            tbody {
                                for ev in snapshot.iter() {
                                    AuditRow {
                                        key: "{ev.id}",
                                        event: ev.clone(),
                                        is_selected: selected_id == Some(ev.id),
                                        on_select: {
                                            let ev = ev.clone();
                                            EventHandler::new(move |_| selected.set(Some(ev.clone())))
                                        },
                                        on_close: EventHandler::new(move |_| selected.set(None)),
                                    }
                                }
                            }
                        }
                        if has_more {
                            button {
                                r#type: "button",
                                class: "btn btn-secondary btn-block audit-load-more",
                                disabled: loading_now,
                                onclick: load_more,
                                if loading_now { "Loading more…" } else { "Load more" }
                            }
                        }
                    }
                }
            }

            // ── Right panel: row detail ─────────────────────────────────
            if let Some(ev) = selected.read().clone() {
                AuditDetailPanel {
                    event: ev,
                    on_close: EventHandler::new(move |_| selected.set(None)),
                }
            }
        }
    }
}

/// One audit table row. A focusable element so the table is keyboard
/// navigable (`ui-brief` §6): `Tab` reaches each row, `↑`/`↓` move row
/// focus, `Enter` opens the detail panel, `Esc` closes it.
#[derive(Props, Clone, PartialEq)]
struct AuditRowProps {
    event: AuditEvent,
    is_selected: bool,
    on_select: EventHandler<()>,
    on_close: EventHandler<()>,
}

#[component]
fn AuditRow(props: AuditRowProps) -> Element {
    let ev = &props.event;
    let on_select = props.on_select;
    let on_close = props.on_close;

    let row_class = {
        let mut c = String::from("audit-row");
        if ev.outcome == "failure" {
            c.push_str(" audit-row--failure");
        }
        if props.is_selected {
            c.push_str(" is-selected");
        }
        c
    };
    let preview = payload_preview(&ev.payload);

    rsx! {
        tr {
            class: "{row_class}",
            tabindex: "0",
            role: "button",
            "aria-label": "Audit event {ev.id}: {kind_label(&ev.kind)}",
            onclick: move |_| on_select.call(()),
            onkeydown: move |evt| {
                match evt.key() {
                    Key::Enter => {
                        evt.prevent_default();
                        on_select.call(());
                    }
                    Key::Escape => {
                        evt.prevent_default();
                        on_close.call(());
                    }
                    Key::ArrowDown => {
                        evt.prevent_default();
                        move_row_focus(true);
                    }
                    Key::ArrowUp => {
                        evt.prevent_default();
                        move_row_focus(false);
                    }
                    _ => {}
                }
            },
            td { class: "audit-time", "{fmt_ts(&ev.occurred_at)}" }
            td {
                ActorCell { user_id: ev.actor_user_id, username: ev.actor_username.clone() }
            }
            td {
                span { class: "audit-kind {kind_badge_class(&ev.kind)}", "{kind_label(&ev.kind)}" }
            }
            td {
                TargetCell {
                    target_kind: ev.target_kind.clone(),
                    target_id: ev.target_id,
                    target_label: ev.target_label.clone(),
                }
            }
            td {
                span { class: if ev.outcome == "failure" { "audit-outcome audit-outcome--failure" } else { "audit-outcome audit-outcome--success" },
                    "{ev.outcome}"
                }
            }
            td { class: "audit-payload-cell",
                if preview.is_empty() {
                    span { class: "muted", "—" }
                } else {
                    code { class: "audit-payload-preview", "{preview}" }
                }
            }
        }
    }
}

/// Actor cell. A deleted actor (`actorUserId: null`) renders the snapshot
/// username in italics so forensics can still read who acted (ui-brief §3.5).
#[derive(Props, Clone, PartialEq)]
struct ActorCellProps {
    user_id: Option<i64>,
    username: String,
}

#[component]
fn ActorCell(props: ActorCellProps) -> Element {
    if props.user_id.is_none() {
        rsx! {
            span { class: "audit-deleted", title: "This account was deleted after the event",
                "deleted user \"{props.username}\""
            }
        }
    } else {
        rsx! {
            span { class: "audit-actor", "{props.username}" }
        }
    }
}

/// Target cell. No target → `–`. Deleted target (`targetId: null` with a
/// `targetKind`) → italic snapshot label. Otherwise the snapshot label.
#[derive(Props, Clone, PartialEq)]
struct TargetCellProps {
    target_kind: Option<String>,
    target_id: Option<i64>,
    target_label: Option<String>,
}

#[component]
fn TargetCell(props: TargetCellProps) -> Element {
    let label = props
        .target_label
        .clone()
        .unwrap_or_else(|| "—".to_string());
    match (&props.target_kind, props.target_id) {
        (None, _) => rsx! {
            span { class: "muted", "–" }
        },
        (Some(_), None) => rsx! {
            span { class: "audit-deleted", title: "This target was deleted after the event",
                "deleted \"{label}\""
            }
        },
        (Some(_), Some(_)) => rsx! {
            span { class: "audit-target", "{label}" }
        },
    }
}

/// Right-hand detail panel for the clicked row. Renders the full redacted
/// payload, the request trace fields, and a copy-to-clipboard for the id.
#[derive(Props, Clone, PartialEq)]
struct AuditDetailPanelProps {
    event: AuditEvent,
    on_close: EventHandler<()>,
}

#[component]
fn AuditDetailPanel(props: AuditDetailPanelProps) -> Element {
    let ev = &props.event;
    let on_close = props.on_close;
    let mut copied = use_signal(|| false);

    // Defence-in-depth: the writer already redacts, but never render a
    // credential field even if a future writer regression let one through.
    let payload_text = match &ev.payload {
        Some(p) => serde_json::to_string_pretty(&redact_payload(p))
            .unwrap_or_else(|_| "<unrenderable payload>".to_string()),
        None => "null".to_string(),
    };
    let row_id = ev.id;

    rsx! {
        aside {
            class: "audit-detail",
            "aria-label": "Audit event detail",
            tabindex: "-1",
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    evt.prevent_default();
                    on_close.call(());
                }
            },
            div { class: "audit-detail-header",
                h2 { "Event {row_id}" }
                button {
                    r#type: "button",
                    class: "btn btn-ghost btn-sm",
                    "aria-label": "Close detail panel",
                    onclick: move |_| on_close.call(()),
                    "✕"
                }
            }
            div { class: "audit-detail-body",
                dl { class: "audit-detail-fields",
                    DetailField { label: "Kind", value: kind_label(&ev.kind).to_string() }
                    DetailField { label: "Outcome", value: ev.outcome.clone() }
                    DetailField { label: "Occurred", value: fmt_ts(&ev.occurred_at) }
                    DetailField { label: "Inserted", value: fmt_ts(&ev.inserted_at) }
                    DetailField { label: "Actor", value: ev.actor_username.clone() }
                    DetailField {
                        label: "Target",
                        value: ev.target_label.clone().unwrap_or_else(|| "–".to_string()),
                    }
                    if let Some(msg) = ev.error_msg.as_ref() {
                        DetailField { label: "Error", value: msg.clone() }
                    }
                    // `requestId` is not part of the v1.1 AuditEvent wire
                    // type (`docs/admin/http-api.md` §2.5); the request IP
                    // and user-agent are the available trace handles.
                    DetailField {
                        label: "Request IP",
                        value: ev.request_ip.clone().unwrap_or_else(|| "–".to_string()),
                    }
                    DetailField {
                        label: "User agent",
                        value: ev.request_user_agent.clone().unwrap_or_else(|| "–".to_string()),
                    }
                }

                h3 { class: "audit-detail-subhead", "Payload" }
                pre { class: "audit-payload-full", "{payload_text}" }
                p { class: "info-hint",
                    "Credential-bearing fields are redacted at write time and again on render."
                }
            }
            div { class: "audit-detail-footer",
                button {
                    r#type: "button",
                    class: "btn btn-secondary btn-sm",
                    onclick: move |_| {
                        copy_to_clipboard(&row_id.to_string());
                        copied.set(true);
                    },
                    if *copied.read() { "Copied id ✓" } else { "Copy row id" }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DetailFieldProps {
    label: String,
    value: String,
}

#[component]
fn DetailField(props: DetailFieldProps) -> Element {
    rsx! {
        div { class: "audit-detail-field",
            dt { "{props.label}" }
            dd { "{props.value}" }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

/// `YYYY-MM-DD` for 24h ago — the default lower bound of the date filter.
fn default_from_date() -> String {
    (Utc::now() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string()
}

/// Human label for an audit kind discriminant. Unknown kinds (e.g. a v1.2
/// addition this build predates) fall back to the raw wire string so the
/// row still renders rather than going blank.
fn kind_label(kind: &str) -> &str {
    AUDIT_KINDS
        .iter()
        .find(|(wire, _)| *wire == kind)
        .map(|(_, label)| *label)
        .unwrap_or(kind)
}

/// CSS modifier class grouping the ten kinds into a small badge palette.
/// Grouped by semantic weight: creation, removal, security, neutral.
fn kind_badge_class(kind: &str) -> &str {
    match kind {
        "userCreated" | "userEnabled" | "setupCompleted" => "audit-kind--create",
        "userDeleted" | "userDisabled" => "audit-kind--remove",
        "userPasswordReset" | "selfPasswordChanged" | "sessionRevoked" => "audit-kind--security",
        "userRoleChanged" => "audit-kind--role",
        _ => "audit-kind--neutral",
    }
}

/// Compact, truncated one-line preview of a payload object for the table
/// cell. The full payload lives in the detail panel.
fn payload_preview(payload: &Option<Value>) -> String {
    let Some(value) = payload else {
        return String::new();
    };
    if value.is_null() {
        return String::new();
    }
    let compact = serde_json::to_string(&redact_payload(value)).unwrap_or_default();
    truncate_chars(&compact, 64)
}

/// Truncate to at most `max` characters, appending `…` when clipped. Works
/// on char boundaries so multi-byte payloads never panic.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Format a UTC timestamp for display.
fn fmt_ts(ts: &chrono::DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Recursively replace credential-bearing fields with `[REDACTED]`. The
/// audit writer already redacts at persist time (`docs/admin/audit-shape.md`
/// §2.3); this is the client-side belt-and-braces pass.
fn redact_payload(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), Value::String("[REDACTED]".to_string()));
                } else {
                    out.insert(k.clone(), redact_payload(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_payload).collect()),
        other => other.clone(),
    }
}

/// `true` when a JSON key names a credential-bearing field. Matches on the
/// key's alphanumeric-only lowercase form so `apiKey`, `api_key`, and
/// `API-KEY` all collapse to the same needle.
fn is_sensitive_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    const NEEDLES: [&str; 11] = [
        "password",
        "passwordhash",
        "apikey",
        "sshkey",
        "sshpassword",
        "privatekey",
        "secret",
        "accesstoken",
        "refreshtoken",
        "authorization",
        "bearertoken",
    ];
    NEEDLES.iter().any(|n| norm.contains(n))
}

/// Build the `GET /api/audit` request path for the given filters and offset.
/// Date inputs are day-granular; `from` widens to the start of the day and
/// `to` to the end, so a same-day `from`/`to` selects the whole day.
fn build_audit_path(f: &AuditFilters, offset: i64) -> String {
    let mut path = String::from("/api/audit?");
    let mut first = true;
    let mut push = |k: &str, v: String| {
        if !first {
            path.push('&');
        }
        path.push_str(k);
        path.push('=');
        path.push_str(&urlencoding::encode(&v));
        first = false;
    };
    if let Some(kind) = f.kind.as_deref() {
        push("kind", kind.to_string());
    }
    if let Some(actor) = f.actor_user_id {
        push("actorUserId", actor.to_string());
    }
    if let Some(target) = f.target_user_id {
        // `targetId` must be paired with `targetKind` (http-api §3.4).
        push("targetKind", "user".to_string());
        push("targetId", target.to_string());
    }
    if let Some(outcome) = f.outcome.as_deref() {
        push("outcome", outcome.to_string());
    }
    if let Some(from) = f.from.as_deref().filter(|s| !s.is_empty()) {
        push("from", format!("{from}T00:00:00Z"));
    }
    if let Some(to) = f.to.as_deref().filter(|s| !s.is_empty()) {
        push("to", format!("{to}T23:59:59Z"));
    }
    push("limit", ROWS_PER_PAGE.to_string());
    push("offset", offset.max(0).to_string());
    // `push` borrows `path` mutably; its last use above releases the borrow
    // under NLL, so `path` is free to move out here.
    path
}

/// Move keyboard focus to the next/previous table row. Reads the currently
/// focused element from the document and walks its element siblings, so it
/// needs no per-row node handles. No-op outside the browser.
fn move_row_focus(forward: bool) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let Some(active) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
        else {
            return;
        };
        let sibling = if forward {
            active.next_element_sibling()
        } else {
            active.previous_element_sibling()
        };
        if let Some(el) = sibling.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
            let _ = el.focus();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = forward;
    }
}

/// Best-effort copy of `text` to the system clipboard. No-op off the
/// browser (SSR / unit tests). Mirrors `ui::pages::widgets::copy_to_clipboard`.
fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

/// Flatten an [`ApiError`] to one operator-facing line.
fn format_error(err: &ApiError) -> String {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut s = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(": ");
                s.push_str(d);
            }
            if let Some(c) = code {
                s.push_str(&format!(" (code {c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::SessionAnonymous => "Loading…".into(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Audit log unavailable in this view.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_payload_masks_credential_keys() {
        let input = json!({
            "role": "admin",
            "password": "Hunter2!",
            "apiKey": "abc123",
            "ssh_key": "-----BEGIN-----",
            "nested": { "sshPassword": "x", "keepme": 7 }
        });
        let out = redact_payload(&input);
        assert_eq!(out["role"], json!("admin"));
        assert_eq!(out["password"], json!("[REDACTED]"));
        assert_eq!(out["apiKey"], json!("[REDACTED]"));
        assert_eq!(out["ssh_key"], json!("[REDACTED]"));
        assert_eq!(out["nested"]["sshPassword"], json!("[REDACTED]"));
        assert_eq!(out["nested"]["keepme"], json!(7));
    }

    #[test]
    fn redact_payload_walks_arrays() {
        let input = json!([{ "secret": "s" }, { "ok": 1 }]);
        let out = redact_payload(&input);
        assert_eq!(out[0]["secret"], json!("[REDACTED]"));
        assert_eq!(out[1]["ok"], json!(1));
    }

    #[test]
    fn is_sensitive_key_is_case_and_separator_insensitive() {
        assert!(is_sensitive_key("password"));
        assert!(is_sensitive_key("API_KEY"));
        assert!(is_sensitive_key("api-key"));
        assert!(is_sensitive_key("sshKey"));
        assert!(is_sensitive_key("refreshToken"));
        assert!(!is_sensitive_key("role"));
        assert!(!is_sensitive_key("family"));
        assert!(!is_sensitive_key("username"));
    }

    #[test]
    fn build_audit_path_includes_only_set_filters() {
        let f = AuditFilters {
            kind: Some("userDisabled".into()),
            actor_user_id: Some(1),
            target_user_id: Some(2),
            outcome: Some("success".into()),
            from: Some("2026-05-14".into()),
            to: None,
        };
        let path = build_audit_path(&f, 50);
        assert!(path.contains("kind=userDisabled"));
        assert!(path.contains("actorUserId=1"));
        // targetId is always paired with targetKind so the index is usable.
        assert!(path.contains("targetKind=user"));
        assert!(path.contains("targetId=2"));
        assert!(path.contains("outcome=success"));
        assert!(path.contains("from=2026-05-14T00%3A00%3A00Z"));
        assert!(!path.contains("&to="));
        assert!(path.contains("limit=50"));
        assert!(path.contains("offset=50"));
    }

    #[test]
    fn build_audit_path_empty_filters_still_paginates() {
        let path = build_audit_path(&AuditFilters::default(), 0);
        assert!(path.contains("limit=50"));
        assert!(path.contains("offset=0"));
        assert!(!path.contains("kind="));
    }

    #[test]
    fn build_audit_path_to_widens_to_end_of_day() {
        let f = AuditFilters {
            to: Some("2026-05-15".into()),
            ..AuditFilters::default()
        };
        let path = build_audit_path(&f, 0);
        assert!(path.contains("to=2026-05-15T23%3A59%3A59Z"));
    }

    #[test]
    fn kind_label_falls_back_to_raw_for_unknown_kind() {
        assert_eq!(kind_label("userDisabled"), "User disabled");
        assert_eq!(kind_label("someFutureKind"), "someFutureKind");
    }

    #[test]
    fn kind_badge_class_groups_the_ten_kinds() {
        assert_eq!(kind_badge_class("userCreated"), "audit-kind--create");
        assert_eq!(kind_badge_class("userDeleted"), "audit-kind--remove");
        assert_eq!(
            kind_badge_class("userPasswordReset"),
            "audit-kind--security"
        );
        assert_eq!(kind_badge_class("userRoleChanged"), "audit-kind--role");
        assert_eq!(kind_badge_class("userPatched"), "audit-kind--neutral");
    }

    #[test]
    fn payload_preview_truncates_and_handles_null() {
        assert_eq!(payload_preview(&None), "");
        assert_eq!(payload_preview(&Some(Value::Null)), "");
        let long = json!({ "fields": ["displayName", "role", "enabled", "extra", "more"] });
        let preview = payload_preview(&Some(long));
        assert!(preview.chars().count() <= 65, "preview: {preview}");
    }

    #[test]
    fn payload_preview_redacts_before_preview() {
        let p = json!({ "password": "Hunter2!" });
        let preview = payload_preview(&Some(p));
        assert!(preview.contains("[REDACTED]"));
        assert!(!preview.contains("Hunter2"));
    }

    #[test]
    fn audit_filters_is_narrowed_ignores_default_from() {
        let default_view = AuditFilters {
            from: Some(default_from_date()),
            ..AuditFilters::default()
        };
        assert!(!default_view.is_narrowed());
        let narrowed = AuditFilters {
            kind: Some("userCreated".into()),
            ..default_view
        };
        assert!(narrowed.is_narrowed());
    }
}
