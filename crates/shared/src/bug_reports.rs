//! Wire-format types for `POST /api/bug-reports`.
//!
//! Field names on the wire are camelCase (`pagePath`, `serverId`, `wsErrors`,
//! `issueUrl`, `issueNumber`). Rust fields stay snake_case with
//! `#[serde(rename_all = "camelCase")]` at the (de)serialise boundary — the
//! same convention as [`crate::auth`] and [`crate::music_bots`].
//!
//! The Dioxus Panel (out of this crate's ownership) deserialises these types
//! directly. Caps below are part of the shared contract so the Panel can
//! truncate before POST and match the server's validator.

use serde::{Deserialize, Serialize};

/// Maximum `pagePath` length after trim. Longer values are rejected (400).
pub const MAX_PAGE_PATH_LEN: usize = 512;
/// Maximum operator `note` length. Longer values are truncated.
pub const MAX_NOTE_LEN: usize = 4096;
/// Maximum items accepted in `toasts` / `wsErrors`. Extra items are dropped.
pub const MAX_LIST_ITEMS: usize = 20;
/// Maximum length of one `toasts` / `wsErrors` item. Longer items are truncated.
pub const MAX_LIST_ITEM_LEN: usize = 500;
/// Maximum `release` length. Longer values are truncated.
pub const MAX_RELEASE_LEN: usize = 128;
/// Maximum keys accepted in `context`. Extra keys are dropped (insertion order).
pub const MAX_CONTEXT_KEYS: usize = 32;
/// Maximum `context` key length. Longer keys are truncated.
pub const MAX_CONTEXT_KEY_LEN: usize = 64;
/// Maximum rendered length of one `context` value. Longer values are truncated.
pub const MAX_CONTEXT_VALUE_LEN: usize = 4096;
/// Maximum total UTF-8 bytes across rendered `context` values. Extra keys dropped.
pub const MAX_CONTEXT_BYTES: usize = 32 * 1024;

/// Stable error strings for the bug-report surface.
///
/// The server returns these verbatim in `{ "error": "..." }` so the Panel
/// can branch without scraping free-form text.
pub mod error_strings {
    pub const PAGE_PATH_REQUIRED: &str = "pagePath is required";
    pub const PAGE_PATH_TOO_LONG: &str = "pagePath is too long";
    pub const SINK_UNCONFIGURED: &str =
        "Bug reports are not configured (BUG_REPORTS_GITHUB_TOKEN unset).";
    pub const SINK_FAILED: &str = "Failed to create GitHub issue";
}

/// `POST /api/bug-reports` request body.
///
/// `pagePath` is required after trim; every other field is optional. Missing
/// `pagePath` deserialises as `""` (via `default`) so the handler can return
/// 400 with [`error_strings::PAGE_PATH_REQUIRED`] instead of axum's 422.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBugReportRequest {
    #[serde(default)]
    pub page_path: String,
    #[serde(default)]
    pub server_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub toasts: Option<Vec<String>>,
    #[serde(default)]
    pub ws_errors: Option<Vec<String>>,
    #[serde(default)]
    pub release: Option<String>,
    /// Optional seat-context bag for Music Bot / Voice / Sidecar (tags + log tails).
    /// Values may be strings or nested JSON; the server stringifies and caps them.
    #[serde(default)]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

/// `POST /api/bug-reports` 201 response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBugReportResponse {
    pub issue_url: String,
    pub issue_number: i64,
}

/// Request after trim / length caps. Built by [`CreateBugReportRequest::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedBugReport {
    pub page_path: String,
    pub server_id: Option<i64>,
    pub note: Option<String>,
    pub toasts: Vec<String>,
    pub ws_errors: Vec<String>,
    pub release: Option<String>,
    /// Sanitised `context` entries as `(key, rendered_value)` in request order.
    pub context: Vec<(String, String)>,
}

