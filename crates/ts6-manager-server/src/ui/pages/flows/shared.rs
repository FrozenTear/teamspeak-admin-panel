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

/// Human label for an [`wire::ActionResult.kind`] wire discriminant. The
/// per-run results panel (`ui-brief.md` §3.3) only has the camelCase
/// string the engine emitted, not the typed [`wire::Action`], so this
/// maps it back to the same copy [`action_kind_label`] produces.
pub fn action_wire_kind_label(kind: &str) -> &str {
    match kind {
        "ts6Command" => "TS6 command",
        "musicBotCommand" => "Music-bot command",
        "webhookOut" => "Webhook",
        "logLine" => "Log line",
        // An unrecognised kind is surfaced verbatim rather than hidden —
        // a wire/engine drift should be visible, not silently relabelled.
        other => other,
    }
}

/// Operator-facing label for a per-action [`wire::ActionStatus`].
pub fn action_status_label(status: wire::ActionStatus) -> &'static str {
    match status {
        wire::ActionStatus::Ok => "Ok",
        wire::ActionStatus::Errored => "Errored",
        wire::ActionStatus::Skipped => "Skipped",
    }
}

/// Pill class for a per-action [`wire::ActionStatus`] — same `bot-badge`
/// vocabulary as [`run_status_badge_class`], so a failed action reads as
/// danger and the benign outcomes stay neutral.
pub fn action_status_badge_class(status: wire::ActionStatus) -> &'static str {
    match status {
        wire::ActionStatus::Ok => "bot-badge bot-badge--play",
        wire::ActionStatus::Errored => "bot-badge bot-badge--error",
        wire::ActionStatus::Skipped => "bot-badge bot-badge--off",
    }
}

// ── Flow glyph icons (PURA-253 §7.1, resolved by UXDesigner) ──────────
// Decorative glyphs paired with a text label everywhere they render, so
// each surface stays colour- and icon-independent (WCAG 1.4.1). Markup
// wraps them in `span.flow-icon[aria-hidden=true]`.

/// Decorative glyph for an action kind — paired with [`action_kind_label`]
/// in the actions-list cards and the Definition tab. PURA-253 H2.
pub fn action_kind_icon(action: &wire::Action) -> &'static str {
    match action {
        wire::Action::Ts6Command { .. } => "»",
        wire::Action::MusicBotCommand { .. } => "♪",
        wire::Action::WebhookOut { .. } => "↗",
        wire::Action::LogLine { .. } => "≡",
    }
}

/// Glyph variant for the per-run action-results drawer, which only has the
/// camelCase wire discriminant. Mirrors [`action_wire_kind_label`].
pub fn action_wire_kind_icon(kind: &str) -> &'static str {
    match kind {
        "ts6Command" => "»",
        "musicBotCommand" => "♪",
        "webhookOut" => "↗",
        "logLine" => "≡",
        // Unknown kind — the label is still shown verbatim alongside.
        _ => "•",
    }
}

/// Decorative status glyph — paired with [`run_status_label`]. Hardens the
/// colour-blind path and disambiguates Interrupted from Skipped, which
/// share the neutral pill colour. PURA-253 H2.
pub fn run_status_icon(status: wire::FlowRunStatus) -> &'static str {
    match status {
        wire::FlowRunStatus::InFlight => "⟳",
        wire::FlowRunStatus::Ok => "✓",
        wire::FlowRunStatus::Errored => "✕",
        wire::FlowRunStatus::Interrupted => "‖",
        wire::FlowRunStatus::SkippedDisabled => "↷",
    }
}

/// Per-action status glyph — the `Ok`/`Errored`/`Skipped` subset of the
/// run-status set, paired with [`action_status_label`].
pub fn action_status_icon(status: wire::ActionStatus) -> &'static str {
    match status {
        wire::ActionStatus::Ok => "✓",
        wire::ActionStatus::Errored => "✕",
        wire::ActionStatus::Skipped => "↷",
    }
}

/// L2-tail — single source for the key/value + header editor remove glyph,
/// so those controls source one registry rather than a bare literal.
pub const REMOVE_GLYPH: &str = "×";

