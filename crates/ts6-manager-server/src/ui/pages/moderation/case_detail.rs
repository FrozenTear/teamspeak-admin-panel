//! `/moderation/cases/{id}` — case detail, action timeline, action
//! composer. PURA-287.
//!
//! Three regions:
//!
//! - **Header** — subject, status, origin, the opening reason, and the
//!   lifecycle timestamps. A link jumps to the subject's full history.
//! - **Timeline** — the append-only `moderation_case_action` log, oldest
//!   first, each row carrying the actor, reason, and (for bans) the TS6
//!   ban id.
//! - **Composer** — the action form. The action kinds offered are
//!   filtered by the caller's role-derived grants ([`perm::role_holds`]);
//!   `resolve` / `reopen` are separate lifecycle controls keyed off the
//!   current status.
//!
//! ## IP ban
//!
//! The moderation action endpoint (`POST …/actions`) bans the case
//! subject by **UID only** — that is the durable key a case is built on
//! (`9.0-spike` recommendation 6). An IP ban is a different, blunter
//! instrument: it is applied through the control ban route
//! (`POST /api/servers/{id}/vs/{sid}/bans`) with a raw IP, and then a
//! `note` action is appended to the case so the timeline still records
//! that it happened. The composer surfaces it behind an explicit
//! collateral-damage warning and gates it on `moderation.action.ban_ip`
//! (admin-only by role default).

use dioxus::prelude::*;
use ts6_manager_shared::control::{BanCreateRequest, BanCreated};
use ts6_manager_shared::moderation::{
    AppendActionRequest, CaseDetail, ModerationCaseAction, ReopenCaseRequest, ResolveCaseRequest,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonType, ButtonVariant};
use crate::ui::routes::Route;

use super::perm;
use super::{
    AccessDenied, action_kind_icon, action_kind_label, case_status_class, fmt_datetime,
    format_error, origin_label, relative_when,
};

/// One selectable composer action: `(kind, label, catalog permission)`.
/// `ban_ip` is in the list but routed specially — see the module docs.
const COMPOSER_ACTIONS: &[(&str, &str, &str)] = &[
    ("note", "Add note", "moderation.note.write"),
    ("kick", "Kick", "moderation.action.kick"),
    ("mute", "Mute", "moderation.action.mute"),
    ("unmute", "Unmute", "moderation.action.mute"),
    ("ban", "Ban (by UID)", "moderation.action.ban"),
    ("ban_ip", "Ban by IP address", "moderation.action.ban_ip"),
];

