//! Typed client for `POST /api/bug-reports`.
//!
//! Confirmed camelCase body (API PR #28):
//!
//! ```text
//! POST /api/bug-reports   RequireAuth
//! { pagePath, serverId?, note?, toasts[], wsErrors[], release? }
//! → 201 { issueUrl, issueNumber }
//! ```
//!
//! `toasts` and `wsErrors` are always plain string arrays (never objects).
//! Types live in this module so the Panel compiles before the shared DTO
//! crate lands. 404 / 501 stay tolerated until the route merges; 503 is
//! the configured-but-token-unset sink.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// Caps from the shared DTO so we truncate before POST.
const MAX_PAGE_PATH_LEN: usize = 512;
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
    #[serde(default)]
    pub toasts: Vec<String>,
    #[serde(default)]
    pub ws_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
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
        page_path: cap_page_path(page_path),
        server_id,
        note: nonempty_truncated(note.into(), MAX_NOTE_LEN),
        toasts: cap_list(crate::client::diagnostics::toast_messages()),
        ws_errors: cap_list(crate::client::diagnostics::ws_error_messages()),
        release: release(),
    }
}

/// Panel version stamped as `release` (e.g. `v0.0.1`).
pub fn release() -> Option<String> {
    let raw = env!("CARGO_PKG_VERSION").trim();
    if raw.is_empty() {
        return None;
    }
    let tagged = if raw.starts_with('v') {
        raw.to_string()
    } else {
        format!("v{raw}")
    };
    Some(truncate_chars(&tagged, MAX_RELEASE_LEN))
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

fn cap_page_path(raw: impl Into<String>) -> String {
    let raw = raw.into();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    truncate_chars(trimmed, MAX_PAGE_PATH_LEN)
}

fn nonempty_truncated(raw: impl AsRef<str>, max: usize) -> Option<String> {
    let trimmed = raw.as_ref().trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, max))
}

fn cap_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .filter_map(|item| nonempty_truncated(item, MAX_LIST_ITEM_LEN))
        .take(MAX_LIST_ITEMS)
        .collect()
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
            toasts: vec!["Kick failed".into()],
            ws_errors: vec!["websocket disconnected".into()],
            release: Some("v1.6.9".into()),
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
        assert_eq!(json["release"], "v1.6.9");
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
    fn request_always_sends_string_arrays_and_omits_empty_optionals() {
        let req = BugReportRequest {
            page_path: "/dashboard".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["pagePath"], "/dashboard");
        assert_eq!(json["toasts"], serde_json::json!([]));
        assert_eq!(json["wsErrors"], serde_json::json!([]));
        for key in ["serverId", "note", "release", "context"] {
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
        assert_eq!(req.toasts, vec![String::from("saved")]);
        assert_eq!(req.ws_errors, vec![String::from("websocket disconnected")]);
        let expected_release = format!("v{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(req.release.as_deref(), Some(expected_release.as_str()));
    }

    #[test]
    fn build_request_omits_blank_note_and_sends_empty_arrays() {
        diagnostics::reset_for_tests();
        let req = build_request("   ", "/logs", None);
        assert!(req.note.is_none());
        assert!(req.server_id.is_none());
        assert!(req.toasts.is_empty());
        assert!(req.ws_errors.is_empty());
    }

    #[test]
    fn build_request_trims_and_caps_page_path() {
        diagnostics::reset_for_tests();
        let req = build_request("", "  /clients?tab=bans  ", None);
        assert_eq!(req.page_path, "/clients?tab=bans");

        let too_long = format!("/{}", "x".repeat(MAX_PAGE_PATH_LEN + 8));
        let req = build_request("", too_long, None);
        assert_eq!(req.page_path.chars().count(), MAX_PAGE_PATH_LEN);

        let req = build_request("", "   ", None);
        assert_eq!(req.page_path, "/");
    }

    #[test]
    fn release_is_v_prefixed_crate_semver() {
        let v = release().expect("CARGO_PKG_VERSION is set");
        assert!(v.starts_with('v'), "expected v-prefix, got {v}");
        assert!(v.as_bytes().get(1).is_some_and(|c| c.is_ascii_digit()));
    }
}
