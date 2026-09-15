//! Title + markdown body for the private GitHub Issue.
//!
//! Operator-authored strings (`note`, toast / WS error text, `pagePath`)
//! are escaped so they cannot break out of inline/code fences or inject
//! headings. Obvious secret-shaped tokens are redacted — we never attach
//! JWTs, GitHub PATs, or `Authorization` values.
//!
//! Auto-attached seat marks (`frameUnderrun`, `connectedLoopStall`,
//! `musicBotLatency`, log tails, …) stay in the body for forensics. This
//! module only formats them: a short Summary rollup up top, and collapsed
//! tails instead of a mid-line wall of text.

use chrono::{DateTime, Utc};
use ts6_manager_shared::bug_reports::ValidatedBugReport;

const TITLE_MAX: usize = 256;
const TAIL_DISPLAY_LINES: usize = 12;
const TAIL_LINE_MAX: usize = 160;
const INLINE_VALUE_MAX: usize = 120;
const FULL_BUFFER_MIN: u64 = 200;

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
        title: build_title(report),
        body: build_body(report, reporter, submitted_at),
    }
}

fn build_title(report: &ValidatedBugReport) -> String {
    let mut head = format!("[bug-report] {}", report.page_path);
    if let Some(rel) = report
        .release
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        head.push_str(" · ");
        head.push_str(rel);
    }
    let sep_len = 3; // " — "
    let budget = TITLE_MAX
        .saturating_sub(head.chars().count())
        .saturating_sub(sep_len);
    match title_symptom(report, budget) {
        Some(s) if !s.is_empty() => truncate_at_word(&format!("{head} — {s}"), TITLE_MAX),
        _ => truncate_at_word(&head, TITLE_MAX),
    }
}

/// Prefer the operator note (release prefix stripped), else a mark rollup.
fn title_symptom(report: &ValidatedBugReport, budget: usize) -> Option<String> {
    if let Some(note) = report.note.as_deref() {
        let first = note.lines().next().unwrap_or("").trim();
        let stripped = strip_leading_release(first, report.release.as_deref());
        if !stripped.is_empty() {
            return Some(truncate_at_word(stripped, budget));
        }
    }
    let rollup = build_rollup(&report.context)?;
    Some(truncate_at_word(&rollup, budget))
}

fn strip_leading_release<'a>(note: &'a str, release: Option<&str>) -> &'a str {
    let Some(rel) = release.map(str::trim).filter(|s| !s.is_empty()) else {
        return note;
    };
    match note.strip_prefix(rel) {
        Some(rest) => rest
            .trim_start_matches([' ', '—', '-', '·', ':'])
            .trim_start(),
        None => note,
    }
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

    if let Some(rollup) = build_rollup(&report.context) {
        out.push_str("### Summary\n\n");
        out.push_str(&escape_cell(&rollup));
        out.push_str("\n\n");
    }

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
            out.push_str(&format_context_entry(key, value));
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

fn format_context_entry(key: &str, value: &str) -> String {
    let key_md = escape_cell(key);
    let sanitized = sanitize_text(value);
    let is_tail = is_tail_key(key);
    let multiline = sanitized.lines().filter(|l| !l.trim().is_empty()).count() > 1;
    let long = sanitized.chars().count() > INLINE_VALUE_MAX;
    if is_tail || multiline || long {
        let (caption, body) = collapse_tail(&sanitized);
        let mut s = format!("**{key_md}**");
        if let Some(cap) = caption {
            s.push_str(" — ");
            s.push_str(&cap);
        }
        s.push_str("\n\n");
        s.push_str(&fenced(body));
        s.push_str("\n\n");
        s
    } else {
        format!("**{key_md}** — {}\n\n", inline_code(&sanitized))
    }
}

fn is_tail_key(key: &str) -> bool {
    key == "musicBotLatency" || key.ends_with("Tail") || key.ends_with("Log")
}

