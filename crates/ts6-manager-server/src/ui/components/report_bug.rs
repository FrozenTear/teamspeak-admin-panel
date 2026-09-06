//! Report bug dialog — operator-facing control that POSTs to
//! `/api/bug-reports` with page / server / toast / WS context.
//!
//! No Sentry / browser SDK. The note is optional; context is always
//! attached. Auth rides the same [`RefreshGate`] as every other operator
//! POST. 404 / 501 are toasted as "API not landed yet" until the sibling
//! route PR merges.

use dioxus::prelude::*;

use crate::client::api::ApiError;
use crate::client::bug_reports::{self, BugReportResponse};
use crate::client::dioxus::use_auth_gate;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Button, ButtonType, ButtonVariant, Field};
use crate::ui::layout::use_servers_context;

/// Modal that collects an optional note and submits a bug report.
///
/// `page_path_override` is for SSR / unit tests. Production leaves it
/// unset and reads the live location (query string included).
#[component]
pub fn ReportBugDialog(
    mut open: Signal<bool>,
    #[props(default)] page_path_override: Option<String>,
) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let servers = use_servers_context();

    let mut note: Signal<String> = use_signal(String::new);
    let mut submitting: Signal<bool> = use_signal(|| false);

    if !*open.read() {
        return rsx! { "" };
    }

    let page_path = page_path_override
        .clone()
        .unwrap_or_else(|| bug_reports::page_path_from_location("/"));
    let selected_id = *servers.selected.read();
    let selected_name = selected_id.and_then(|id| {
        servers
            .data
            .read()
            .rows()
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
    });
    let toasts = crate::client::diagnostics::toast_messages();
    let ws_errors = crate::client::diagnostics::ws_error_messages();
    let release = bug_reports::release();
    let busy = *submitting.read();

    let on_cancel = move |_| {
        if !*submitting.read() {
            note.set(String::new());
            open.set(false);
        }
    };

    let on_submit = {
        let gate = gate.clone();
        let page_path = page_path.clone();
        move |_| {
            if *submitting.read() {
                return;
            }
            submitting.set(true);
            let gate = gate.clone();
            let body =
                bug_reports::build_request(note.read().trim(), page_path.clone(), selected_id);
            spawn(async move {
                match bug_reports::submit(gate, &body).await {
                    Ok(resp) => {
                        push_success_toast(toaster, &resp);
                        note.set(String::new());
                        open.set(false);
                    }
                    Err(e) => {
                        if e.is_unauthorized() {
                            toaster.push(
                                ToastVariant::Danger,
                                "Session expired. Sign in again.",
                                None,
                            );
                        } else if bug_reports::is_route_unavailable(&e) {
                            toaster.push(
                                ToastVariant::Warning,
                                "Could not send bug report",
                                Some(bug_reports::unavailable_message().to_string()),
                            );
                        } else if bug_reports::is_sink_unconfigured(&e) {
                            toaster.push(
                                ToastVariant::Warning,
                                "Could not send bug report",
                                Some(bug_reports::sink_unconfigured_message().to_string()),
                            );
                        } else {
                            toaster.push(
                                ToastVariant::Danger,
                                "Could not send bug report",
                                Some(format_submit_error(&e)),
                            );
                        }
                    }
                }
                submitting.set(false);
            });
        }
    };

    rsx! {
        div {
            class: "modal-backdrop",
            onclick: move |_| {
                if !busy {
                    note.set(String::new());
                    open.set(false);
                }
            },
            onkeydown: move |evt| {
                if evt.key() == Key::Escape && !busy {
                    evt.prevent_default();
                    note.set(String::new());
                    open.set(false);
                }
            },
            div {
                class: "modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "report-bug-title",
                "aria-describedby": "report-bug-lede",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-header",
                    h2 { id: "report-bug-title", "Report bug" }
                }
                div { class: "modal-body stack-md",
                    p { id: "report-bug-lede", class: "info-hint",
                        "Optional note — page path, selected server id, recent toasts, and recent connection errors are always attached. Nothing is sent to a third-party crash service."
                    }
                    Field {
                        label: "What happened?".to_string(),
                        id: Some("report-bug-note".to_string()),
                        optional: true,
                        helper: Some("A sentence or two is enough. Context below is sent either way.".to_string()),
                        textarea {
                            id: "report-bug-note",
                            class: "input",
                            rows: "4",
                            placeholder: "e.g. Clients table stayed on Loading after I switched servers",
                            value: "{note.read()}",
                            disabled: busy,
                            oninput: move |e| note.set(e.value()),
                        }
                    }
                    ContextPreview {
                        page_path: page_path,
                        server_id: selected_id,
                        server_name: selected_name,
                        toasts: toasts,
                        ws_errors: ws_errors,
                        release: release,
                    }
                }
                div { class: "modal-footer",
                    button {
                        r#type: "button",
                        class: "btn btn-ghost",
                        disabled: busy,
                        onclick: on_cancel,
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Button,
                        loading: busy,
                        onclick: on_submit,
                        "Send report"
                    }
                }
            }
        }
    }
}

