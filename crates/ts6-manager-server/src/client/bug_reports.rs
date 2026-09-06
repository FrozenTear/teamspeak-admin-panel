//! Typed client for `POST /api/bug-reports`.
//!
//! The API route is landing in a sibling PR. This module owns the
//! provisional camelCase request / response shapes so the panel can ship
//! the Report bug control without waiting on a shared DTO. 404 and 501
//! are treated as "route not available yet" so a panel build that predates
//! the API still fails cleanly instead of looking like a crash.
//!
//! No backend route is registered here — client call only.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::client::api::{self, ApiError};
use crate::client::diagnostics::{ClientErrorSnapshot, ToastSnapshot};
use crate::client::session::RefreshGate;

/// `POST /api/bug-reports` body. Field names are camelCase on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportRequest {
    pub note: String,
    pub page_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    pub toasts: Vec<ToastSnapshot>,
    pub ws_errors: Vec<ClientErrorSnapshot>,
    pub user_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
}

/// Success body. Either field (or both, or neither on 204) is accepted so
/// the UI stays tolerant of the API PR's final shape.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportResponse {
    #[serde(default, alias = "issue_url")]
    pub issue_url: Option<String>,
    #[serde(default, alias = "issue_number")]
    pub issue_number: Option<i64>,
}

/// `true` when the panel build does not yet expose the route (API PR
/// pending). Callers surface a dedicated toast rather than a generic 5xx.
pub fn is_route_unavailable(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Client { status: 404, .. } | ApiError::Server { status: 501, .. }
    )
}

/// Operator-facing copy for a 404 / 501 until the API PR merges.
pub fn unavailable_message() -> &'static str {
    "Bug reports are not available on this panel yet (the API route has not landed)."
}

/// Auth-gated `POST /api/bug-reports`. A 204 / empty 2xx body is success
/// with an empty [`BugReportResponse`].
pub async fn submit(
    gate: Arc<RefreshGate>,
    body: &BugReportRequest,
) -> Result<BugReportResponse, ApiError> {
    let parsed: Option<BugReportResponse> = api::authorized_post_json(
        gate.as_ref(),
        &api::api_base(),
        "/api/bug-reports",
        Some(body),
    )
    .await?;
    Ok(parsed.unwrap_or_default())
}

/// Build the request from values the dialog already collected. Kept as a
/// free function so unit tests can assert the wire shape without mounting
/// Dioxus.
pub fn build_request(
    note: impl Into<String>,
    page_path: impl Into<String>,
    server_id: Option<i64>,
    server_name: Option<String>,
    user_agent: impl Into<String>,
    app_version: Option<String>,
) -> BugReportRequest {
    BugReportRequest {
        note: note.into(),
        page_path: page_path.into(),
        server_id,
        server_name,
        toasts: crate::client::diagnostics::snapshot_toasts(),
        ws_errors: crate::client::diagnostics::snapshot_client_errors(),
        user_agent: user_agent.into(),
        app_version,
    }
}

