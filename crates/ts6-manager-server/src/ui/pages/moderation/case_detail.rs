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
//! subject by **UID** for a `ban` kind — the durable key a case is built
//! on (`9.0-spike` recommendation 6). An IP ban is a different, blunter
//! instrument: it goes through the same endpoint as a `ban_ip` kind with
//! a raw `ip` (PURA-290), which dispatches `banadd?ip=`, checks
//! `moderation.action.ban_ip` server-side, and writes a real `ban_ip`
//! timeline row. The composer surfaces it behind an explicit
//! collateral-damage warning and gates it on the same permission
//! (admin-only by role default).

use dioxus::prelude::*;
use ts6_manager_shared::moderation::{
    AppendActionRequest, CaseDetail, DecideAppealRequest, ModerationAppeal, ModerationCaseAction,
    ReopenCaseRequest, ResolveCaseRequest,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};
use crate::ui::routes::Route;

use super::perm;
use super::{
    AccessDenied, action_kind_icon, action_kind_label, case_status_class, fmt_datetime,
    format_error, origin_label, relative_when,
};

/// One selectable composer action: `(kind, label, catalog permission)`.
/// Every kind — including `ban_ip` — posts to the moderation action
/// endpoint; see the module docs.
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
    let appeals = props.detail.appeals.clone();
    let is_resolved = case.status == "resolved";
    let is_appealed = case.status == "appealed";
    let subject_uid = case.subject_uid.clone();

    // PURA-303 — revert is an automod-case affordance, gated per kind on
    // the same catalog permissions the composer uses.
    let is_automod = case.origin == "automod";
    let can_revert_mute = perm::role_holds(&props.role, "moderation.action.mute");
    let can_revert_ban = perm::role_holds(&props.role, "moderation.action.ban");

    let gate = use_auth_gate();
    let toaster = use_toaster();
    let revert_busy = use_signal(|| false);
    let on_revert = {
        let gate = gate.clone();
        let mut reload = props.reload;
        let mut revert_busy = revert_busy;
        let case_id = props.case_id;
        move |action_id: i64| {
            if *revert_busy.peek() {
                return;
            }
            let gate = gate.clone();
            revert_busy.set(true);
            spawn(async move {
                let path = format!("/api/moderation/cases/{case_id}/actions/{action_id}/revert");
                let res = api::authorized_post_json::<(), ModerationCaseAction>(
                    &gate,
                    &api::api_base(),
                    &path,
                    None,
                )
                .await;
                revert_busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Action reverted", None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not revert action",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

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
            Timeline {
                rows: timeline,
                is_automod,
                can_revert_mute,
                can_revert_ban,
                revert_busy: *revert_busy.read(),
                on_revert: EventHandler::new(on_revert),
            }
        }

        if is_resolved {
            ReopenPanel {
                case_id: props.case_id,
                role: props.role.clone(),
                reload: props.reload,
            }
        } else if is_appealed {
            AppealPanel {
                case_id: props.case_id,
                appeals,
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
                is_automod,
                role: props.role.clone(),
                reload: props.reload,
            }
        }
    }
}

// ── appeal decision panel ───────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct AppealPanelProps {
    case_id: i64,
    /// Appeals lodged against this case (newest-first). The panel acts on
    /// the single `pending` one.
    appeals: Vec<ModerationAppeal>,
    role: String,
    reload: Signal<u64>,
}

