//! `/moderation/subjects/{uid}` — per-subject moderation history. PURA-287.
//!
//! Everything recorded against one subject UID, fanned in from
//! `GET /api/moderation/subjects/{uid}/history`: every case, every case
//! action across those cases, and every free-text moderator note. The
//! note composer (`POST …/notes`) lets a moderator add a UID-scoped note
//! that is independent of any single case.
//!
//! Page-gated to `admin` + `moderator` and `moderation.history.view`; the
//! note composer additionally requires `moderation.note.write`.

use dioxus::prelude::*;
use ts6_manager_shared::moderation::{CreateNoteRequest, ModerationNote, SubjectHistory};

use crate::client::api::{self};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonType, ButtonVariant};
use crate::ui::routes::Route;

use super::perm;
use super::{
    AccessDenied, action_kind_icon, action_kind_label, case_status_class, fmt_datetime,
    format_error, relative_when,
};

/// Build an API path for a subject sub-resource, percent-encoding the
/// base64 UID as a single path segment. TS6 client UIDs routinely contain
/// `/`, `+`, `=`; an unencoded `/` splits the segment, misses the route,
/// and falls through to the SPA HTML fallback. PURA-293.
fn subject_api_path(uid: &str, suffix: &str) -> String {
    format!(
        "/api/moderation/subjects/{}/{suffix}",
        urlencoding::encode(uid)
    )
}

