//! Typed client for `POST /api/bug-reports`.
//!
//! Wire shape is locked by API PR #28 (`ts6_manager_shared::bug_reports`).
//! Types live in this module so the Panel compiles before that PR merges;
//! field names and optionality match the shared DTO byte-for-byte.
//!
//! ```text
//! POST /api/bug-reports   RequireAuth
//! { pagePath, serverId?, note?, toasts[]?, wsErrors[]?, release?, context? }
//! → 201 { issueUrl, issueNumber }
//! ```
//!
//! `toasts` / `wsErrors` are plain strings. `context` is an optional
//! string→JSON map reserved for Music / Voice / Sidecar — Panel omits it.
//! 404 / 501 stay tolerated until the route lands; 503 is the configured-
//! but-token-unset sink.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// Caps from the shared DTO so we truncate before POST.
const MAX_NOTE_LEN: usize = 4096;
const MAX_LIST_ITEMS: usize = 20;
const MAX_LIST_ITEM_LEN: usize = 500;
const MAX_RELEASE_LEN: usize = 128;

/// `POST /api/bug-reports` body. Field names are camelCase on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportRequest {
    pub page_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toasts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_errors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Seat-context bag. Panel leaves this unset; other seats fill it later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Map<String, Value>>,
}

/// `POST /api/bug-reports` 201 body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportResponse {
    pub issue_url: String,
    pub issue_number: i64,
}

/// `true` when the panel build does not yet expose the route (API PR
/// pending). Callers surface a dedicated toast rather than a generic 5xx.
pub fn is_route_unavailable(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Client { status: 404, .. } | ApiError::Server { status: 501, .. }
    )
}

/// `true` when the route is up but `BUG_REPORTS_GITHUB_TOKEN` is unset (503).
pub fn is_sink_unconfigured(err: &ApiError) -> bool {
    matches!(err, ApiError::Server { status: 503, .. })
}

/// Operator-facing copy for a 404 / 501 until the API PR merges.
pub fn unavailable_message() -> &'static str {
    "Bug reports are not available on this panel yet (the API route has not landed)."
}

/// Operator-facing copy for a 503 unconfigured sink.
pub fn sink_unconfigured_message() -> &'static str {
    "Bug reports are not configured (BUG_REPORTS_GITHUB_TOKEN unset)."
}

/// Auth-gated `POST /api/bug-reports`. Success is 201 `{ issueUrl, issueNumber }`.
pub async fn submit(
    gate: Arc<RefreshGate>,
    body: &BugReportRequest,
) -> Result<BugReportResponse, ApiError> {
    api::authorized_post_json(
        gate.as_ref(),
        &api::api_base(),
        "/api/bug-reports",
        Some(body),
    )
    .await
}

/// Build the locked request from values the dialog already collected.
pub fn build_request(
    note: impl Into<String>,
    page_path: impl Into<String>,
    server_id: Option<i64>,
) -> BugReportRequest {
    BugReportRequest {
        page_path: page_path.into(),
        server_id,
        note: nonempty_truncated(note.into(), MAX_NOTE_LEN),
        toasts: nonempty_list(crate::client::diagnostics::toast_messages()),
        ws_errors: nonempty_list(crate::client::diagnostics::ws_error_messages()),
        release: release(),
        context: None,
    }
}

/// Panel version stamped as `release` when the crate version is set.
pub fn release() -> Option<String> {
    nonempty_truncated(env!("CARGO_PKG_VERSION"), MAX_RELEASE_LEN)
}

/// SPA path used as `pagePath`. Prefers the live location (so query
/// strings survive); falls back to the supplied path.
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

fn nonempty_truncated(raw: impl AsRef<str>, max: usize) -> Option<String> {
    let trimmed = raw.as_ref().trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, max))
}

