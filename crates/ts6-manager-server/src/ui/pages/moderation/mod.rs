//! `/moderation/*` + `/admin/permissions` — Phase 9.0 operator surfaces
//! ([PURA-287](/PURA/issues/PURA-287), workstream `9.0-ui`).
//!
//! Four route components, all backed by the `/api/moderation/*` REST
//! surface (PURA-286 / PURA-289) and the `moderation.*` RBAC catalog
//! (PURA-284):
//!
//! - [`ModerationQueuePage`] — `/moderation`. The operator's landing
//!   surface: the open-case queue plus the live TS6 complaint queue for
//!   the selected server.
//! - [`ModerationCasePage`] — `/moderation/cases/{id}`. Case detail, the
//!   append-only action timeline, and the action composer (note / kick /
//!   mute / ban / resolve / reopen). The IP-ban escalation lives here
//!   behind an explicit collateral-damage warning.
//! - [`SubjectHistoryPage`] — `/moderation/subjects/{uid}`. Every case,
//!   action, and note for one subject UID, plus the add-note form.
//! - [`PermissionGrantsPage`] — `/admin/permissions`. The per-user
//!   `moderation.*` grant editor (admin-only).
//!
//! ## Gating
//!
//! Two layers, both **visual** — the API is the real boundary:
//!
//! - *Page-level role gate*: `/moderation/*` admits `admin` + `moderator`
//!   ([`perm::role_can_moderate`]); `/admin/permissions` admits `admin`
//!   only. A non-qualifying session lands on an in-page 403 surface
//!   ([`AccessDenied`]) rather than a doomed fetch loop.
//! - *Action-level permission gate*: each write affordance is suppressed
//!   unless the role holds the matching catalog permission
//!   ([`perm::role_holds`]). The server `RequirePermission` extractor
//!   re-checks every call regardless.

mod case_detail;
mod grants;
mod history;
pub(crate) mod perm;
mod queue;

pub use case_detail::ModerationCasePage;
pub use grants::PermissionGrantsPage;
pub use history::SubjectHistoryPage;
pub use queue::ModerationQueuePage;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;

use crate::client::api::ApiError;

// ── shared error formatting ─────────────────────────────────────────────

/// Render an [`ApiError`] as a single operator-facing line. Mirrors the
/// per-page formatters on the control surfaces; the BadGateway arm keeps
/// the upstream TS6 code/detail because the moderation routes pass those
/// through verbatim.
pub(crate) fn format_error(err: &ApiError) -> String {
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
                s.push_str(&format!(" ({c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired — sign in again.".into(),
        ApiError::SessionAnonymous => "Loading…".into(),
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Not available in this view.".into(),
    }
}

// ── shared time formatting ──────────────────────────────────────────────

/// Absolute UTC timestamp, minute precision — `2026-05-16 14:30 UTC`. Used
/// in the dense table cells where a relative label would be ambiguous.
pub(crate) fn fmt_datetime(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Coarse relative label — `just now`, `12m ago`, `3h ago`, `5d ago`, then
/// falls back to the absolute date past a week. Drives the timeline rows
/// where recency matters more than the exact instant.
pub(crate) fn relative_when(ts: DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 0 {
        // Clock skew between browser and server — don't render "−3s ago".
        return fmt_datetime(ts);
    }
    if secs < 60 {
        return "just now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    ts.format("%Y-%m-%d").to_string()
}

/// Unix-seconds (the TS6 `complainlist` `timestamp` wire field) → relative
/// label, reusing [`relative_when`].
pub(crate) fn relative_from_unix(secs: i64) -> String {
    match DateTime::from_timestamp(secs, 0) {
        Some(ts) => relative_when(ts),
        None => "unknown".into(),
    }
}

// ── shared status / kind presentation ───────────────────────────────────

/// CSS modifier class for a case `status` (`open` / `actioned` /
/// `resolved`). Unknown values fall back to the neutral base.
pub(crate) fn case_status_class(status: &str) -> &'static str {
    match status {
        "open" => "mod-badge mod-badge--open",
        "actioned" => "mod-badge mod-badge--actioned",
        "resolved" => "mod-badge mod-badge--resolved",
        _ => "mod-badge",
    }
}

/// Icon glyph for a timeline action `kind`.
pub(crate) fn action_kind_icon(kind: &str) -> &'static str {
    match kind {
        "kick" => "⊗",
        "ban" => "⊘",
        "ban_ip" => "⊘",
        "mute" => "🔇",
        "unmute" => "🔊",
        "note" => "✎",
        "resolve" => "✓",
        "reopen" => "↺",
        _ => "•",
    }
}

/// Human label for a timeline action `kind`.
pub(crate) fn action_kind_label(kind: &str) -> &'static str {
    match kind {
        "kick" => "Kicked",
        "ban" => "Banned",
        "ban_ip" => "Banned (IP)",
        "mute" => "Muted",
        "unmute" => "Unmuted",
        "note" => "Note",
        "resolve" => "Resolved",
        "reopen" => "Reopened",
        _ => "Action",
    }
}

/// Human label for a case `origin` (`operator` / `complaint` / `automod`).
pub(crate) fn origin_label(origin: &str) -> &'static str {
    match origin {
        "operator" => "Operator-opened",
        "complaint" => "From complaint",
        "automod" => "Auto-moderation",
        _ => "Unknown origin",
    }
}

// ── shared in-page 403 surface ──────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub(crate) struct AccessDeniedProps {
    /// Breadcrumb text, e.g. `"Moderation"` or `"Admin · Permissions"`.
    crumb: String,
    /// Page `<h1>` text.
    heading: String,
    /// One-line explanation of which role is required.
    detail: String,
}

/// The in-page 403 surface a non-qualifying session lands on when it
/// deep-links a gated route. The nav entry is also hidden for these
/// sessions; this is the belt-and-braces surface for a forged URL. The
/// API re-checks the role/permission regardless — this is cosmetic.
#[component]
pub(crate) fn AccessDenied(props: AccessDeniedProps) -> Element {
    rsx! {
        div { class: "crumb", "{props.crumb}" }
        h1 { "{props.heading}" }
        div { class: "empty",
            div { class: "icon", "⛔" }
            h3 { "Insufficient permissions" }
            p { "{props.detail}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn relative_when_buckets_by_age() {
        let now = Utc::now();
        assert_eq!(relative_when(now), "just now");
        assert_eq!(relative_when(now - Duration::minutes(12)), "12m ago");
        assert_eq!(relative_when(now - Duration::hours(3)), "3h ago");
        assert_eq!(relative_when(now - Duration::days(5)), "5d ago");
        // Past a week → absolute date, not "9d ago".
        assert!(relative_when(now - Duration::days(40)).starts_with("20"));
    }

    #[test]
    fn relative_when_does_not_render_negative_for_clock_skew() {
        let future = Utc::now() + Duration::minutes(5);
        // A future timestamp degrades to the absolute form, never "−5m ago".
        assert!(!relative_when(future).contains("ago"));
    }

    #[test]
    fn case_status_class_falls_back_for_unknown() {
        assert_eq!(case_status_class("open"), "mod-badge mod-badge--open");
        assert_eq!(case_status_class("weird"), "mod-badge");
    }

    #[test]
    fn relative_from_unix_handles_bad_timestamp() {
        assert_eq!(relative_from_unix(i64::MAX), "unknown");
    }
}
