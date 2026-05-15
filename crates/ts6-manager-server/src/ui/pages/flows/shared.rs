//! Shared helpers for the flow pages — pill class derivation, error
//! formatting, action-kind copy, relative time, cron presets.

use chrono::{DateTime, Utc};
use ts6_manager_shared::flows as wire;

use crate::client::api::ApiError;

/// Map a flow's enabled-state to a pill class. Reuses the `bot-badge`
/// vocabulary from the music-bot pages so the operator sees one shared
/// design language across the manager.
pub fn enabled_badge_class(enabled: bool) -> &'static str {
    if enabled {
        "bot-badge bot-badge--idle"
    } else {
        "bot-badge bot-badge--off"
    }
}

/// Operator-facing label for the enabled-state pill.
pub fn enabled_label(enabled: bool) -> &'static str {
    if enabled { "Enabled" } else { "Disabled" }
}

/// Map a [`wire::FlowRunStatus`] to a pill class. The dot legend on the
/// detail page renders the same vocabulary.
///
/// PURA-246 B2 — colour must not collapse a *failure* (`Errored`) into the
/// same neutral bucket as the two benign terminal outcomes. The Runs tab
/// exists to answer "did it work?", so `Errored` carries a distinct danger
/// token; `Interrupted`/`SkippedDisabled` stay neutral. Text labels (see
/// [`run_status_label`]) keep the surface colour-independent.
pub fn run_status_badge_class(status: wire::FlowRunStatus) -> &'static str {
    match status {
        wire::FlowRunStatus::InFlight => "bot-badge bot-badge--pending",
        wire::FlowRunStatus::Ok => "bot-badge bot-badge--play",
        wire::FlowRunStatus::Errored => "bot-badge bot-badge--error",
        wire::FlowRunStatus::Interrupted => "bot-badge bot-badge--off",
        wire::FlowRunStatus::SkippedDisabled => "bot-badge bot-badge--off",
    }
}

/// Operator-facing label for a run status.
pub fn run_status_label(status: wire::FlowRunStatus) -> &'static str {
    match status {
        wire::FlowRunStatus::InFlight => "Running",
        wire::FlowRunStatus::Ok => "Ok",
        wire::FlowRunStatus::Errored => "Errored",
        wire::FlowRunStatus::Interrupted => "Interrupted",
        // `ui-brief.md` §6.2 — the engine reuses `skipped_disabled` for
        // "a run was already in flight". The label leans on that hint.
        wire::FlowRunStatus::SkippedDisabled => "Skipped",
    }
}

/// Tooltip explainer for the `skipped_disabled` status — flagged in the
/// brief as a footgun operators need surface copy for.
pub fn run_status_hint(status: wire::FlowRunStatus) -> Option<&'static str> {
    match status {
        wire::FlowRunStatus::SkippedDisabled => {
            Some("A run was already in flight when this trigger fired — the new run was dropped.")
        }
        _ => None,
    }
}

/// Short, one-line summary of the trigger for the list page.
pub fn trigger_summary(trigger: &wire::Trigger) -> String {
    match trigger {
        wire::Trigger::Cron { expression } => format!("cron `{expression}`"),
        wire::Trigger::ManualFire => "manual fire".into(),
        wire::Trigger::Ts6ClientJoined { channel_id: None } => {
            "ts6 client joined (any channel)".into()
        }
        wire::Trigger::Ts6ClientJoined {
            channel_id: Some(c),
        } => format!("ts6 client joined (channel {c})"),
    }
}

/// One-word label for an action kind. Used inside the actions-list cards
/// on the form and the read-only Definition tab.
pub fn action_kind_label(action: &wire::Action) -> &'static str {
    match action {
        wire::Action::Ts6Command { .. } => "TS6 command",
        wire::Action::MusicBotCommand { .. } => "Music-bot command",
        wire::Action::WebhookOut { .. } => "Webhook",
        wire::Action::LogLine { .. } => "Log line",
    }
}

/// `ui-brief.md` §2 cron preset chips. Display label → cron expression.
pub const CRON_PRESETS: &[(&str, &str)] = &[
    ("every 5 min", "0 */5 * * * *"),
    ("hourly", "0 0 * * * *"),
    ("daily at noon UTC", "0 0 12 * * *"),
];

/// `ui-brief.md` §3.2 hard cap on actions per flow.
pub const MAX_ACTIONS: usize = 8;