#[component]
pub fn SubjectHistoryPage(uid: String) -> Element {
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
    if !perm::role_can_moderate(&role) || !perm::role_holds(&role, "moderation.history.view") {
        return rsx! {
            AccessDenied {
                crumb: "Moderation · Subject".to_string(),
                heading: "Subject history".to_string(),
                detail: "Subject history requires the moderation history-view permission.".to_string(),
            }
        };
    }

    let can_write_notes = perm::role_holds(&role, "moderation.note.write");
    let gate = use_auth_gate();

    let reload: Signal<u64> = use_signal(|| 0u64);
    let history = use_resource({
        let gate = gate.clone();
        let uid = uid.clone();
        move || {
            let gate = gate.clone();
            let uid = uid.clone();
            let _ = *reload.read();
            async move {
                let path = subject_api_path(&uid, "history");
                api::authorized_get_json::<SubjectHistory>(&gate, &api::api_base(), &path).await
            }
        }
    });

    let snapshot = history.read().clone();

    rsx! {
        div { class: "crumb",
            Link { to: Route::ModerationQueuePage {}, "Moderation" }
            " · Subject"
        }
        h1 { "Subject history" }
        p { class: "info-hint mono", "{uid}" }

        match snapshot {
            None => rsx! {
                p { class: "info-hint", "Loading history…" }
            },
            Some(Err(e)) => rsx! {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load history".to_string(),
                    "{format_error(&e)}"
                }
            },
            Some(Ok(history)) => rsx! {
                HistoryBody {
                    history,
                    uid: uid.clone(),
                    can_write_notes,
                    reload,
                }
            },
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct HistoryBodyProps {
    history: SubjectHistory,
    uid: String,
    can_write_notes: bool,
    reload: Signal<u64>,
}

#[component]
fn HistoryBody(props: HistoryBodyProps) -> Element {
    let h = props.history.clone();
    let case_count = h.cases.len();
    let action_count = h.actions.len();
    let note_count = h.notes.len();

    rsx! {
        p { class: "info-hint",
            "{case_count} case(s) · {action_count} action(s) · {note_count} note(s)"
        }

        // ── cases ───────────────────────────────────────────────────────
        section { class: "stack-md mod-panel",
            h2 { "Cases" }
            if h.cases.is_empty() {
                div { class: "empty",
                    div { class: "icon", "✓" }
                    h3 { "No cases" }
                    p { "This subject has no moderation cases on record." }
                }
            } else {
                table { class: "data-table", "aria-label": "Cases for this subject",
                    thead {
                        tr {
                            th { scope: "col", "Case" }
                            th { scope: "col", "Reason" }
                            th { scope: "col", "Status" }
                            th { scope: "col", "Opened" }
                        }
                    }
                    tbody {
                        for c in h.cases.iter() {
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
                                        td { "{c.reason}" }
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

        // ── action history ──────────────────────────────────────────────
        section { class: "stack-md mod-panel",
            h2 { "Actions" }
            if h.actions.is_empty() {
                div { class: "empty",
                    div { class: "icon", "✎" }
                    h3 { "No actions" }
                    p { "Kicks, bans, and mutes against this subject will appear here." }
                }
            } else {
                ol { class: "mod-timeline",
                    for a in h.actions.iter() {
                        li { key: "{a.id}", class: "mod-timeline-row",
                            span { class: "mod-timeline-icon", aria_hidden: "true",
                                "{action_kind_icon(&a.action_kind)}"
                            }
                            div { class: "mod-timeline-body",
                                div { class: "mod-timeline-head",
                                    strong { "{action_kind_label(&a.action_kind)}" }
                                    span { class: "muted", " by {a.actor_username_snapshot}" }
                                    Link {
                                        to: Route::ModerationCasePage { case_id: a.case_id },
                                        class: "mod-timeline-caselink",
                                        "case #{a.case_id}"
                                    }
                                    span { class: "mod-timeline-when", "{relative_when(a.created_at)}" }
                                }
                                p { class: "mod-timeline-reason", "{a.reason}" }
                            }
                        }
                    }
                }
            }
        }

        // ── notes ───────────────────────────────────────────────────────
        section { class: "stack-md mod-panel",
            h2 { "Notes" }
            NoteList { notes: h.notes }
            if props.can_write_notes {
                NoteComposer { uid: props.uid.clone(), reload: props.reload }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct NoteListProps {
    notes: Vec<ModerationNote>,
}

#[component]
fn NoteList(props: NoteListProps) -> Element {
    if props.notes.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "✎" }
                h3 { "No notes" }
                p { "Moderator notes on this subject will appear here." }
            }
        };
    }
    rsx! {
        ul { class: "mod-note-list",
            for n in props.notes.iter() {
                li { key: "{n.id}", class: "mod-note",
                    p { class: "mod-note-body", "{n.body}" }
                    p { class: "info-hint",
                        "{n.author_username_snapshot} · {relative_when(n.created_at)}"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct NoteComposerProps {
    uid: String,
    reload: Signal<u64>,
}

#[component]
fn NoteComposer(props: NoteComposerProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let mut body = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let uid = props.uid.clone();
    let mut reload = props.reload;

    let on_add = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() {
                return;
            }
            let body_v = body.peek().trim().to_string();
            if body_v.is_empty() {
                toaster.push(
                    ToastVariant::Warning,
                    "Note text is required",
                    None,
                );
                return;
            }
            let gate = gate.clone();
            let toaster = toaster;
            let uid = uid.clone();
            busy.set(true);
            spawn(async move {
                let req = CreateNoteRequest { body: body_v };
                let path = subject_api_path(&uid, "notes");
                let res = api::authorized_post_json::<_, ModerationNote>(
                    &gate,
                    &api::api_base(),
                    &path,
                    Some(&req),
                )
                .await;
                busy.set(false);
                match res {
                    Ok(_) => {
                        toaster.push(ToastVariant::Success, "Note added", None);
                        body.set(String::new());
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not add note",
                        Some(format_error(&e)),
                    ),
                }
            });
        }
    };

    rsx! {
        form {
            class: "ban-create",
            "aria-label": "Add a note",
            onsubmit: move |evt| { evt.prevent_default(); on_add.clone()(()); },
            div { class: "form-row",
                label { r#for: "note-body", "New note" }
                textarea {
                    id: "note-body",
                    class: "input",
                    rows: "2",
                    placeholder: "A note about this subject, visible to all moderators",
                    value: "{body.read()}",
                    oninput: move |e| body.set(e.value()),
                }
            }
            Button {
                variant: ButtonVariant::Primary,
                kind: ButtonType::Submit,
                loading: *busy.read(),
                "Add note"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::subject_api_path;

    /// PURA-293 — TS6 base64 client UIDs contain `/`, `+`, `=`; each must be
    /// percent-encoded so the API path stays a single segment and hits the
    /// route instead of the SPA HTML fallback.
    #[test]
    fn subject_api_path_percent_encodes_base64_uid() {
        let uid = "aB3/xY+z9w==";
        assert_eq!(
            subject_api_path(uid, "history"),
            "/api/moderation/subjects/aB3%2FxY%2Bz9w%3D%3D/history"
        );
        assert_eq!(
            subject_api_path(uid, "notes"),
            "/api/moderation/subjects/aB3%2FxY%2Bz9w%3D%3D/notes"
        );
    }

    /// A plain alphanumeric UID round-trips unchanged.
    #[test]
    fn subject_api_path_leaves_plain_uid_untouched() {
        assert_eq!(
            subject_api_path("plainuid123", "history"),
            "/api/moderation/subjects/plainuid123/history"
        );
    }
}