impl CreateBugReportRequest {
    /// Validate required fields and apply the documented caps.
    ///
    /// - `pagePath` must be non-empty after trim and ≤ [`MAX_PAGE_PATH_LEN`].
    /// - `note` / `release` / list items are truncated (report still accepted).
    /// - `toasts` / `wsErrors` keep at most [`MAX_LIST_ITEMS`] items.
    pub fn validate(&self) -> Result<ValidatedBugReport, &'static str> {
        let page_path = self.page_path.trim().to_string();
        if page_path.is_empty() {
            return Err(error_strings::PAGE_PATH_REQUIRED);
        }
        if page_path.len() > MAX_PAGE_PATH_LEN {
            return Err(error_strings::PAGE_PATH_TOO_LONG);
        }

        Ok(ValidatedBugReport {
            page_path,
            server_id: self.server_id,
            note: truncate_opt(self.note.as_deref(), MAX_NOTE_LEN),
            toasts: cap_list(self.toasts.as_deref()),
            ws_errors: cap_list(self.ws_errors.as_deref()),
            release: truncate_opt(self.release.as_deref(), MAX_RELEASE_LEN),
            context: cap_context(self.context.as_ref()),
        })
    }
}

fn truncate_opt(raw: Option<&str>, max: usize) -> Option<String> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty())?;
    Some(truncate_chars(trimmed, max))
}

fn cap_list(raw: Option<&[String]>) -> Vec<String> {
    raw.unwrap_or(&[])
        .iter()
        .filter_map(|item| {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_chars(trimmed, MAX_LIST_ITEM_LEN))
            }
        })
        .take(MAX_LIST_ITEMS)
        .collect()
}

fn cap_context(raw: Option<&serde_json::Map<String, serde_json::Value>>) -> Vec<(String, String)> {
    let Some(map) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut used = 0usize;
    for (key, value) in map {
        if out.len() >= MAX_CONTEXT_KEYS {
            break;
        }
        let key = truncate_chars(key.trim(), MAX_CONTEXT_KEY_LEN);
        if key.is_empty() {
            continue;
        }
        let rendered = render_context_value(value);
        let rendered = truncate_chars(&rendered, MAX_CONTEXT_VALUE_LEN);
        if used.saturating_add(rendered.len()) > MAX_CONTEXT_BYTES {
            break;
        }
        used += rendered.len();
        out.push((key, rendered));
    }
    out
}