/// `ui-brief.md` §3.2 — name field is 120 chars max.
pub const MAX_NAME_LEN: usize = 120;

/// Convert an [`ApiError`] into the operator-facing message banners +
/// toasts use. Mirrors `music_bots::shared::format_error` so the FE
/// renders errors with the same vocabulary.
pub fn format_error(err: &ApiError) -> String {
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
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::Client { status, message } => match status {
            403 => "Admin-only. Ask your server admin to make the change.".into(),
            429 => "Slow down — flows can only fire once every 2 s manually.".into(),
            _ => format!("{status}: {message}"),
        },
        ApiError::Server { status, message } => match status {
            503 => "Flow engine busy. Try again in a moment.".into(),
            _ => format!("{status}: {message}"),
        },
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Action unavailable in this view.".into(),
        // Session not yet rehydrated — the page is still mounting. Mirrors
        // `music_bots::shared::format_error` so the flows pages render the
        // same transient copy rather than an error banner.
        ApiError::SessionAnonymous => "Loading…".into(),
    }
}

/// `error == "run_in_flight"` indicates a delete needs `?force=true`. We
/// rely on `status == 409` plus a heuristic over the message because
/// `ApiError::Client` flattens the envelope's discriminant into `message`
/// upstream (see `client::api::classify_response`).
pub fn is_run_in_flight_conflict(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Client { status: 409, message } if message.contains("run_in_flight") || message.contains("in-flight")
    )
}

/// Render a "Last run" cell from a [`wire::FlowRunSummary`]. Empty when
/// no runs have happened yet.
pub fn last_run_cell(last: Option<&wire::FlowRunSummary>) -> String {
    let Some(s) = last else { return "—".into() };
    let when = relative_when(s.started_at);
    let status_word = match s.status {
        wire::FlowRunStatus::Ok => "ok",
        wire::FlowRunStatus::Errored => "errored",
        wire::FlowRunStatus::InFlight => "running",
        wire::FlowRunStatus::Interrupted => "interrupted",
        wire::FlowRunStatus::SkippedDisabled => "skipped",
    };
    if let Some(d) = s.duration_ms {
        format!("{when} — {status_word} ({d} ms)")
    } else {
        format!("{when} — {status_word}")
    }
}

/// Compact relative time ("5m ago", "2h ago", "just now"). Falls back to
/// an ISO timestamp for anything past 7 days. Avoid pulling
/// `chrono-humanize` in for this single use.
pub fn relative_when(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 0 {
        return "just now".into();
    }
    if secs < 60 {
        return "just now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days <= 7 {
        return format!("{days}d ago");
    }
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn enabled_label_matches_pill_state() {
        assert_eq!(enabled_label(true), "Enabled");
        assert_eq!(enabled_label(false), "Disabled");
    }

    #[test]
    fn trigger_summary_distinguishes_any_channel_from_specific_channel() {
        let any = trigger_summary(&wire::Trigger::Ts6ClientJoined { channel_id: None });
        let one = trigger_summary(&wire::Trigger::Ts6ClientJoined {
            channel_id: Some(5),
        });
        assert!(any.contains("any"), "got: {any}");
        assert!(one.contains("channel 5"), "got: {one}");
    }

    #[test]
    fn relative_when_buckets() {
        let now = Utc::now();
        assert_eq!(relative_when(now), "just now");
        assert_eq!(relative_when(now - Duration::seconds(30)), "just now");
        let r = relative_when(now - Duration::minutes(5));
        assert!(r.starts_with('5') && r.contains('m'), "got: {r}");
        let r = relative_when(now - Duration::hours(3));
        assert!(r.contains('h'), "got: {r}");
        let r = relative_when(now - Duration::days(2));
        assert!(r.contains('d'), "got: {r}");
    }

    #[test]
    fn run_in_flight_conflict_branches_on_status_and_message() {
        let err = ApiError::Client {
            status: 409,
            message: "run_in_flight: run 42 is in-flight".into(),
        };
        assert!(is_run_in_flight_conflict(&err));

        let err = ApiError::Client {
            status: 409,
            message: "name_taken".into(),
        };
        assert!(!is_run_in_flight_conflict(&err));

        let err = ApiError::Client {
            status: 400,
            message: "in-flight".into(),
        };
        assert!(!is_run_in_flight_conflict(&err));
    }

    #[test]
    fn last_run_cell_em_dash_when_never_run() {
        assert_eq!(last_run_cell(None), "—");
    }
}