/// The operator's uphold / overturn surface for a case in `appealed`
/// status. The appellant's raw statement is quarantined off the case
/// timeline (plan §4.7) and surfaced only here — rendered through a
/// Dioxus text node, so it is escaped (plan §6 hook 5).
#[component]
fn AppealPanel(props: AppealPanelProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let Some(appeal) = props
        .appeals
        .iter()
        .find(|a| a.status == "pending")
        .cloned()
    else {
        return rsx! {
            section { class: "stack-md mod-panel",
                h2 { "Appeal" }
                p { class: "info-hint",
                    "This case is marked appealed but carries no pending appeal. Reload, or resolve it from the timeline."
                }
            }
        };
    };

    let can_manage = perm::role_holds(&props.role, "moderation.case.manage");

    let mut note = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let case_id = props.case_id;
    let mut reload = props.reload;

    // One `EventHandler` (Copy) drives both buttons; the argument is the
    // trailing path segment — `"uphold"` or `"overturn"`.
    let decide = EventHandler::new(move |decision: &'static str| {
        if *busy.peek() {
            return;
        }
        let note_v = note.peek().trim().to_string();
        let req = DecideAppealRequest {
            decision_note: if note_v.is_empty() {
                None
            } else {
                Some(note_v)
            },
        };
        let gate = gate.clone();
        busy.set(true);
        spawn(async move {
            let path = format!("/api/moderation/cases/{case_id}/appeal/{decision}");
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
                    let label = if decision == "uphold" {
                        "Appeal upheld — case resolved"
                    } else {
                        "Appeal overturned — case resolved"
                    };
                    toaster.push(ToastVariant::Success, label, None);
                    note.set(String::new());
                    reload.with_mut(|n| *n += 1);
                }
                Err(e) => toaster.push(
                    ToastVariant::Danger,
                    "Could not record appeal decision",
                    Some(format_error(&e)),
                ),
            }
        });
    });

    rsx! {
        section { class: "stack-md mod-panel",
            h2 { "Appeal under review" }

            dl { class: "mod-kv",
                div {
                    dt { "Appellant UID" }
                    dd { class: "mono", "{appeal.submitter_uid}" }
                }
                div {
                    dt { "Filed" }
                    dd { "{fmt_datetime(appeal.created_at)}" }
                }
                div {
                    dt { "Identity proof" }
                    dd { "{appeal.identity_proof}" }
                }
            }
            // The appellant's raw statement — escaped via a text node.
            p { class: "info-hint", "Appellant's statement" }
            p { class: "mod-timeline-reason", "{appeal.statement}" }

            if can_manage {
                form {
                    class: "ban-create",
                    "aria-label": "Decide this appeal",
                    onsubmit: move |evt| evt.prevent_default(),
                    div { class: "form-row",
                        label { r#for: "appeal-note", "Decision note (optional)" }
                        textarea {
                            id: "appeal-note",
                            class: "input",
                            rows: "2",
                            placeholder: "Recorded on the appeal and the case timeline",
                            value: "{note.read()}",
                            oninput: move |e| note.set(e.value()),
                        }
                    }
                    p { class: "info-hint",
                        "Overturning lifts any TeamSpeak ban recorded on this case as its own timeline action. Both decisions resolve the case."
                    }
                    div { style: "display: flex; gap: var(--space-3); flex-wrap: wrap;",
                        Button {
                            variant: ButtonVariant::Danger,
                            kind: ButtonType::Button,
                            loading: *busy.read(),
                            onclick: move |_| decide.call("overturn"),
                            "Overturn appeal"
                        }
                        Button {
                            variant: ButtonVariant::Primary,
                            kind: ButtonType::Button,
                            loading: *busy.read(),
                            onclick: move |_| decide.call("uphold"),
                            "Uphold action"
                        }
                    }
                }
            } else {
                p { class: "info-hint",
                    "Deciding an appeal requires the case-manage permission."
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct TimelineProps {
    rows: Vec<ModerationCaseAction>,
    /// Revert is offered only on automod cases (PURA-303).
    is_automod: bool,
    can_revert_mute: bool,
    can_revert_ban: bool,
    /// A revert request is in flight — buttons render disabled.
    revert_busy: bool,
    /// Fires with the id of the `mute` / `ban` action to revert.
    on_revert: EventHandler<i64>,
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

    // An action that already carries a reverting row (`unmute` / `unban`
    // tagged with its id) must not offer a second Revert control.
    let reverted_ids: std::collections::HashSet<i64> = props
        .rows
        .iter()
        .filter_map(|a| {
            a.payload
                .as_ref()
                .and_then(|p| p.get("revertsActionId"))
                .and_then(serde_json::Value::as_i64)
        })
        .collect();

    rsx! {
        ol { class: "mod-timeline",
            for a in props.rows.iter() {
                {
                    let a = a.clone();
                    // Revert is shown for an un-reverted mute/ban on an
                    // automod case when the role holds the matching grant.
                    let can_revert = props.is_automod
                        && !reverted_ids.contains(&a.id)
                        && match a.action_kind.as_str() {
                            "mute" => props.can_revert_mute,
                            "ban" => props.can_revert_ban,
                            _ => false,
                        };
                    let action_id = a.id;
                    let on_revert = props.on_revert;
                    rsx! {
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
                                if can_revert {
                                    div { class: "mod-timeline-actions",
                                        Button {
                                            variant: ButtonVariant::Ghost,
                                            size: ButtonSize::Small,
                                            disabled: props.revert_busy,
                                            onclick: move |_| on_revert.call(action_id),
                                            "Revert"
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

            // `ban_ip` carries a raw IP instead of a clid — validate it up
            // front so the composer gives immediate feedback. The endpoint
            // re-checks it server-side (PURA-290).
            let ip_v = if kind_v == "ban_ip" {
                let v = ip.peek().trim().to_string();
                if v.is_empty() {
                    toaster.push(
                        ToastVariant::Warning,
                        "An IP address is required",
                        Some("Enter the IP address to ban.".into()),
                    );
                    busy.set(false);
                    return;
                }
                Some(v)
            } else {
                None
            };

            // Every kind — including `ban_ip` — posts to the moderation
            // action endpoint, which dispatches to TS6 and writes the
            // timeline row.
            let req = AppendActionRequest {
                action_kind: kind_v.clone(),
                reason: reason_v,
                clid: clid_v,
                ip: ip_v,
                ban_duration_secs: if matches!(kind_v.as_str(), "ban" | "ban_ip") {
                    duration_v
                } else {
                    None
                },
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
                        ip.set(String::new());
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
    /// PURA-303 — when set, the resolve form offers a false-positive
    /// toggle. Always `false` for the reopen panel.
    #[props(default)]
    is_automod: bool,
}

#[component]
fn ResolvePanel(props: LifecycleProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    if !perm::role_holds(&props.role, "moderation.case.manage") {
        return rsx! { "" };
    }

    let mut note = use_signal(String::new);
    let mut false_positive = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let case_id = props.case_id;
    let is_automod = props.is_automod;
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
            // The flag travels only on automod cases — the server tags
            // the `resolve` action's payload so the metrics view can
            // compute a per-rule false-positive rate.
            let fp_v = is_automod.then(|| *false_positive.peek());
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let req = ResolveCaseRequest {
                    resolution_note: note_v,
                    false_positive: fp_v,
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
                        false_positive.set(false);
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
                if is_automod {
                    label { class: "mod-fp-check",
                        input {
                            r#type: "checkbox",
                            checked: *false_positive.read(),
                            onchange: move |e| false_positive.set(e.checked()),
                        }
                        span { class: "mod-fp-check-text",
                            span { "Resolve as a false positive" }
                            span { class: "mod-fp-check-hint",
                                "Tags this resolution as an automod misfire — it counts against the rule in the automod metrics view."
                            }
                        }
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