fn nonempty_list(items: Vec<String>) -> Option<Vec<String>> {
    let capped: Vec<String> = items
        .into_iter()
        .filter_map(|item| nonempty_truncated(item, MAX_LIST_ITEM_LEN))
        .take(MAX_LIST_ITEMS)
        .collect();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::diagnostics;

    #[test]
    fn request_serialises_locked_camel_case() {
        let req = BugReportRequest {
            page_path: "/clients".into(),
            server_id: Some(1),
            note: Some("clients table stuck".into()),
            toasts: Some(vec!["Kick failed".into()]),
            ws_errors: Some(vec!["websocket disconnected".into()]),
            release: Some("0.0.1".into()),
            context: None,
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["pagePath"], "/clients");
        assert_eq!(json["serverId"], 1);
        assert_eq!(json["note"], "clients table stuck");
        assert_eq!(json["toasts"], serde_json::json!(["Kick failed"]));
        assert_eq!(
            json["wsErrors"],
            serde_json::json!(["websocket disconnected"])
        );
        assert_eq!(json["release"], "0.0.1");
        assert!(json.get("context").is_none());
        assert!(json.get("serverName").is_none());
        assert!(json.get("userAgent").is_none());
        assert!(json.get("appVersion").is_none());
        assert!(json.get("page_path").is_none());
        assert!(json.get("ws_errors").is_none());
        assert!(json["toasts"][0].is_string());
        assert!(json["wsErrors"][0].is_string());
    }

    #[test]
    fn request_omits_optional_fields_when_empty() {
        let req = BugReportRequest {
            page_path: "/dashboard".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["pagePath"], "/dashboard");
        for key in [
            "serverId", "note", "toasts", "wsErrors", "release", "context",
        ] {
            assert!(
                json.get(key).is_none(),
                "expected {key} omitted, got {json}"
            );
        }
    }

    #[test]
    fn response_requires_issue_url_and_number() {
        let resp: BugReportResponse = serde_json::from_str(
            r#"{"issueUrl":"https://github.com/FrozenTear/teamspeak-admin-panel/issues/12","issueNumber":12}"#,
        )
        .unwrap();
        assert_eq!(
            resp.issue_url,
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/12"
        );
        assert_eq!(resp.issue_number, 12);

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""issueUrl":"#));
        assert!(json.contains(r#""issueNumber":12"#));
        assert!(!json.contains("issue_url"));
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
            status: 503,
            message: "unset".into(),
        }));
    }

    #[test]
    fn sink_unconfigured_is_503() {
        assert!(is_sink_unconfigured(&ApiError::Server {
            status: 503,
            message: "Bug reports are not configured (BUG_REPORTS_GITHUB_TOKEN unset).".into(),
        }));
        assert!(!is_sink_unconfigured(&ApiError::Server {
            status: 502,
            message: "Failed to create GitHub issue".into(),
        }));
    }

    #[test]
    fn build_request_matches_locked_shape() {
        diagnostics::reset_for_tests();
        diagnostics::record_toast("warning", "saved");
        diagnostics::record_client_error("websocket disconnected");
        let req = build_request("note", "/clients", Some(7));
        assert_eq!(req.page_path, "/clients");
        assert_eq!(req.server_id, Some(7));
        assert_eq!(req.note.as_deref(), Some("note"));
        assert_eq!(
            req.toasts.as_deref(),
            Some([String::from("saved")].as_slice())
        );
        assert_eq!(
            req.ws_errors.as_deref(),
            Some([String::from("websocket disconnected")].as_slice())
        );
        assert_eq!(req.release.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert!(req.context.is_none());
    }

    #[test]
    fn build_request_omits_blank_note() {
        diagnostics::reset_for_tests();
        let req = build_request("   ", "/logs", None);
        assert!(req.note.is_none());
        assert!(req.server_id.is_none());
        assert!(req.toasts.is_none());
        assert!(req.ws_errors.is_none());
    }

    #[test]
    fn release_is_crate_semver() {
        let v = release().expect("CARGO_PKG_VERSION is set");
        assert!(!v.is_empty());
        assert!(v.chars().next().unwrap().is_ascii_digit());
    }
}