#[component]
pub fn ModerationCasePage(case_id: i64) -> Element {
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
                crumb: "Moderation · Case".to_string(),
                heading: "Case".to_string(),
                detail: "Moderation cases are available to moderator and admin accounts only.".to_string(),
            }
        };
    }

    let gate = use_auth_gate();

    // Bump to force a refetch after any successful mutation.
    let reload: Signal<u64> = use_signal(|| 0u64);
    let detail = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move {
                let path = format!("/api/moderation/cases/{case_id}");
                api::authorized_get_json::<CaseDetail>(&gate, &api::api_base(), &path).await
            }
        }
    });

    let snapshot = detail.read().clone();

    rsx! {
        div { class: "crumb",
            Link { to: Route::ModerationQueuePage {}, "Moderation" }
            " · Case #{case_id}"
        }

        match snapshot {
            None => rsx! {
                h1 { "Case #{case_id}" }
                p { class: "info-hint", "Loading case…" }
            },
            Some(Err(ApiError::Client { status: 404, .. })) => rsx! {
                h1 { "Case #{case_id}" }
                div { class: "empty",
                    div { class: "icon", "⊙" }
                    h3 { "Case not found" }
                    p { "This case does not exist or has been removed." }
                }
            },
            Some(Err(e)) => rsx! {
                h1 { "Case #{case_id}" }
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load case".to_string(),
                    "{format_error(&e)}"
                }
            },
            Some(Ok(detail)) => rsx! {
                CaseBody { case_id, detail, role: role.clone(), reload }
            },
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CaseBodyProps {
    case_id: i64,
    detail: CaseDetail,
    role: String,
    reload: Signal<u64>,
}

#[component]
fn CaseBody(props: CaseBodyProps) -> Element {
    let case = props.detail.case.clone();
    let timeline = props.detail.timeline.clone();
    let is_resolved = case.status == "resolved";
    let subject_uid = case.subject_uid.clone();

    rsx! {
        div { class: "mod-case-head",
            h1 { "{case.subject_nickname_snapshot}" }
            span { class: case_status_class(&case.status), "{case.status}" }
        }
        dl { class: "mod-kv",
            div { dt { "Subject UID" } dd { class: "mono", "{case.subject_uid}" } }
            div { dt { "Origin" } dd { "{origin_label(&case.origin)}" } }
            div { dt { "Opened" } dd { "{fmt_datetime(case.opened_at)}" } }
            div { dt { "Updated" } dd { "{fmt_datetime(case.updated_at)}" } }
            if let Some(resolved) = case.resolved_at {
                div { dt { "Resolved" } dd { "{fmt_datetime(resolved)}" } }
            }
            div { dt { "Reason" } dd { "{case.reason}" } }
            if let Some(note) = case.resolution_note.as_ref().filter(|n| !n.is_empty()) {
                div { dt { "Resolution" } dd { "{note}" } }
            }
        }
        p {
            Link {
                to: Route::SubjectHistoryPage { uid: subject_uid.clone() },
                "View full history for this subject →"
            }
        }

        section { class: "stack-md mod-panel",
            h2 { "Action timeline" }
            Timeline { rows: timeline }
        }

        if is_resolved {
            ReopenPanel {
                case_id: props.case_id,
                role: props.role.clone(),
                reload: props.reload,
            }
        } else {
            ComposerPanel {
                case_id: props.case_id,
                server_config_id: case.server_config_id,
                virtual_server_id: case.virtual_server_id,
                role: props.role.clone(),
                reload: props.reload,
            }
            ResolvePanel {
                case_id: props.case_id,
                role: props.role.clone(),
                reload: props.reload,
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct TimelineProps {
    rows: Vec<ModerationCaseAction>,
}

#[component]
fn Timeline(props: TimelineProps) -> Element {
    if props.rows.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✎" }
                h3 { "No actions yet" }
                p { "Kicks, bans, mutes, and notes will appear here as they are recorded." }
            }
        };
    }
    rsx! {
        ol { class: "mod-timeline",
            for a in props.rows.iter() {
                li { key: "{a.id}", class: "mod-timeline-row",
                    span { class: "mod-timeline-icon", aria_hidden: "true",
                        "{action_kind_icon(&a.action_kind)}"
                    }
                    div { class: "mod-timeline-body",
                        div { class: "mod-timeline-head",
                            strong { "{action_kind_label(&a.action_kind)}" }
                            span { class: "muted", " by {a.actor_username_snapshot}" }
                            span { class: "mod-timeline-when", "{relative_when(a.created_at)}" }
                        }
                        p { class: "mod-timeline-reason", "{a.reason}" }
                        if let Some(ts_ref) = a.ts_ref.as_ref().filter(|r| !r.is_empty()) {
                            p { class: "info-hint", "TeamSpeak ban #{ts_ref}" }
                        }
                    }
                }
            }
        }
    }
}

// ── action composer ─────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ComposerProps {
    case_id: i64,
    server_config_id: i64,
    virtual_server_id: i64,
    role: String,
    reload: Signal<u64>,
}

#[component]
fn ComposerPanel(props: ComposerProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    // Only the actions this role can perform are offered.
    let available: Vec<(&str, &str)> = COMPOSER_ACTIONS
        .iter()
        .filter(|(_, _, p)| perm::role_holds(&props.role, p))
        .map(|(kind, label, _)| (*kind, *label))
        .collect();

    if available.is_empty() {
        return rsx! {
            section { class: "stack-md mod-panel",
                h2 { "Record an action" }
                p { class: "info-hint",
                    "Your role does not hold any moderation action permissions for this case."
                }
            }
        };
    }

    let mut kind = use_signal(|| available[0].0.to_string());
    let mut reason = use_signal(String::new);
    let mut clid = use_signal(String::new);
    let mut duration = use_signal(String::new);
    let mut ip = use_signal(String::new);
    let mut busy = use_signal(|| false);

    let kind_now = kind.read().clone();
    let needs_clid = matches!(kind_now.as_str(), "kick" | "mute" | "unmute");
    let is_ban = kind_now == "ban";
    let is_ip_ban = kind_now == "ban_ip";

    let case_id = props.case_id;
    let server_config_id = props.server_config_id;
    let sid = props.virtual_server_id;
    let mut reload = props.reload;

    let on_submit = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() {
                return;
            }
            let kind_v = kind.peek().clone();
            let reason_v = reason.peek().trim().to_string();
            if reason_v.is_empty() {
                toaster.push(
                    ToastVariant::Warning,
                    "Reason is required",
                    Some("Every timeline action records why it was taken.".into()),
                );
                return;
            }

            // kick / mute / unmute act on a live connection → need a clid.
            let clid_v = if matches!(kind_v.as_str(), "kick" | "mute" | "unmute") {
                match clid.peek().trim().parse::<i64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        toaster.push(
                            ToastVariant::Warning,
                            "A client id is required",
                            Some("Kick, mute, and unmute act on a connected client — enter its clid.".into()),
                        );
                        return;
                    }
                }
            } else {
                None
            };

            // Ban duration: blank / 0 → permanent.
            let duration_v = duration
                .peek()
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0);

            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);

            if kind_v == "ban_ip" {
                // IP ban routes through the control ban surface, then
                // backfills a `note` so the case timeline still records it.
                let ip_v = ip.peek().trim().to_string();
                if ip_v.is_empty() {
                    toaster.push(
                        ToastVariant::Warning,
                        "An IP address is required",
                        Some("Enter the IP address to ban.".into()),
                    );
                    busy.set(false);
                    return;
                }
                spawn(async move {
                    let ban_req = BanCreateRequest {
                        ip: Some(ip_v.clone()),
                        uid: None,
                        my_ts_id: None,
                        name: None,
                        reason: Some(reason_v.clone()),
                        duration: duration_v,
                    };
                    let ban_path = format!("/api/servers/{server_config_id}/vs/{sid}/bans");
                    match api::authorized_post_json::<_, BanCreated>(
                        &gate,
                        &api::api_base(),
                        &ban_path,
                        Some(&ban_req),
                    )
                    .await
                    {
                        Ok(BanCreated { banid }) => {
                            // Backfill the timeline. A failure here is
                            // non-fatal — the ban already landed — so it
                            // only warns rather than rolling back.
                            let note = AppendActionRequest {
                                action_kind: "note".into(),
                                reason: format!(
                                    "IP ban applied to {ip_v} (TeamSpeak ban #{banid}): {reason_v}"
                                ),
                                clid: None,
                                ban_duration_secs: None,
                            };
                            let note_path =
                                format!("/api/moderation/cases/{case_id}/actions");
                            let backfilled = api::authorized_post_json::<_, ModerationCaseAction>(
                                &gate,
                                &api::api_base(),
                                &note_path,
                                Some(&note),
                            )
                            .await;
                            busy.set(false);
                            if backfilled.is_err() {
                                toaster.push(
                                    ToastVariant::Warning,
                                    format!("IP ban #{banid} applied"),
                                    Some("The ban landed but the case timeline note could not be written.".into()),
                                );
                            } else {
                                toaster.push(
                                    ToastVariant::Success,
                                    format!("IP ban #{banid} applied"),
                                    None,
                                );
                            }
                            reason.set(String::new());
                            ip.set(String::new());
                            duration.set(String::new());
                            reload.with_mut(|n| *n += 1);
                        }
                        Err(e) => {
                            busy.set(false);
                            toaster.push(
                                ToastVariant::Danger,
                                "Could not apply IP ban",
                                Some(format_error(&e)),
                            );
                        }
                    }
                });
                return;
            }

            // note / kick / mute / unmute / ban → the moderation action
            // endpoint.
            let req = AppendActionRequest {
                action_kind: kind_v.clone(),
                reason: reason_v,
                clid: clid_v,
                ban_duration_secs: if kind_v == "ban" { duration_v } else { None },
            };
            spawn(async move {
                let path = format!("/api/moderation/cases/{case_id}/actions");
                let res = api::authorized_post_json::<_, ModerationCaseAction>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Action recorded", None);
                        reason.set(String::new());
                        clid.set(String::new());
                        duration.set(String::new());
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not record action",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        section { class: "stack-md mod-panel",
            h2 { "Record an action" }
            form {
                class: "ban-create",
                "aria-label": "Record a case action",
                onsubmit: move |evt| { evt.prevent_default(); on_submit.clone()(()); },

                div { class: "form-row",
                    label { r#for: "action-kind", "Action" }
                    select {
                        id: "action-kind",
                        class: "input",
                        value: "{kind_now}",
                        onchange: move |e| kind.set(e.value()),
                        for (k , label) in available.iter() {
                            option { key: "{k}", value: "{k}", "{label}" }
                        }
                    }
                }

                if needs_clid {
                    div { class: "form-row",
                        label { r#for: "action-clid", "Client id (clid)" }
                        input {
                            id: "action-clid",
                            class: "input",
                            inputmode: "numeric",
                            placeholder: "Connected client id",
                            value: "{clid.read()}",
                            oninput: move |e| clid.set(e.value()),
                        }
                    }
                }

                if is_ban || is_ip_ban {
                    div { class: "form-row",
                        label { r#for: "action-duration", "Duration (seconds, 0 / blank = permanent)" }
                        input {
                            id: "action-duration",
                            class: "input",
                            inputmode: "numeric",
                            placeholder: "0",
                            value: "{duration.read()}",
                            oninput: move |e| duration.set(e.value()),
                        }
                    }
                }

                if is_ip_ban {
                    Banner {
                        variant: BannerVariant::Danger,
                        title: "IP bans cause collateral damage".to_string(),
                        "An IP ban blocks every client connecting from that address — shared households, "
                        "NAT gateways, mobile carriers, and VPN exit nodes can all put unrelated users "
                        "behind one IP. Prefer a UID ban unless you specifically intend to block the "
                        "address itself. This action is recorded against the case and the audit log."
                    }
                    div { class: "form-row",
                        label { r#for: "action-ip", "IP address to ban" }
                        input {
                            id: "action-ip",
                            class: "input",
                            placeholder: "e.g. 203.0.113.4",
                            value: "{ip.read()}",
                            oninput: move |e| ip.set(e.value()),
                        }
                    }
                }

                div { class: "form-row",
                    label { r#for: "action-reason", "Reason" }
                    textarea {
                        id: "action-reason",
                        class: "input",
                        rows: "2",
                        placeholder: "Recorded on the timeline and the audit log",
                        value: "{reason.read()}",
                        oninput: move |e| reason.set(e.value()),
                    }
                }

                Button {
                    variant: if is_ip_ban { ButtonVariant::Danger } else { ButtonVariant::Primary },
                    kind: ButtonType::Submit,
                    loading: *busy.read(),
                    if is_ip_ban { "Apply IP ban" } else { "Record action" }
                }
            }
        }
    }
}

// ── resolve / reopen lifecycle panels ───────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct LifecycleProps {
    case_id: i64,
    role: String,
    reload: Signal<u64>,
}

#[component]
fn ResolvePanel(props: LifecycleProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    if !perm::role_holds(&props.role, "moderation.case.manage") {
        return rsx! { "" };
    }

    let mut note = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let case_id = props.case_id;
    let mut reload = props.reload;

    let on_resolve = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() {
                return;
            }
            let note_v = note.peek().trim().to_string();
            if note_v.is_empty() {
                toaster.push(
                    ToastVariant::Warning,
                    "A resolution note is required",
                    Some("Record how the case was resolved before closing it.".into()),
                );
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let req = ResolveCaseRequest {
                    resolution_note: note_v,
                };
                let path = format!("/api/moderation/cases/{case_id}/resolve");
                let res = api::authorized_post_json::<_, serde_json::Value>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Case resolved", None);
                        note.set(String::new());
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not resolve case",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        section { class: "stack-md mod-panel",
            h2 { "Resolve case" }
            form {
                class: "ban-create",
                "aria-label": "Resolve this case",
                onsubmit: move |evt| { evt.prevent_default(); on_resolve.clone()(()); },
                div { class: "form-row",
                    label { r#for: "resolve-note", "Resolution note" }
                    textarea {
                        id: "resolve-note",
                        class: "input",
                        rows: "2",
                        placeholder: "How was this resolved?",
                        value: "{note.read()}",
                        oninput: move |e| note.set(e.value()),
                    }
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Submit,
                    loading: *busy.read(),
                    "Mark resolved"
                }
            }
        }
    }
}

#[component]
fn ReopenPanel(props: LifecycleProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    if !perm::role_holds(&props.role, "moderation.case.manage") {
        return rsx! {
            section { class: "stack-md mod-panel",
                p { class: "info-hint",
                    "This case is resolved. Reopening it requires the case-manage permission."
                }
            }
        };
    }

    let mut reason = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let case_id = props.case_id;
    let mut reload = props.reload;

    let on_reopen = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() {
                return;
            }
            let reason_v = reason.peek().trim().to_string();
            if reason_v.is_empty() {
                toaster.push(
                    ToastVariant::Warning,
                    "A reason is required",
                    Some("Record why the case is being reopened.".into()),
                );
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let req = ReopenCaseRequest { reason: reason_v };
                let path = format!("/api/moderation/cases/{case_id}/reopen");
                let res = api::authorized_post_json::<_, serde_json::Value>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Case reopened", None);
                        reason.set(String::new());
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not reopen case",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        section { class: "stack-md mod-panel",
            h2 { "Reopen case" }
            p { class: "info-hint",
                "This case is resolved. Reopen it to record further actions."
            }
            form {
                class: "ban-create",
                "aria-label": "Reopen this case",
                onsubmit: move |evt| { evt.prevent_default(); on_reopen.clone()(()); },
                div { class: "form-row",
                    label { r#for: "reopen-reason", "Reason" }
                    textarea {
                        id: "reopen-reason",
                        class: "input",
                        rows: "2",
                        placeholder: "Why is this case being reopened?",
                        value: "{reason.read()}",
                        oninput: move |e| reason.set(e.value()),
                    }
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Submit,
                    loading: *busy.read(),
                    "Reopen case"
                }
            }
        }
    }
}