fn collapse_tail(value: &str) -> (Option<String>, String) {
    let lines = split_log_lines(value);
    let capped: Vec<String> = lines
        .into_iter()
        .map(|l| truncate_at_word(&l, TAIL_LINE_MAX))
        .filter(|l| !l.is_empty())
        .collect();
    let total = capped.len();
    let (omitted, kept) = select_tail_lines(&capped, TAIL_DISPLAY_LINES);
    let mut body = String::new();
    if omitted > 0 {
        body.push_str(&format!("… {omitted} earlier line"));
        if omitted != 1 {
            body.push('s');
        }
        body.push_str(" omitted\n");
    }
    body.push_str(&kept.join("\n"));
    let caption = (omitted > 0).then(|| format!("_last {} of {total} lines_", kept.len()));
    (caption, body)
}

fn split_log_lines(value: &str) -> Vec<String> {
    let raw: Vec<String> = value
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut out = Vec::new();
    for line in raw {
        let split = resplit_joined_events(&line);
        if split.len() > 1 {
            out.extend(split);
        } else {
            out.push(line);
        }
    }
    out
}

/// Recover events that arrived as one whitespace-joined blob (or a
/// `sanitize_text` leftover that collapsed newlines).
fn resplit_joined_events(line: &str) -> Vec<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 2 {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 && is_event_starter(tok) && !cur.is_empty() {
            out.push(cur.join(" "));
            cur.clear();
        }
        cur.push(*tok);
    }
    if !cur.is_empty() {
        out.push(cur.join(" "));
    }
    if out.len() <= 1 {
        vec![line.to_string()]
    } else {
        out
    }
}

fn is_event_starter(tok: &str) -> bool {
    matches!(
        tok,
        "frame_underrun"
            | "connected_loop_stall"
            | "first_frame_on_wire"
            | "handshake_dropped"
            | "send_audio_error"
            | "encode_error"
            | "actor_panic"
            | "resolver_warm_retry"
            | "resolver_resolved"
            | "resolver_failed"
            | "music_bot_latency"
            | "audio_send_attribution"
            | "audio_send_summary"
            | "yt_dlp"
    )
}

fn is_high_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "underrun", "stall", "error", "warn", "fail", "panic", "reject", "timeout", "c_loop",
        "deferral", "crackle", "dropped",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Keep recent lines plus earlier high-signal ones, dropping the rest.
fn select_tail_lines(lines: &[String], keep: usize) -> (usize, Vec<String>) {
    if lines.len() <= keep {
        return (0, lines.to_vec());
    }
    let recent_n = (keep + 1) / 2;
    let recent_n = recent_n.min(lines.len());
    let split_at = lines.len() - recent_n;
    let recent = &lines[split_at..];
    let older = &lines[..split_at];
    let mut signal: Vec<String> = older
        .iter()
        .filter(|l| is_high_signal(l))
        .cloned()
        .collect();
    let budget = keep.saturating_sub(recent.len());
    if signal.len() > budget {
        signal = signal[signal.len() - budget..].to_vec();
    }
    let shown = signal.len() + recent.len();
    let omitted = lines.len().saturating_sub(shown);
    let mut out = signal;
    out.extend(recent.iter().cloned());
    (omitted, out)
}