fn render_context_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
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

    #[test]
    fn request_deserialises_camel_case_keys() {
        let json = r#"{
            "pagePath": "/music-bots/42",
            "serverId": 1,
            "note": "optional operator text",
            "toasts": ["Failed to fetch"],
            "wsErrors": ["SSE closed"],
            "release": "v1.6.9"
        }"#;
        let req: CreateBugReportRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.page_path, "/music-bots/42");
        assert_eq!(req.server_id, Some(1));
        assert_eq!(req.note.as_deref(), Some("optional operator text"));
        assert_eq!(
            req.toasts.as_deref(),
            Some([String::from("Failed to fetch")].as_slice())
        );
        assert_eq!(
            req.ws_errors.as_deref(),
            Some([String::from("SSE closed")].as_slice())
        );
        assert_eq!(req.release.as_deref(), Some("v1.6.9"));
    }

    #[test]
    fn request_serialises_camel_case_keys() {
        let req = CreateBugReportRequest {
            page_path: "/music-bots/42".into(),
            server_id: Some(1),
            note: Some("n".into()),
            toasts: Some(vec!["t".into()]),
            ws_errors: Some(vec!["w".into()]),
            release: Some("v1.6.9".into()),
            context: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        for key in [
            "pagePath", "serverId", "toasts", "wsErrors", "release", "note",
        ] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing {key}: {json}"
            );
        }
        for forbidden in ["page_path", "server_id", "ws_errors"] {
            assert!(
                !json.contains(forbidden),
                "leaked snake_case {forbidden}: {json}"
            );
        }
    }

    #[test]
    fn response_serialises_camel_case_keys() {
        let resp = CreateBugReportResponse {
            issue_url: "https://github.com/o/r/issues/7".into(),
            issue_number: 7,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""issueUrl":"https://github.com/o/r/issues/7""#));
        assert!(json.contains(r#""issueNumber":7"#));
        assert!(!json.contains("issue_url"));
        assert!(!json.contains("issue_number"));
    }

    #[test]
    fn missing_page_path_deserialises_as_empty_and_fails_validate() {
        let req: CreateBugReportRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.page_path, "");
        assert_eq!(
            req.validate().unwrap_err(),
            error_strings::PAGE_PATH_REQUIRED
        );
    }

    #[test]
    fn whitespace_page_path_fails_validate() {
        let req = CreateBugReportRequest {
            page_path: "   ".into(),
            ..Default::default()
        };
        assert_eq!(
            req.validate().unwrap_err(),
            error_strings::PAGE_PATH_REQUIRED
        );
    }

    #[test]
    fn too_long_page_path_fails_validate() {
        let req = CreateBugReportRequest {
            page_path: "x".repeat(MAX_PAGE_PATH_LEN + 1),
            ..Default::default()
        };
        assert_eq!(
            req.validate().unwrap_err(),
            error_strings::PAGE_PATH_TOO_LONG
        );
    }

    #[test]
    fn validate_trims_and_caps_optional_fields() {
        let req = CreateBugReportRequest {
            page_path: "  /logs  ".into(),
            server_id: Some(9),
            note: Some(format!("  {}  ", "n".repeat(MAX_NOTE_LEN + 8))),
            toasts: Some(
                (0..MAX_LIST_ITEMS + 5)
                    .map(|i| format!("toast-{i}"))
                    .collect(),
            ),
            ws_errors: Some(vec![
                "  ".into(),
                "ok".into(),
                "x".repeat(MAX_LIST_ITEM_LEN + 3),
            ]),
            release: Some(format!("  {}  ", "r".repeat(MAX_RELEASE_LEN + 2))),
            context: Some(serde_json::Map::from_iter([
                (
                    "musicBotLatency".into(),
                    serde_json::json!("resolve=20s retry=1"),
                ),
                ("nested".into(), serde_json::json!({"retry": 1})),
            ])),
        };
        let v = req.validate().unwrap();
        assert_eq!(v.page_path, "/logs");
        assert_eq!(v.server_id, Some(9));
        assert_eq!(v.note.as_ref().unwrap().chars().count(), MAX_NOTE_LEN);
        assert_eq!(v.toasts.len(), MAX_LIST_ITEMS);
        assert_eq!(v.ws_errors.len(), 2);
        assert_eq!(v.ws_errors[0], "ok");
        assert_eq!(v.ws_errors[1].chars().count(), MAX_LIST_ITEM_LEN);
        assert_eq!(v.release.as_ref().unwrap().chars().count(), MAX_RELEASE_LEN);
        assert_eq!(
            v.context,
            vec![
                ("musicBotLatency".into(), "resolve=20s retry=1".into()),
                ("nested".into(), r#"{"retry":1}"#.into()),
            ]
        );
    }

    #[test]
    fn request_deserialises_context_object() {
        let json = r#"{
            "pagePath": "/music-bots/42",
            "context": {
                "musicBotLatency": "resolve=20s retry=1",
                "logTail": "last line"
            }
        }"#;
        let req: CreateBugReportRequest = serde_json::from_str(json).unwrap();
        let ctx = req.context.as_ref().expect("context");
        assert_eq!(
            ctx.get("musicBotLatency").and_then(|v| v.as_str()),
            Some("resolve=20s retry=1")
        );
        let v = req.validate().unwrap();
        assert_eq!(v.context.len(), 2);
    }
}
