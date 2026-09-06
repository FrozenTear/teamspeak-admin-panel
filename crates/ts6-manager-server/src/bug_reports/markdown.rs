//! Title + markdown body for the private GitHub Issue.
//!
//! Operator-authored strings (`note`, toast / WS error text, `pagePath`)
//! are escaped so they cannot break out of inline/code fences or inject
//! headings. Obvious secret-shaped tokens are redacted — we never attach
//! JWTs, GitHub PATs, or `Authorization` values.

use chrono::{DateTime, Utc};
use ts6_manager_shared::bug_reports::ValidatedBugReport;

const TITLE_MAX: usize = 256;
const NOTE_PREFIX_MAX: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDraft {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Reporter {
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

pub fn build_issue(
    report: &ValidatedBugReport,
    reporter: &Reporter,
    submitted_at: DateTime<Utc>,
) -> IssueDraft {
    IssueDraft {
        title: build_title(&report.page_path, report.note.as_deref()),
        body: build_body(report, reporter, submitted_at),
    }
}

fn build_title(page_path: &str, note: Option<&str>) -> String {
    let mut title = format!("[bug-report] {page_path}");
    if let Some(note) = note {
        let prefix = note
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(NOTE_PREFIX_MAX)
            .collect::<String>();
        if !prefix.is_empty() {
            title.push_str(" — ");
            title.push_str(&prefix);
        }
    }
    truncate_chars(&title, TITLE_MAX)
}

fn build_body(
    report: &ValidatedBugReport,
    reporter: &Reporter,
    submitted_at: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    out.push_str("## Operator bug report\n\n");
    out.push_str("| Field | Value |\n| --- | --- |\n");
    out.push_str(&row(
        "Reporter",
        &format!(
            "{} (`{}`, id {})",
            escape_cell(&reporter.display_name),
            escape_cell(&reporter.username),
            reporter.id
        ),
    ));
    out.push_str(&row("pagePath", &inline_code(&report.page_path)));
    out.push_str(&row(
        "serverId",
        &report
            .server_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "_none_".into()),
    ));
    out.push_str(&row(
        "release",
        &report
            .release
            .as_deref()
            .map(inline_code)
            .unwrap_or_else(|| "_none_".into()),
    ));
    out.push_str(&row("submittedAt", &submitted_at.to_rfc3339()));
    out.push('\n');

    out.push_str("### Note\n\n");
    match report.note.as_deref() {
        Some(note) => {
            out.push_str(&fenced(sanitize_text(note)));
            out.push('\n');
        }
        None => out.push_str("_none_\n"),
    }

    out.push_str("\n### Toasts\n\n");
    append_list(&mut out, &report.toasts);

    out.push_str("\n### WS / SSE errors\n\n");
    append_list(&mut out, &report.ws_errors);

    out.push_str("\n### Context\n\n");
    if report.context.is_empty() {
        out.push_str("_none_\n");
    } else {
        for (key, value) in &report.context {
            out.push_str(&format!("**{}**\n\n", escape_cell(key)));
            out.push_str(&fenced(sanitize_text(value)));
            out.push_str("\n\n");
        }
    }

    out
}

fn append_list(out: &mut String, items: &[String]) {
    if items.is_empty() {
        out.push_str("_none_\n");
        return;
    }
    for item in items {
        out.push_str(&fenced(sanitize_text(item)));
        out.push('\n');
    }
}

fn row(key: &str, value: &str) -> String {
    format!("| {key} | {value} |\n")
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn inline_code(s: &str) -> String {
    // Break any run of backticks so the value cannot close the span.
    let escaped = sanitize_text(s).replace('`', "'");
    format!("`{escaped}`")
}

fn fenced(s: String) -> String {
    // Pick a fence longer than any backtick run in the payload.
    let mut longest = 3;
    let mut run = 0;
    for ch in s.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run + 1);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest.max(3));
    format!("{fence}\n{s}\n{fence}")
}

/// Redact secret-shaped tokens. Applied before any markdown wrapping.
fn sanitize_text(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut out = Vec::with_capacity(tokens.len());
    let mut redact_next = false;
    for tok in tokens {
        let trimmed = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | ')'));
        let lower = trimmed.to_ascii_lowercase();
        if redact_next || looks_secret(trimmed) {
            out.push("[redacted]");
            redact_next = false;
            continue;
        }
        if lower == "bearer" || lower == "authorization:" {
            out.push("[redacted]");
            redact_next = true;
            continue;
        }
        out.push(tok);
    }
    out.join(" ").replace('\0', "")
}