/// `ui-brief.md` §2 cron preset chips. Display label → cron expression.
pub const CRON_PRESETS: &[(&str, &str)] = &[
    ("every 5 min", "0 */5 * * * *"),
    ("hourly", "0 0 * * * *"),
    ("daily at noon UTC", "0 0 12 * * *"),
];

/// `ui-brief.md` §3.2 hard cap on actions per flow.
pub const MAX_ACTIONS: usize = 8;

/// PURA-248 M5 — tooltip shown on a write-action control when a read-only
/// (non-admin) operator hovers it. The route layer is the real gate; this
/// just explains the disabled state up front instead of after a 403.
pub const ADMIN_ONLY_HINT: &str = "Admin-only. Ask a server admin to make changes.";

/// `Some(hint)` only when the operator is not an admin — so a control's
/// `title` prop stays `None` (no tooltip) for admins.
pub fn admin_only_title(is_admin: bool) -> Option<String> {
    (!is_admin).then(|| ADMIN_ONLY_HINT.to_string())
}

/// `ui-brief.md` §3.2 — name field is 120 chars max.
pub const MAX_NAME_LEN: usize = 120;

/// Field count the engine's cron dialect expects — the `cron` crate
/// (`docs/flows/architecture.md` open questions) parses a 6-field
/// expression (`second minute hour day-of-month month day-of-week`) and
/// also accepts an optional 7th `year` field.
pub const CRON_MIN_FIELDS: usize = 6;
pub const CRON_MAX_FIELDS: usize = 7;

/// Live, client-side sanity check for the cron input (`ui-brief.md` §3.2).
///
/// A *non-authoritative* field-count heuristic — the server does the real
/// dialect parse and owns the 400. The point is to catch the most common
/// operator slip (a 5-field "standard" cron with no seconds field) before
/// a round-trip. Returns `None` when the expression is empty (the
/// required-field check on submit owns that case) or the field count is
/// plausible.
pub fn cron_validation_message(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return None;
    }
    let fields = trimmed.split_whitespace().count();
    if fields < CRON_MIN_FIELDS {
        Some(format!(
            "Looks short — this engine expects {CRON_MIN_FIELDS} fields \
             (second minute hour day month weekday). Got {fields}."
        ))
    } else if fields > CRON_MAX_FIELDS {
        Some(format!(
            "Looks long — expected {CRON_MIN_FIELDS} fields plus an optional \
             year ({CRON_MAX_FIELDS} max). Got {fields}."
        ))
    } else {
        None
    }
}

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

    #[test]
    fn action_wire_kind_icon_matches_known_kinds_and_falls_back() {
        // The typed-enum helpers are exhaustive by the compiler; the
        // wire-discriminant match over `&str` is not, so pin it.
        assert_eq!(action_wire_kind_icon("ts6Command"), "»");
        assert_eq!(action_wire_kind_icon("musicBotCommand"), "♪");
        assert_eq!(action_wire_kind_icon("webhookOut"), "↗");
        assert_eq!(action_wire_kind_icon("logLine"), "≡");
        // An unknown discriminant gets a neutral dot — never empty.
        assert_eq!(action_wire_kind_icon("somethingNew"), "•");
        assert!(!action_wire_kind_icon("").is_empty());
    }

    #[test]
    fn cron_validation_flags_short_and_long_expressions() {
        // Empty is the required-field check's job, not ours.
        assert_eq!(cron_validation_message(""), None);
        assert_eq!(cron_validation_message("   "), None);
        // The classic slip: a 5-field standard cron, no seconds.
        assert!(cron_validation_message("*/5 * * * *").is_some());
        // A well-formed 6-field expression clears the check.
        assert_eq!(cron_validation_message("0 */5 * * * *"), None);
        // 7 fields (with year) is still accepted by the `cron` crate.
        assert_eq!(cron_validation_message("0 0 12 * * * 2026"), None);
        // 8+ fields is over the cap.
        assert!(cron_validation_message("0 0 12 * * * 2026 extra").is_some());
        // Every preset must pass its own validation.
        for (_, expr) in CRON_PRESETS {
            assert_eq!(cron_validation_message(expr), None, "preset {expr}");
        }
    }
}