/// Panel version stamped into the payload when the UI knows one.
pub fn app_version() -> Option<String> {
    let v = env!("CARGO_PKG_VERSION");
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Browser UA on WASM; empty string on native / SSR.
pub fn user_agent() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.navigator().user_agent().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// SPA path used as `pagePath`. Prefers the live location (so query
/// strings survive); falls back to the Routable display path.
pub fn page_path_from_location(fallback: &str) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            if let Ok(mut path) = loc.pathname() {
                if let Ok(search) = loc.search()
                    && !search.is_empty()
                {
                    path.push_str(&search);
                }
                if !path.is_empty() {
                    return path;
                }
            }
        }
        fallback.to_string()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::diagnostics::{self, ClientErrorSnapshot, ToastSnapshot};

    #[test]
    fn request_serialises_camel_case() {
        diagnostics::reset_for_tests();
        let req = BugReportRequest {
            note: "clients table stuck".into(),
            page_path: "/clients".into(),
            server_id: Some(1),
            server_name: Some("Scuffed World".into()),
            toasts: vec![ToastSnapshot {
                variant: "error".into(),
                message: "Kick failed".into(),
                at: "2026-09-06T18:00:00.000Z".into(),
            }],
            ws_errors: vec![ClientErrorSnapshot {
                message: "websocket disconnected".into(),
                at: "2026-09-06T18:00:01.000Z".into(),
            }],
            user_agent: "Mozilla/5.0".into(),
            app_version: Some("0.0.1".into()),
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["note"], "clients table stuck");
        assert_eq!(json["pagePath"], "/clients");
        assert_eq!(json["serverId"], 1);
        assert_eq!(json["serverName"], "Scuffed World");
        assert_eq!(json["toasts"][0]["variant"], "error");
        assert_eq!(json["toasts"][0]["message"], "Kick failed");
        assert_eq!(json["toasts"][0]["at"], "2026-09-06T18:00:00.000Z");
        assert_eq!(json["wsErrors"][0]["message"], "websocket disconnected");
        assert_eq!(json["userAgent"], "Mozilla/5.0");
        assert_eq!(json["appVersion"], "0.0.1");
        assert!(json.get("server_id").is_none());
        assert!(json.get("page_path").is_none());
    }

    #[test]
    fn request_omits_optional_server_and_version_when_none() {
        let req = BugReportRequest {
            note: String::new(),
            page_path: "/dashboard".into(),
            server_id: None,
            server_name: None,
            toasts: Vec::new(),
            ws_errors: Vec::new(),
            user_agent: String::new(),
            app_version: None,
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert!(json.get("serverId").is_none());
        assert!(json.get("serverName").is_none());
        assert!(json.get("appVersion").is_none());
        assert_eq!(json["note"], "");
        assert_eq!(json["toasts"], serde_json::json!([]));
        assert_eq!(json["wsErrors"], serde_json::json!([]));
    }

    #[test]
    fn response_accepts_camel_case_and_snake_aliases() {
        let camel: BugReportResponse = serde_json::from_str(
            r#"{"issueUrl":"https://github.com/org/repo/issues/12","issueNumber":12}"#,
        )
        .unwrap();
        assert_eq!(
            camel.issue_url.as_deref(),
            Some("https://github.com/org/repo/issues/12")
        );
        assert_eq!(camel.issue_number, Some(12));

        let snake: BugReportResponse =
            serde_json::from_str(r#"{"issue_url":"https://example.test/42","issue_number":42}"#)
                .unwrap();
        assert_eq!(snake.issue_url.as_deref(), Some("https://example.test/42"));
        assert_eq!(snake.issue_number, Some(42));

        let empty: BugReportResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, BugReportResponse::default());
    }

    #[test]
    fn route_unavailable_is_404_or_501() {
        assert!(is_route_unavailable(&ApiError::Client {
            status: 404,
            message: "Not found".into(),
        }));
        assert!(is_route_unavailable(&ApiError::Server {
            status: 501,
            message: "Not implemented".into(),
        }));
        assert!(!is_route_unavailable(&ApiError::Server {
            status: 500,
            message: "boom".into(),
        }));
        assert!(!is_route_unavailable(&ApiError::Transport("net".into())));
    }

    #[test]
    fn build_request_pulls_current_rings() {
        diagnostics::reset_for_tests();
        diagnostics::record_toast("warning", "saved");
        diagnostics::record_client_error("websocket disconnected");
        let req = build_request(
            "note",
            "/clients",
            Some(7),
            Some("Primary".into()),
            "ua",
            Some("0.0.1".into()),
        );
        assert_eq!(req.note, "note");
        assert_eq!(req.page_path, "/clients");
        assert_eq!(req.server_id, Some(7));
        assert_eq!(req.server_name.as_deref(), Some("Primary"));
        assert_eq!(req.toasts.len(), 1);
        assert_eq!(req.toasts[0].message, "saved");
        assert_eq!(req.ws_errors.len(), 1);
        assert_eq!(req.ws_errors[0].message, "websocket disconnected");
        assert_eq!(req.app_version.as_deref(), Some("0.0.1"));
    }

    #[test]
    fn app_version_is_crate_semver() {
        let v = app_version().expect("CARGO_PKG_VERSION is set");
        assert!(!v.is_empty());
        assert!(v.chars().next().unwrap().is_ascii_digit());
    }
}