fn build_rollup(context: &[(String, String)]) -> Option<String> {
    if context.is_empty() {
        return None;
    }
    let lookup = |key: &str| -> Option<&str> {
        context
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    let underruns = underrun_count(context);
    let buffered = lookup("frameUnderrun")
        .and_then(|v| parse_kv_u64(v, "buffered"))
        .or_else(|| {
            context.iter().find_map(|(_, v)| {
                parse_kv_u64(v, "buffered").or_else(|| parse_kv_u64(v, "buffered_frames"))
            })
        });
    let stall = lookup("connectedLoopStall").is_some()
        || context.iter().any(|(_, v)| {
            v.contains("connected_loop_stall")
                || v.contains("connectedLoopStall=yes")
                || v.contains("C_loop_deferral")
        });
    let first_frame = lookup("firstFrameOnWireMs")
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .or_else(|| {
            context
                .iter()
                .find_map(|(_, v)| parse_kv_raw(v, "firstFrameOnWireMs").map(str::to_string))
        });

    let mut parts = Vec::new();
    match underruns {
        0 => {}
        1 => parts.push("~1 underrun".into()),
        n => parts.push(format!("~{n} underruns")),
    }
    if let Some(n) = buffered {
        if n >= FULL_BUFFER_MIN {
            parts.push(format!("full buffer (buffered={n})"));
        } else {
            parts.push(format!("buffered={n}"));
        }
    }
    if stall {
        parts.push("C_loop_deferral".into());
    }
    for key in [
        "handshakeDropped",
        "sendAudioError",
        "encodeError",
        "sidecarFfmpegExit",
        "sidecarSsrfReject",
        "sidecarMoqError",
    ] {
        if lookup(key).is_some() {
            parts.push(key.to_string());
        }
    }

    if parts.is_empty() && first_frame.is_none() {
        return None;
    }
    let mut line = parts.join(", ");
    if let Some(ms) = first_frame {
        if !line.is_empty() {
            line.push_str("; ");
        }
        line.push_str("firstFrameOnWireMs=");
        line.push_str(&ms);
    }
    Some(line)
}

fn underrun_count(context: &[(String, String)]) -> usize {
    let from_values = context
        .iter()
        .map(|(_, v)| v.matches("frame_underrun").count() + v.matches("frameUnderrun").count())
        .max()
        .unwrap_or(0);
    if from_values > 0 {
        from_values
    } else if context.iter().any(|(k, _)| k == "frameUnderrun") {
        1
    } else {
        0
    }
}

fn parse_kv_u64(hay: &str, key: &str) -> Option<u64> {
    parse_kv_raw(hay, key)?.parse().ok()
}

fn parse_kv_raw<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let rest = hay.split(&needle).nth(1)?;
    let val = rest
        .split(|c: char| c == ';' || c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("");
    if val.is_empty() { None } else { Some(val) }
}

fn row(key: &str, value: &str) -> String {
    format!("| {key} | {value} |\n")
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn inline_code(s: &str) -> String {
    // Break any run of backticks so the value cannot close the span.
    let escaped = sanitize_text(s)
        .replace('`', "'")
        .replace(['\n', '\r'], " ");
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
/// Newlines are kept so log tails stay one event per line.
fn sanitize_text(s: &str) -> String {
    s.replace('\r', "")
        .split('\n')
        .map(sanitize_line)
        .collect::<Vec<_>>()
        .join("\n")
        .replace('\0', "")
}

fn sanitize_line(s: &str) -> String {
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
    out.join(" ")
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

fn truncate_at_word(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max).collect();
    let min_keep = (max / 2).max(1);
    let mut last_break: Option<usize> = None;
    let mut chars = 0usize;
    for (i, ch) in taken.char_indices() {
        if chars >= min_keep && is_title_break(ch) {
            last_break = Some(i);
        }
        chars += 1;
    }
    if let Some(i) = last_break {
        taken[..i]
            .trim_end()
            .trim_end_matches(['—', '-', '·', ',', ';', '/', ':'])
            .trim_end()
            .to_string()
    } else {
        taken
    }
}

fn is_title_break(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '—' | '-' | '·' | '/' | ',' | ';' | ':')
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

    /// Marks shaped like live Contabo issue #40 (space-joined tails, as
    /// the previous sanitiser used to dump them).
    fn issue40_marks() -> Vec<(String, String)> {
        let voice_tail = [
            "frame_underrun regime=midsong frame=21188 lateness_ms=127 buffered=249",
            "frame_underrun regime=midsong frame=21196 lateness_ms=78 buffered=249",
            "connected_loop_stall arm=audio elapsed_ms=43 audio_msg=frame",
            "connected_loop_stall arm=audio elapsed_ms=81 audio_msg=frame",
            "connected_loop_stall arm=audio elapsed_ms=413 audio_msg=frame",
            "frame_underrun regime=midsong frame=21215 lateness_ms=390 buffered=249",
            "frame_underrun regime=midsong frame=21241 lateness_ms=20 buffered=249",
            "frame_underrun regime=midsong frame=21243 lateness_ms=43 buffered=249",
            "frame_underrun regime=midsong frame=21248 lateness_ms=18 buffered=249",
            "frame_underrun regime=midsong frame=21250 lateness_ms=79 buffered=249",
            "frame_underrun regime=midsong frame=21255 lateness_ms=115 buffered=249",
            "frame_underrun regime=midsong frame=21266 lateness_ms=24 buffered=249",
            "connected_loop_stall arm=event elapsed_ms=51 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=78 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=67 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=53 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=54 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=87 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=24 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=18 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=10 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=30 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=13 item=other chat_lines=0",
            "connected_loop_stall arm=event elapsed_ms=70 item=other chat_lines=0",
        ];
        let latency = [
            "frame_underrun elapsed_ms=18 retry=0",
            "frame_underrun elapsed_ms=79 retry=0",
            "frame_underrun elapsed_ms=115 retry=0",
            "frame_underrun elapsed_ms=24 retry=0",
            "connected_loop_stall elapsed_ms=51 retry=0",
            "connected_loop_stall elapsed_ms=78 retry=0",
            "connected_loop_stall elapsed_ms=67 retry=0",
            "connected_loop_stall elapsed_ms=53 retry=0",
            "connected_loop_stall elapsed_ms=54 retry=0",
            "connected_loop_stall elapsed_ms=87 retry=0",
            "connected_loop_stall elapsed_ms=24 retry=0",
            "connected_loop_stall elapsed_ms=18 retry=0",
            "connected_loop_stall elapsed_ms=10 retry=0",
            "connected_loop_stall elapsed_ms=30 retry=0",
            "connected_loop_stall elapsed_ms=13 retry=0",
            "connected_loop_stall elapsed_ms=70 retry=0",
        ];
        let log_tail = [
            "music_bot_latency stage=frame_underrun elapsed_ms=79 regime=midsong frame_index=21250 buffered_frames=249 frame delivered late",
            "music_bot_latency stage=frame_underrun elapsed_ms=115 regime=midsong frame_index=21255 buffered_frames=249 frame delivered late",
            "music_bot_latency stage=frame_underrun elapsed_ms=24 regime=midsong frame_index=21266 buffered_frames=249 frame delivered late",
            "music_bot_latency invoker=Vasquez93 command=Pause chat command received",
            "music_bot_latency stage=connected_loop_stall elapsed_ms=51 arm=event detail=item=other chat_lines=0 connected-loop arm body outran the 20 ms audio-frame cadence",
            "music_bot_latency stage=connected_loop_stall elapsed_ms=78 arm=event detail=item=other chat_lines=0 connected-loop arm body outran the 20 ms audio-frame cadence",
            "music_bot_latency stage=connected_loop_stall elapsed_ms=67 arm=event detail=item=other chat_lines=0 connected-loop arm body outran the 20 ms audio-frame cadence",
            "music_bot_latency stage=connected_loop_stall elapsed_ms=70 arm=event detail=item=other chat_lines=0 connected-loop arm body outran the 20 ms audio-frame cadence",
        ];
        vec![
            (
                "connectedLoopStall".into(),
                "arm=event elapsed_ms=70 item=other chat_lines=0".into(),
            ),
            ("firstFrameOnWireMs".into(), "21794".into()),
            (
                "frameUnderrun".into(),
                "regime=midsong frame=21266 lateness_ms=24 buffered=249".into(),
            ),
            ("logTail".into(), log_tail.join(" ")),
            ("musicBotLatency".into(), latency.join(" ")),
            ("sidecarHealth".into(), "ok sources=0 sessions=0".into()),
            ("voiceLogTail".into(), voice_tail.join(" ")),
            (
                "voiceState".into(),
                "firstFrameOnWireMs=21794; connectedLoopStall=yes; frameUnderrun=yes".into(),
            ),
        ]
    }

    #[test]
    fn title_includes_path_release_and_note() {
        let t = build_title(&sample_report());
        assert_eq!(
            t,
            "[bug-report] /music-bots/42 · v1.6.9 — optional operator text"
        );
    }

    #[test]
    fn title_without_note_or_marks_is_path_and_release() {
        let mut report = sample_report();
        report.note = None;
        report.release = None;
        report.context.clear();
        assert_eq!(build_title(&report), "[bug-report] /music-bots/42");
    }

    #[test]
    fn title_does_not_truncate_mid_word() {
        let mut report = sample_report();
        report.page_path = "/music-bots/5".into();
        report.release = Some("v1.6.12".into());
        report.note = Some(
            "v1.6.12 Contabo hardstyle stutter capture — C_loop_deferral / full-buffer underruns — please keep"
                .into(),
        );
        let t = build_title(&report);
        assert_eq!(
            t,
            "[bug-report] /music-bots/5 · v1.6.12 — Contabo hardstyle stutter capture — C_loop_deferral / full-buffer underruns — please keep"
        );
        assert!(!t.ends_with("captur"), "old 40-char cap cut mid-word");
        assert!(
            !t.contains("v1.6.12 Contabo"),
            "release must not be duplicated in the symptom"
        );
    }

    #[test]
    fn title_uses_rollup_when_note_absent() {
        let mut report = sample_report();
        report.note = None;
        report.release = Some("v1.6.12".into());
        report.page_path = "/music-bots/5".into();
        report.context = issue40_marks();
        let t = build_title(&report);
        assert!(t.starts_with("[bug-report] /music-bots/5 · v1.6.12 — "));
        assert!(t.contains("~9 underruns"));
        assert!(t.contains("C_loop_deferral"));
        assert!(t.chars().count() <= TITLE_MAX);
    }

    #[test]
    fn title_word_boundary_at_max() {
        let mut report = sample_report();
        report.page_path = "/x".into();
        report.release = None;
        report.note = Some(format!("{}endword leftover", "word ".repeat(80)));
        let t = build_title(&report);
        assert!(t.chars().count() <= TITLE_MAX);
        assert!(
            !t.ends_with("wor") && !t.ends_with("endwor"),
            "must not end mid-word, got {t:?}"
        );
    }

    #[test]
    fn rollup_from_sample_marks() {
        let text = build_rollup(&issue40_marks()).expect("rollup");
        assert_eq!(
            text,
            "~9 underruns, full buffer (buffered=249), C_loop_deferral; firstFrameOnWireMs=21794"
        );
    }

    #[test]
    fn rollup_counts_lone_frame_underrun_mark() {
        let ctx = vec![(
            "frameUnderrun".into(),
            "regime=startup frame=1 lateness_ms=4 buffered=12".into(),
        )];
        let text = build_rollup(&ctx).expect("rollup");
        assert!(text.contains("~1 underrun"));
        assert!(text.contains("buffered=12"));
        assert!(!text.contains("full buffer"));
    }

    #[test]
    fn tail_collapse_resplits_joined_events() {
        let joined = (0..16)
            .map(|i| format!("frame_underrun elapsed_ms={i} retry=0"))
            .collect::<Vec<_>>()
            .join(" ");
        let (caption, body) = collapse_tail(&joined);
        assert!(body.contains('\n'), "events become separate lines");
        assert!(
            !body.contains("frame_underrun elapsed_ms=0 retry=0 frame_underrun"),
            "must not dump a mid-line wall"
        );
        assert!(
            body.lines().count() <= TAIL_DISPLAY_LINES + 1,
            "omitted marker + at most {TAIL_DISPLAY_LINES} events"
        );
        assert!(caption.unwrap().contains("last"));
        assert!(body.contains("… "));
        assert!(body.contains("omitted"));
    }

    #[test]
    fn tail_collapse_keeps_recent_high_signal() {
        let mut lines: Vec<String> = (0..20).map(|i| format!("info noise line {i}")).collect();
        lines.push("frame_underrun regime=midsong buffered=249".into());
        let (_, body) = collapse_tail(&lines.join("\n"));
        assert!(body.contains("frame_underrun"));
        assert!(body.contains("… "));
        assert!(
            !body.contains("info noise line 0"),
            "oldest noise should drop"
        );
    }

    #[test]
    fn tail_line_cap_is_word_boundary() {
        let long = format!(
            "frame_underrun {}",
            "buffered_frames=249 late wire-send stall ".repeat(8)
        );
        let (_, body) = collapse_tail(&long);
        assert!(body.chars().count() <= TAIL_LINE_MAX);
        assert!(
            !body.ends_with("sta") && !body.ends_with("wir"),
            "must not cut mid-word, got {body:?}"
        );
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
        assert!(
            !draft.body.contains("### Summary"),
            "benign latency line is not a mark rollup"
        );
    }

    #[test]
    fn body_summary_sits_above_note_and_raw_context() {
        let mut report = sample_report();
        report.page_path = "/music-bots/5".into();
        report.release = Some("v1.6.12".into());
        report.note = Some(
            "v1.6.12 Contabo hardstyle stutter capture — C_loop_deferral / full-buffer underruns — please keep"
                .into(),
        );
        report.context = issue40_marks();
        let draft = build_issue(&report, &reporter(), Utc::now());
        let summary = draft.body.find("### Summary").expect("summary");
        let note = draft.body.find("### Note").expect("note");
        let ctx = draft.body.find("### Context").expect("context");
        assert!(summary < note, "rollup must sit above the operator note");
        assert!(note < ctx, "note must sit above the raw context dump");
        assert!(draft.body[summary..note].contains(
            "~9 underruns, full buffer (buffered=249), C_loop_deferral; firstFrameOnWireMs=21794"
        ));
        assert!(draft.body.contains("**frameUnderrun**"));
        assert!(draft.body.contains("buffered=249"));
        assert!(draft.body.contains("**voiceLogTail**"));
        assert!(
            draft.body.contains("last ") && draft.body.contains(" of "),
            "long tails should caption how many lines were kept"
        );
        // Forensics: raw stall / underrun lines still present below Summary.
        assert!(draft.body[ctx..].contains("connected_loop_stall"));
        assert!(draft.body[ctx..].contains("frame_underrun"));
    }

    #[test]
    fn note_preserves_multiline_structure() {
        let mut report = sample_report();
        report.note = Some("line one\nline two".into());
        let draft = build_issue(&report, &reporter(), Utc::now());
        assert!(draft.body.contains("line one\nline two"));
    }

    #[test]
    fn note_cannot_close_fence_or_inject_heading() {
        let mut report = sample_report();
        report.note = Some("```\n# injected\n```".into());
        let draft = build_issue(&report, &reporter(), Utc::now());
        // Leftover backticks must sit inside a longer fence.
        assert!(draft.body.contains("````"));
        let note_at = draft.body.find("### Note").expect("note");
        let after_note = &draft.body[note_at..];
        let open = after_note.find("````\n").expect("opening fence");
        let inner = &after_note[open + 5..];
        let close = inner.find("\n````").expect("closing fence");
        let fenced = &inner[..close];
        assert!(
            fenced.contains("# injected"),
            "payload stays inside the fence"
        );
        let after_fence = &inner[close..];
        if let Some(next) = after_fence.find("\n### ") {
            assert!(
                !after_fence[..next].contains("# injected"),
                "heading must not leak out of the fence"
            );
        }
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