fn looks_secret(tok: &str) -> bool {
    let lower = tok.to_ascii_lowercase();
    if lower.starts_with("bearer ")
        || lower.contains("authorization:")
        || tok.starts_with("ghp_")
        || tok.starts_with("github_pat_")
        || tok.starts_with("gho_")
        || tok.starts_with("ghu_")
        || tok.starts_with("ghs_")
        || tok.starts_with("ghr_")
    {
        return true;
    }
    // Compact JWT: three base64url segments starting with `eyJ`.
    let parts: Vec<&str> = tok.split('.').collect();
    parts.len() == 3 && parts[0].starts_with("eyJ") && parts.iter().all(|p| !p.is_empty())
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
    use chrono::TimeZone;
    use ts6_manager_shared::bug_reports::ValidatedBugReport;

    fn sample_report() -> ValidatedBugReport {
        ValidatedBugReport {
            page_path: "/music-bots/42".into(),
            server_id: Some(1),
            note: Some("optional operator text".into()),
            toasts: vec!["Failed to fetch".into()],
            ws_errors: vec!["SSE closed".into()],
            release: Some("v1.6.9".into()),
            context: vec![("musicBotLatency".into(), "resolve=20s retry=1".into())],
        }
    }

    fn reporter() -> Reporter {
        Reporter {
            id: 7,
            username: "robert".into(),
            display_name: "Robert".into(),
        }
    }

    #[test]
    fn title_includes_path_and_note_prefix() {
        let t = build_title("/music-bots/42", Some("optional operator text"));
        assert_eq!(t, "[bug-report] /music-bots/42 — optional operator text");
    }

    #[test]
    fn title_without_note_is_path_only() {
        assert_eq!(build_title("/logs", None), "[bug-report] /logs");
    }

    #[test]
    fn body_includes_reporter_and_fields() {
        let at = Utc.with_ymd_and_hms(2026, 9, 6, 18, 0, 0).unwrap();
        let draft = build_issue(&sample_report(), &reporter(), at);
        assert!(draft.body.contains("Robert"));
        assert!(draft.body.contains("`robert`"));
        assert!(draft.body.contains("id 7"));
        assert!(draft.body.contains("`/music-bots/42`"));
        assert!(draft.body.contains("| serverId | 1 |"));
        assert!(draft.body.contains("`v1.6.9`"));
        assert!(draft.body.contains("optional operator text"));
        assert!(draft.body.contains("Failed to fetch"));
        assert!(draft.body.contains("SSE closed"));
        assert!(draft.body.contains("2026-09-06T18:00:00+00:00"));
        assert!(draft.body.contains("**musicBotLatency**"));
        assert!(draft.body.contains("resolve=20s retry=1"));
    }

    #[test]
    fn note_cannot_close_fence_or_inject_heading() {
        let mut report = sample_report();
        report.note = Some("```\n# injected\n```".into());
        let draft = build_issue(&report, &reporter(), Utc::now());
        // Sanitiser collapses whitespace; leftover backticks must sit
        // inside a longer fence, never as a lone closer.
        assert!(draft.body.contains("````"));
        assert!(!draft.body.contains("\n# injected\n"));
    }

    #[test]
    fn table_cell_escapes_pipes() {
        let reporter = Reporter {
            id: 1,
            username: "a|b".into(),
            display_name: "x|y".into(),
        };
        let draft = build_issue(&sample_report(), &reporter, Utc::now());
        assert!(draft.body.contains("x\\|y"));
        assert!(draft.body.contains("a\\|b"));
    }

    #[test]
    fn jwt_and_github_tokens_are_redacted() {
        let mut report = sample_report();
        report.note = Some(
            "got eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig and ghp_abcdefghijklmnopqrstuvwxyz012345"
                .into(),
        );
        report.toasts = vec!["Authorization: Bearer abc".into()];
        let draft = build_issue(&report, &reporter(), Utc::now());
        assert!(!draft.body.contains("eyJ"));
        assert!(!draft.body.contains("ghp_"));
        assert!(draft.body.contains("[redacted]"));
    }
}