#[component]
fn ContextPreview(
    page_path: String,
    server_id: Option<i64>,
    server_name: Option<String>,
    toasts: Vec<String>,
    ws_errors: Vec<String>,
    release: Option<String>,
) -> Element {
    let server_line = match (server_name.as_deref(), server_id) {
        (Some(name), Some(id)) => format!("{name} (id {id})"),
        (None, Some(id)) => format!("id {id}"),
        _ => "None selected".to_string(),
    };
    let release_line = release.unwrap_or_else(|| "Unknown".into());

    rsx! {
        div { class: "bug-report-context",
            p { class: "bug-report-context__title", "Attached automatically" }
            dl { class: "bug-report-context__list",
                dt { "Page" }
                dd { "{page_path}" }
                dt { "Server" }
                dd { "{server_line}" }
                dt { "Release" }
                dd { "{release_line}" }
                dt { "Recent toasts" }
                dd {
                    SnapshotList {
                        empty: "None yet".to_string(),
                        items: toasts,
                    }
                }
                dt { "Recent connection errors" }
                dd {
                    SnapshotList {
                        empty: "None yet".to_string(),
                        items: ws_errors,
                    }
                }
            }
        }
    }
}

#[component]
fn SnapshotList(empty: String, items: Vec<String>) -> Element {
    if items.is_empty() {
        return rsx! { span { class: "bug-report-context__empty", "{empty}" } };
    }
    rsx! {
        ul { class: "bug-report-context__items",
            for (i, line) in items.iter().enumerate() {
                li { key: "{i}", "{line}" }
            }
        }
    }
}

fn push_success_toast(toaster: crate::ui::components::Toaster, resp: &BugReportResponse) {
    let title = format!("Bug reported — #{}", resp.issue_number);
    let href = resp.issue_url.trim();
    let href = if href.is_empty() {
        None
    } else {
        Some(href.to_string())
    };
    toaster.push_with_link(ToastVariant::Success, title, None, href);
}

fn format_submit_error(err: &ApiError) -> String {
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
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::SessionAnonymous => "Session is still loading.".into(),
        ApiError::UnsupportedTarget => "Reporting is unavailable in this view.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use crate::ui::components::provide_toaster;
    use crate::ui::layout::{ServersContext, ServersData};
    use ts6_manager_shared::auth::UserInfo;
    use ts6_manager_shared::servers::ServerSummary;

    #[test]
    fn success_title_includes_issue_number() {
        let resp = BugReportResponse {
            issue_url: "https://github.com/FrozenTear/teamspeak-admin-panel/issues/99".into(),
            issue_number: 99,
        };
        let title = format!("Bug reported — #{}", resp.issue_number);
        assert_eq!(title, "Bug reported — #99");
        assert_eq!(
            resp.issue_url,
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/99"
        );
    }

    #[test]
    fn unavailable_errors_get_dedicated_copy() {
        let err = ApiError::Client {
            status: 404,
            message: "Not found".into(),
        };
        assert!(bug_reports::is_route_unavailable(&err));
        assert!(bug_reports::unavailable_message().contains("not available"));
    }

    fn fixture_server() -> ServerSummary {
        let now = chrono::Utc::now();
        ServerSummary {
            id: 1,
            name: "Scuffed World".into(),
            host: "ts.example.com".into(),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: None,
            has_ssh_credentials: false,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
            last_seen_at: None,
        }
    }

    #[component]
    fn DialogHarness() -> Element {
        crate::client::diagnostics::reset_for_tests();
        crate::client::diagnostics::record_toast("error", "Kick failed");
        crate::client::diagnostics::record_client_error("websocket disconnected");
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "rsoot".into(),
                    display_name: "Robert Soot".into(),
                    role: "admin".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
            ready: SyncSignal::new_maybe_sync(true),
        });
        use_context_provider(|| provide_auth_gate(session));
        let _ = provide_toaster();
        use_context_provider(|| ServersContext {
            data: Signal::new(ServersData::Loaded(vec![fixture_server()])),
            selected: Signal::new(Some(1)),
        });
        let open = use_signal(|| true);
        rsx! { ReportBugDialog { open, page_path_override: Some("/clients".into()) } }
    }

    fn render_open_dialog() -> String {
        let _lock = crate::client::diagnostics::exclusive_for_tests();
        let mut dom = VirtualDom::new(DialogHarness);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn open_dialog_shows_note_and_attached_context() {
        let html = render_open_dialog();
        assert!(
            html.contains(r#"role="dialog""#),
            "dialog role missing: {html}"
        );
        assert!(
            html.contains(r#"id="report-bug-title""#),
            "title id missing: {html}"
        );
        assert!(html.contains("Report bug"), "heading missing: {html}");
        assert!(
            html.contains("What happened?"),
            "note field missing: {html}"
        );
        assert!(
            html.contains("(optional)"),
            "note should be marked optional: {html}"
        );
        assert!(
            html.contains("Attached automatically"),
            "context preview missing: {html}"
        );
        assert!(html.contains("/clients"), "page path missing: {html}");
        assert!(
            html.contains("Scuffed World"),
            "selected server name missing: {html}"
        );
        assert!(
            html.contains("Kick failed"),
            "toast ring missing from preview: {html}"
        );
        assert!(
            html.contains("websocket disconnected"),
            "ws error ring missing from preview: {html}"
        );
        assert!(html.contains("Send report"), "submit CTA missing: {html}");
        assert!(html.contains("Cancel"), "cancel missing: {html}");
    }
}
